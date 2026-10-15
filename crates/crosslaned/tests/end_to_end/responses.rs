use super::*;
use serde_json::{json, Value};

const CODEX_AGENT: &str = "codex_cli_rs/0.147.0 (Mac OS 26.0; arm64) Terminal/1.0";

#[derive(Clone)]
struct ToolRoundtrip {
    assistant_content: &'static str,
}

impl wiremock::Respond for ToolRoundtrip {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let payload: Value = serde_json::from_slice(&request.body).unwrap();
        let context = &payload["conversationState"]["currentMessage"]["userInputMessage"]
            ["userInputMessageContext"];
        let results = context["toolResults"].as_array().map_or(0, Vec::len);
        let body = if results > 0 {
            generation_body("Read the file and applied the patch.")
        } else {
            let names = context["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool["toolSpecification"]["name"].as_str())
                .collect::<Vec<_>>();
            assert_eq!(names.len(), 2);
            let mut body = Vec::new();
            if !self.assistant_content.is_empty() {
                body.extend(event_stream_frame(
                    "assistantResponseEvent",
                    json!({"content":self.assistant_content}),
                ));
            }
            body.extend(event_stream_frame(
                "toolUseEvent",
                json!({"toolUseId":"call_read","name":names[0],"input":"{\"path\":","stop":false}),
            ));
            body.extend(event_stream_frame("toolUseEvent", json!({"toolUseId":"call_patch","name":names[1],"input":"{\"input\":","stop":false})));
            // Kiro continuation fragments can omit the name supplied at the
            // start of the call, including when parallel calls are interleaved.
            body.extend(event_stream_frame(
                "toolUseEvent",
                json!({"toolUseId":"call_read","input":"\"README.md\"}","stop":true}),
            ));
            body.extend(event_stream_frame("toolUseEvent", json!({"toolUseId":"call_patch","inputDelta":"\"*** Begin Patch\\n*** End Patch\"}","stop":true})));
            body.extend(event_stream_frame("messageMetadataEvent", json!({"messageMetadataEvent":{"usage":{"inputTokens":100,"outputTokens":20,"creditsConsumed":0.25}}})));
            body
        };
        ResponseTemplate::new(200)
            .insert_header("content-type", "application/vnd.amazon.eventstream")
            .set_body_bytes(body)
    }
}

#[derive(Clone)]
struct EmptyToolResultRetry {
    tool_result_calls: Arc<AtomicUsize>,
    recover: bool,
}

impl wiremock::Respond for EmptyToolResultRetry {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let payload: Value = serde_json::from_slice(&request.body).unwrap();
        let context = &payload["conversationState"]["currentMessage"]["userInputMessage"]
            ["userInputMessageContext"];
        let has_results = context["toolResults"]
            .as_array()
            .is_some_and(|results| !results.is_empty());
        let body = if !has_results {
            let name = context["tools"][0]["toolSpecification"]["name"]
                .as_str()
                .unwrap();
            let mut body = event_stream_frame(
                "toolUseEvent",
                json!({
                    "toolUseId":"call_read", "name":name,
                    "input":"{\"path\":\"README.md\"}", "stop":true
                }),
            );
            body.extend(event_stream_frame(
                "messageMetadataEvent",
                json!({"messageMetadataEvent":{"usage":{
                    "inputTokens":100,"outputTokens":10,"creditsConsumed":0.1
                }}}),
            ));
            body
        } else {
            let attempt = self.tool_result_calls.fetch_add(1, Ordering::SeqCst);
            if !self.recover || attempt.is_multiple_of(2) {
                // A transport-successful but semantically empty continuation
                // must be retried rather than completed as an empty message.
                let mut body = event_stream_frame(
                    "reasoningContentEvent",
                    json!({"reasoningContentEvent":{
                        "text":"discarded reasoning from an empty tool-result turn"
                    }}),
                );
                body.extend(event_stream_frame(
                    "messageMetadataEvent",
                    json!({"messageMetadataEvent":{"usage":{
                        "inputTokens":100,"outputTokens":1,"creditsConsumed":0.01
                    }}}),
                ));
                body
            } else {
                generation_body("Recovered after the empty tool-result continuation.")
            }
        };
        ResponseTemplate::new(200)
            .insert_header("content-type", "application/vnd.amazon.eventstream")
            .set_body_bytes(body)
    }
}

async fn create_response_with_session(
    client: &reqwest::Client,
    daemon: &Daemon,
    port: u16,
    path: &str,
    session_id: Option<&str>,
    request: &Value,
) -> Value {
    let mut builder = client
        .post(format!("http://127.0.0.1:{port}{path}"))
        .bearer_auth(daemon.api_key.as_deref().unwrap())
        .header("user-agent", CODEX_AGENT);
    if let Some(session_id) = session_id {
        builder = builder.header("session-id", session_id);
    }
    let response = builder.json(request).send().await.unwrap();
    let status = response.status();
    assert!(response.headers().contains_key("request-id"));
    let content_type = response
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let body = response.text().await.unwrap();
    assert_eq!(status, reqwest::StatusCode::OK, "{body}");
    if request["stream"] == true {
        assert!(content_type.starts_with("text/event-stream"));
        assert!(!body.contains("chat.completion.chunk"));
        assert!(!body.contains("[DONE]"));
        let events = body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event["sequence_number"], index);
        }
        assert_eq!(events[0]["type"], "response.created");
        let completed = events.last().unwrap();
        assert_eq!(completed["type"], "response.completed", "{body}");
        assert_eq!(events[0]["response"]["id"], completed["response"]["id"]);
        for (index, item) in completed["response"]["output"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            let done = events
                .iter()
                .find(|event| {
                    event["type"] == "response.output_item.done" && event["output_index"] == index
                })
                .unwrap();
            assert_eq!(done["item"], *item);
            for event in events.iter().filter(|event| event["output_index"] == index) {
                if event["item"]["name"].is_string() {
                    assert_eq!(event["item"]["name"], item["name"]);
                    assert_eq!(event["item"]["namespace"], item["namespace"]);
                }
            }
        }
        completed["response"].clone()
    } else {
        assert!(content_type.starts_with("application/json"));
        serde_json::from_str(&body).unwrap()
    }
}

async fn create_response(
    client: &reqwest::Client,
    daemon: &Daemon,
    port: u16,
    path: &str,
    request: &Value,
) -> Value {
    create_response_with_session(
        client,
        daemon,
        port,
        path,
        Some("codex-response-e2e"),
        request,
    )
    .await
}

#[tokio::test]
async fn responses_tool_roundtrip_works_through_both_aliases_and_buffer_modes() {
    assert_tool_roundtrip("Inspecting the file. ").await;
}

#[tokio::test]
async fn responses_tool_only_roundtrip_does_not_invent_assistant_text() {
    assert_tool_roundtrip("").await;
}

#[tokio::test]
async fn responses_long_namespace_names_restore_and_resume_in_both_modes() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(ToolRoundtrip {
            assistant_content: "",
        })
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    let client = reqwest::Client::new();
    for buffered in [false, true] {
        model_policy::configure(&daemon, |config| {
            config.features.buffer_tool_calls = buffered
        })
        .await;
        for stream in [false, true] {
            for length in [64, 128] {
                for additional_tools in [false, true] {
                    let namespace = "n".repeat(length);
                    let function_name = "f".repeat(length);
                    let custom_name = "c".repeat(length);
                    let declaration = json!({"type":"namespace","name":namespace,"tools":[
                        {"type":"function","name":function_name,
                            "parameters":{"type":"object","properties":{"path":{"type":"string"}}}},
                        {"type":"custom","name":custom_name,"format":{"type":"text"}}
                    ]});
                    let mut initial = json!({
                        "model":"source-large","input":[{"role":"user","content":"Use both tools."}],
                        "stream":stream,"max_output_tokens":512,
                        "tool_choice":{"type":"allowed_tools","mode":"required","tools":[
                            {"type":"function","namespace":namespace,"name":function_name},
                            {"type":"custom","namespace":namespace,"name":custom_name}
                        ]}
                    });
                    if additional_tools {
                        initial["input"].as_array_mut().unwrap().insert(
                            0,
                            json!({"type":"additional_tools","tools":[declaration.clone()]}),
                        );
                    } else {
                        initial["tools"] = json!([declaration.clone()]);
                    }
                    let first =
                        create_response(&client, &daemon, port, "/v1/responses", &initial).await;
                    let output = first["output"].as_array().unwrap();
                    assert_eq!(output.len(), 2);
                    let function = output
                        .iter()
                        .find(|item| item["type"] == "function_call")
                        .unwrap();
                    let custom = output
                        .iter()
                        .find(|item| item["type"] == "custom_tool_call")
                        .unwrap();
                    assert_eq!(function["name"], function_name);
                    assert_eq!(function["namespace"], namespace);
                    assert_eq!(custom["name"], custom_name);
                    assert_eq!(custom["namespace"], namespace);
                    assert_eq!(first["tools"], json!([declaration.clone()]));
                    let continuation = json!({
                        "model":"source-large","previous_response_id":first["id"],"stream":stream,
                        "input":[
                            {"type":"function_call_output","call_id":function["call_id"],"output":"README contents"},
                            {"type":"custom_tool_call_output","call_id":custom["call_id"],"output":"Patch applied."}
                        ]
                    });
                    let second =
                        create_response(&client, &daemon, port, "/v1/responses", &continuation)
                            .await;
                    assert_eq!(second["previous_response_id"], first["id"]);
                    assert_eq!(second["tools"], json!([declaration]));
                    assert_eq!(
                        second["output"][0]["content"][0]["text"],
                        "Read the file and applied the patch."
                    );
                    let payloads = mock
                        .received_requests()
                        .await
                        .unwrap()
                        .iter()
                        .filter(|request| request.url.path() == "/generateAssistantResponse")
                        .rev()
                        .take(2)
                        .map(|request| serde_json::from_slice::<Value>(&request.body).unwrap())
                        .collect::<Vec<_>>();
                    let expected = [function_name, custom_name]
                        .map(|name| crosslane_translate::tool_name(&format!("{namespace}.{name}")));
                    for payload in &payloads {
                        let names = payload["conversationState"]["currentMessage"]
                            ["userInputMessage"]["userInputMessageContext"]["tools"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|tool| tool["toolSpecification"]["name"].as_str().unwrap())
                            .collect::<Vec<_>>();
                        assert_eq!(names, expected);
                        assert!(names.iter().all(|name| name.len() <= 64));
                    }
                    let history_names = payloads[0]["conversationState"]["history"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(|message| {
                            message["assistantResponseMessage"]["toolUses"].as_array()
                        })
                        .flatten()
                        .map(|tool| tool["name"].as_str().unwrap())
                        .collect::<Vec<_>>();
                    assert_eq!(history_names.len(), expected.len());
                    assert!(expected
                        .iter()
                        .all(|name| history_names.contains(&name.as_str())));
                }
            }
        }
    }
    daemon.stop().await;
}

async fn assert_tool_roundtrip(assistant_content: &'static str) {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(ToolRoundtrip { assistant_content })
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    let client = reqwest::Client::new();

    // Authentication and client filtering must run before spending upstream credits.
    for (agent, key, expected) in [
        (
            "claude-cli/2.1.235 (external, test)",
            daemon.api_key.as_deref().unwrap(),
            reqwest::StatusCode::BAD_REQUEST,
        ),
        (
            CODEX_AGENT,
            "invalid-test-key",
            reqwest::StatusCode::UNAUTHORIZED,
        ),
    ] {
        let response = client
            .post(format!("http://127.0.0.1:{port}/v1/responses"))
            .header("user-agent", agent)
            .bearer_auth(key)
            .json(&json!({"model":"source-large","input":"hello"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }

    for buffered in [false, true] {
        let config_path = daemon.home().join("config.toml");
        let mut config: crosslane_core::config::Config =
            toml::from_str(&tokio::fs::read_to_string(&config_path).await.unwrap()).unwrap();
        config.features.buffer_tool_calls = buffered;
        config.features.auto_continue_rounds = 0;
        tokio::fs::write(&config_path, toml::to_string_pretty(&config).unwrap())
            .await
            .unwrap();
        assert_eq!(
            expect_ok(daemon.call("config.reload", json!({})).await)["applied"],
            true
        );
        for stream in [false, true] {
            for endpoint in ["/v1/responses", "/responses"] {
                let initial_input = json!({"role":"user","content":[{"type":"input_text","text":"Inspect the file."}]});
                let additional_tools = json!({"type":"additional_tools","role":"developer","tools":[
                    {"type":"namespace","name":"functions","tools":[
                        {"type":"function","name":"read_file","parameters":{"type":"object","properties":{"path":{"type":"string"}}}},
                        {"type":"custom","name":"apply_patch","format":{"type":"text"}}
                    ]}
                ]});
                let mut request = json!({
                    "model":"source-large","instructions":"Respect the repository rules.",
                    "input":[additional_tools.clone(),initial_input.clone()],
                    "stream":stream,"store":false,"max_output_tokens":512
                });
                let first = create_response(&client, &daemon, port, endpoint, &request).await;
                assert_eq!(first["object"], "response");
                assert_eq!(first["status"], "completed");
                assert_eq!(first["model"], "source-large");
                assert_eq!(first["usage"]["input_tokens"], 100);
                assert_eq!(first["usage"]["output_tokens"], 20);
                assert!(first["id"].as_str().unwrap().starts_with("resp_"));
                assert_eq!(first["tools"].as_array().map(Vec::len), Some(1));
                let output = first["output"].as_array().unwrap();
                let message = output.iter().find(|item| item["type"] == "message");
                if assistant_content.is_empty() {
                    assert!(message.is_none(), "{first}");
                } else {
                    assert_eq!(message.unwrap()["content"][0]["text"], assistant_content);
                }
                let function = output
                    .iter()
                    .find(|item| item["type"] == "function_call")
                    .unwrap();
                assert_eq!(function["call_id"], "call_read");
                assert_eq!(function["name"], "read_file");
                assert_eq!(function["namespace"], "functions");
                assert_eq!(
                    serde_json::from_str::<Value>(function["arguments"].as_str().unwrap()).unwrap(),
                    json!({"path":"README.md"})
                );
                let custom = output
                    .iter()
                    .find(|item| item["type"] == "custom_tool_call")
                    .unwrap_or_else(|| panic!("missing custom tool: buffered={buffered}, stream={stream}, endpoint={endpoint}, response={first}"));
                assert_eq!(custom["name"], "apply_patch");
                assert_eq!(custom["namespace"], "functions");
                assert_eq!(custom["input"], "*** Begin Patch\n*** End Patch");

                let mut replay = vec![additional_tools, initial_input];
                replay.extend(output.iter().cloned());
                replay.push(json!({"type":"function_call_output","call_id":"call_read","output":"README contents"}));
                replay.push(json!({"type":"custom_tool_call_output","call_id":"call_patch","output":[{"type":"input_text","text":"Patch applied."}]}));
                request["input"] = json!(replay);
                let second = create_response(&client, &daemon, port, endpoint, &request).await;
                assert_eq!(
                    second["output"][0]["content"][0]["text"],
                    "Read the file and applied the patch."
                );
                assert_ne!(first["id"], second["id"]);
            }
        }
    }

    let received = mock.received_requests().await.unwrap();
    let payloads = received
        .iter()
        .filter(|request| request.url.path() == "/generateAssistantResponse")
        .map(|request| serde_json::from_slice::<Value>(&request.body).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(payloads.len(), 16);
    for pair in payloads.chunks_exact(2) {
        assert_eq!(pair[0]["inferenceConfig"]["maxTokens"], 512);
        assert_eq!(
            pair[1]["conversationState"]["conversationId"],
            pair[0]["conversationState"]["conversationId"]
        );
        let assistants = pair[1]["conversationState"]["history"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item.get("assistantResponseMessage"))
            .filter(|message| message["toolUses"].is_array())
            .collect::<Vec<_>>();
        assert_eq!(assistants.len(), 1);
        assert_eq!(assistants[0]["content"], assistant_content);
        assert_eq!(assistants[0]["toolUses"].as_array().unwrap().len(), 2);
        let results = pair[1]["conversationState"]["currentMessage"]["userInputMessage"]
            ["userInputMessageContext"]["toolResults"]
            .as_array()
            .unwrap();
        assert_eq!(results.len(), 2);
        let read = results
            .iter()
            .find(|result| result["toolUseId"] == "call_read")
            .unwrap();
        let patch = results
            .iter()
            .find(|result| result["toolUseId"] == "call_patch")
            .unwrap();
        assert_eq!(read["content"][0]["text"], "README contents");
        assert_eq!(patch["content"][0]["text"], "Patch applied.");
    }
    daemon.stop().await;
}

#[tokio::test]
async fn responses_codex_automation_bootstrap_survives_tool_roundtrip_and_replay() {
    const BOOTSTRAP: &str = "Automation: File maintenance\nAutomation ID: file-maintenance\nPreserve the saved task instructions.";
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(|request: &wiremock::Request| {
            let payload: Value = serde_json::from_slice(&request.body).unwrap();
            let context = payload["conversationState"]["history"][0]["userInputMessage"]["content"]
                .as_str()
                .unwrap();
            assert!(context.contains("Follow repository instructions."));
            assert_eq!(context.matches(BOOTSTRAP).count(), 1, "{context}");
            assert!(!payload.to_string().contains("fco_bootstrap"));
            let results = payload["conversationState"]["currentMessage"]["userInputMessage"]
                ["userInputMessageContext"]["toolResults"]
                .as_array();
            if let Some(results) = results.filter(|results| !results.is_empty()) {
                assert_eq!(results.len(), 2);
                for (id, text) in [
                    ("call_read", "README contents"),
                    ("call_patch", "Patch applied."),
                ] {
                    let result = results
                        .iter()
                        .find(|result| result["toolUseId"] == id)
                        .unwrap();
                    assert_eq!(result["content"][0]["text"], text);
                }
            }
            wiremock::Respond::respond(
                &ToolRoundtrip {
                    assistant_content: "Inspecting the file. ",
                },
                request,
            )
        })
        .expect(24)
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    let client = reqwest::Client::new();
    let tools = json!([
        {"type":"function","name":"read_file","parameters":{"type":"object",
            "properties":{"path":{"type":"string"}}}},
        {"type":"custom","name":"apply_patch","format":{"type":"text"}}
    ]);

    for endpoint in ["/v1/responses", "/responses"] {
        for stream in [false, true] {
            for bootstrap_index in [6, 14] {
                let mut bootstrap = json!({
                    "type":"function_call_output", "id":"fco_bootstrap",
                    "namespace":"codex_app", "name":"automation_update", "output":BOOTSTRAP
                });
                if bootstrap_index == 14 {
                    bootstrap["call_id"] = Value::Null;
                    bootstrap["output"] = json!([{"type":"input_text","text":BOOTSTRAP}]);
                }
                let mut input = vec![
                    json!({"role":"developer","content":"Follow repository instructions."});
                    bootstrap_index
                ];
                input.push(bootstrap);
                input.push(json!({"role":"user","content":"Run scheduled file maintenance."}));
                let initial = json!({
                    "model":"source-large", "input":input, "tools":tools,
                    "stream":stream, "max_output_tokens":512
                });
                let first = create_response(&client, &daemon, port, endpoint, &initial).await;
                let output = first["output"].as_array().unwrap();
                assert!(output.iter().any(|item| item["call_id"] == "call_read"));
                assert!(output.iter().any(|item| item["call_id"] == "call_patch"));
                let results = vec![
                    json!({"type":"function_call_output","call_id":"call_read","output":"README contents"}),
                    json!({"type":"custom_tool_call_output","call_id":"call_patch","output":"Patch applied."}),
                ];
                for stateful in [false, true] {
                    let mut continuation = json!({
                        "model":"source-large", "stream":stream, "store":false,
                        "max_output_tokens":512
                    });
                    if stateful {
                        continuation["previous_response_id"] = first["id"].clone();
                        continuation["input"] = json!(results);
                    } else {
                        let mut replay = input.clone();
                        replay.extend(output.iter().cloned());
                        replay.extend(results.iter().cloned());
                        continuation["input"] = json!(replay);
                        continuation["tools"] = tools.clone();
                    }
                    let second =
                        create_response(&client, &daemon, port, endpoint, &continuation).await;
                    assert_eq!(
                        second["output"][0]["content"][0]["text"],
                        "Read the file and applied the patch."
                    );
                }
            }
        }
    }

    // A malformed ordinary tool result must still fail before upstream execution.
    let response = client
        .post(format!("http://127.0.0.1:{port}/v1/responses"))
        .bearer_auth(daemon.api_key.as_deref().unwrap())
        .header("user-agent", CODEX_AGENT)
        .json(&json!({"model":"source-large","input":[{
            "type":"function_call_output", "namespace":"codex_app",
            "name":"automation_update", "call_id":123, "output":BOOTSTRAP
        }]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(response.text().await.unwrap().contains("input.0.call_id"));
    mock.verify().await;
    daemon.stop().await;
}

#[tokio::test]
async fn responses_retry_an_empty_turn_after_tool_results() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    let tool_result_calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(EmptyToolResultRetry {
            tool_result_calls: Arc::clone(&tool_result_calls),
            recover: true,
        })
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    let client = reqwest::Client::new();

    for stream in [false, true] {
        let initial = json!({
            "model":"source-large", "input":"Read the file.", "stream":stream,
            "tools":[{"type":"function","name":"read_file","parameters":{
                "type":"object","properties":{"path":{"type":"string"}}
            }}]
        });
        let session = format!("empty-tool-result-{stream}");
        let first = create_response_with_session(
            &client,
            &daemon,
            port,
            "/v1/responses",
            Some(&session),
            &initial,
        )
        .await;
        let call = first["output"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call")
            .unwrap();
        assert_eq!(call["call_id"], "call_read");

        let continuation = json!({
            "model":"source-large", "previous_response_id":first["id"],
            "input":[{"type":"function_call_output","call_id":"call_read",
                "output":"README contents"}],
            "stream":stream
        });
        let second = create_response_with_session(
            &client,
            &daemon,
            port,
            "/v1/responses",
            Some(&session),
            &continuation,
        )
        .await;
        assert_eq!(
            second["output"][0]["content"][0]["text"],
            "Recovered after the empty tool-result continuation."
        );
        assert!(!second
            .to_string()
            .contains("discarded reasoning from an empty tool-result turn"));
        assert_eq!(second["usage"]["output_tokens"], 21);
    }

    assert_eq!(tool_result_calls.load(Ordering::SeqCst), 4);
    let generate_calls = mock
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/generateAssistantResponse")
        .count();
    assert_eq!(generate_calls, 6);
    daemon.stop().await;
}

#[tokio::test]
async fn responses_fail_after_repeated_empty_tool_result_turns() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    let tool_result_calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(EmptyToolResultRetry {
            tool_result_calls: Arc::clone(&tool_result_calls),
            recover: false,
        })
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    let client = reqwest::Client::new();

    for stream in [false, true] {
        let session = format!("repeated-empty-tool-result-{stream}");
        let initial = json!({
            "model":"source-large", "input":"Read the file.", "stream":stream,
            "tools":[{"type":"function","name":"read_file"}]
        });
        let first = create_response_with_session(
            &client,
            &daemon,
            port,
            "/v1/responses",
            Some(&session),
            &initial,
        )
        .await;
        let continuation = json!({
            "model":"source-large", "previous_response_id":first["id"],
            "input":[{"type":"function_call_output","call_id":"call_read",
                "output":"README contents"}],
            "stream":stream
        });
        let response = client
            .post(format!("http://127.0.0.1:{port}/v1/responses"))
            .bearer_auth(daemon.api_key.as_deref().unwrap())
            .header("user-agent", CODEX_AGENT)
            .header("session-id", &session)
            .json(&continuation)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        if stream {
            assert_eq!(status, reqwest::StatusCode::OK, "{body}");
            assert!(body.contains("event: response.failed\n"), "{body}");
            assert!(!body.contains("event: response.completed\n"), "{body}");
            assert!(
                !body.contains("discarded reasoning from an empty tool-result turn"),
                "{body}"
            );
        } else {
            assert_eq!(status, reqwest::StatusCode::BAD_GATEWAY, "{body}");
            assert!(body.contains("no assistant output"), "{body}");
        }
    }

    assert_eq!(tool_result_calls.load(Ordering::SeqCst), 4);
    let generate_calls = mock
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/generateAssistantResponse")
        .count();
    assert_eq!(generate_calls, 6);
    daemon.stop().await;
}

#[tokio::test]
async fn responses_default_store_resumes_history_and_honors_session_forks() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(ToolRoundtrip {
            assistant_content: "Inspecting the file. ",
        })
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    let client = reqwest::Client::new();

    for (index, stream) in [false, true].into_iter().enumerate() {
        let initial = json!({
            "model":"source-large",
            "instructions":"Follow the repository rules.",
            "input":"Inspect the file.",
            "stream":stream,
            "tools":[{"type":"namespace","name":"functions","tools":[
                {"type":"function","name":"read_file","parameters":{"type":"object"}},
                {"type":"custom","name":"apply_patch"}
            ]}],
            "tool_choice":{"type":"allowed_tools","mode":"required","tools":[
                {"type":"function","name":"functions.read_file"},
                {"type":"custom","name":"functions.apply_patch"}
            ]}
        });
        let first = create_response_with_session(
            &client,
            &daemon,
            port,
            "/v1/responses",
            Some("responses-parent"),
            &initial,
        )
        .await;
        let first_id = first["id"].as_str().expect("response id").to_owned();
        assert_eq!(first["store"], true);

        // Only newly supplied items are sent. Omitted `store` defaults to true,
        // and an omitted affinity header inherits the stored conversation.
        // An explicit different session forks that conversation while reusing
        // its stored input and tool definitions.
        let continuation = json!({
            "model":"source-large",
            "previous_response_id":first_id,
            "input":[
                {"type":"function_call_output","call_id":"call_read","output":"README contents"},
                {"type":"custom_tool_call_output","call_id":"call_patch","output":"Patch applied."}
            ],
            "stream":stream
        });
        let continuation_session = (index == 1).then_some("responses-child");
        let second = create_response_with_session(
            &client,
            &daemon,
            port,
            "/v1/responses",
            continuation_session,
            &continuation,
        )
        .await;
        assert_eq!(second["previous_response_id"], first_id);
        assert_eq!(second["store"], true);
        assert_eq!(
            second["output"][0]["content"][0]["text"],
            "Read the file and applied the patch."
        );
    }

    let payloads = mock
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/generateAssistantResponse")
        .map(|request| serde_json::from_slice::<Value>(&request.body).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(payloads.len(), 4);
    for (index, pair) in payloads.chunks_exact(2).enumerate() {
        if index == 0 {
            assert_eq!(
                pair[1]["conversationState"]["conversationId"],
                pair[0]["conversationState"]["conversationId"],
                "omitted session headers must inherit the stored conversation"
            );
        } else {
            assert_ne!(
                pair[1]["conversationState"]["conversationId"],
                pair[0]["conversationState"]["conversationId"],
                "explicit different sessions must fork the stored conversation"
            );
        }
        let context = &pair[1]["conversationState"]["currentMessage"]["userInputMessage"]
            ["userInputMessageContext"];
        assert_eq!(context["tools"].as_array().unwrap().len(), 2);
        assert_eq!(context["toolResults"].as_array().unwrap().len(), 2);
    }
    daemon.stop().await;
}

#[tokio::test]
async fn responses_upstream_stream_errors_remain_failed_events() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    let mut body = event_stream_frame(
        "assistantResponseEvent",
        json!({"content":"partial answer"}),
    );
    body.extend(event_stream_frame(
        "toolUseEvent",
        json!({"toolUseId":"broken","name":"read_file","input":"{broken JSON","stop":true}),
    ));
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(body),
        )
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    let response = reqwest::Client::new().post(format!("http://127.0.0.1:{port}/v1/responses"))
        .bearer_auth(daemon.api_key.as_deref().unwrap()).header("user-agent", CODEX_AGENT)
        .json(&json!({"model":"source-large","input":"read the file","stream":true,"tools":[{"type":"function","name":"read_file","parameters":{"type":"object"}}]}))
        .send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains("event: response.failed\n"), "{body}");
    assert!(!body.contains("event: response.completed\n"), "{body}");
    assert!(!body.contains("[DONE]"), "{body}");
    daemon.stop().await;
}
