use super::model_policy::configure;
use super::*;
use serde_json::{json, Value};

const COMPACT_PROMPT: &str = "Your task is to create a detailed summary of the conversation so far, paying close attention to the user's explicit requests and your previous actions.\nReturn the summary in <summary> tags.";
const CLIENT_SUMMARY: &str =
    "<summary>Manual summary completed: cobalt-731, mango-482, spruce-956.</summary>";

fn summary_response(request: &wiremock::Request) -> ResponseTemplate {
    let payload: Value = serde_json::from_slice(&request.body).unwrap();
    let current = payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
        .as_str()
        .unwrap();
    let content = if current.contains("durable conversation checkpoint") {
        "<summary>Preserve governing constraints, complete tool contracts, cobalt-731, mango-482 and spruce-956.</summary>"
    } else {
        CLIENT_SUMMARY
    };
    ResponseTemplate::new(200)
        .insert_header("content-type", "application/vnd.amazon.eventstream")
        .set_body_bytes(generation_body(content))
}

#[tokio::test]
async fn partitioned_compaction_reconciles_missing_fact_claims_and_accounts_for_merge_failure() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    const FACTS: &str = "project=cobalt-731; recovery_tag=mango-482; checksum=spruce-956";
    const LOCAL_ABSENCE: &str = "UNKNOWN_VALUES_CLAIM: project, recovery_tag and checksum were not provided in this part. Request them again.";
    for (stream, native_summary, fail_merge) in [
        (false, true, false),
        (true, true, false),
        (false, false, false),
        (true, false, true),
    ] {
        let mock = MockServer::start().await;
        mount_context_alignment_models(&mock).await;
        let merges = Arc::new(AtomicUsize::new(0));
        let merge_count = Arc::clone(&merges);
        Mock::given(method("POST"))
            .and(path("/generateAssistantResponse"))
            .respond_with(move |request: &wiremock::Request| {
                let payload: Value = serde_json::from_slice(&request.body).unwrap();
                let state = &payload["conversationState"];
                let current = state["currentMessage"]["userInputMessage"]["content"]
                    .as_str()
                    .unwrap();
                let summary = if state["conversationId"].as_str().unwrap().ends_with("-merge") {
                    merge_count.fetch_add(1, Ordering::SeqCst);
                    assert!(current.contains(FACTS));
                    assert!(current.contains(LOCAL_ABSENCE));
                    assert!(current.contains("cannot negate an explicit value in another part"));
                    assert!(state["history"].as_array().is_none_or(Vec::is_empty));
                    if fail_merge {
                        "".to_owned()
                    } else {
                        format!("<summary>{FACTS}</summary>")
                    }
                } else if current.contains("durable conversation checkpoint") {
                    if state["history"].to_string().contains("cobalt-731") {
                        format!("<summary>{FACTS}</summary>")
                    } else {
                        format!("<summary>{LOCAL_ABSENCE}</summary>")
                    }
                } else {
                    let history = state["history"].to_string();
                    assert!(history.contains("cobalt-731"));
                    assert!(history.contains("mango-482"));
                    assert!(history.contains("spruce-956"));
                    if native_summary {
                        assert!(history.contains(LOCAL_ABSENCE));
                        assert!(history.contains("Source part summaries in chronological order"));
                        assert!(history.contains("cannot negate an explicit value in another part"));
                        assert!(history.contains("Only an explicit later correction"));
                    } else {
                        assert!(!history.contains("UNKNOWN_VALUES_CLAIM"));
                    }
                    CLIENT_SUMMARY.to_owned()
                };
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/vnd.amazon.eventstream")
                    .set_body_bytes(generation_body(&summary))
            })
            .mount(&mock)
            .await;
        let port = unused_tcp_port();
        let daemon =
            Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
        import_context_alignment_account(&daemon, 0.0).await;
        configure(&daemon, |config| {
            // Consolidation must wait for parts to release their account slots.
            config.pool.max_concurrent_per_account = 1;
            config.context.auto_compact_on_overflow = false;
        })
        .await;
        let records = "gateway synthetic record\n".repeat(32_000);
        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("x-api-key", daemon.api_key.as_deref().unwrap())
            .header("user-agent", "claude-cli/2.1.296 (external, sdk-cli)")
            .json(&json!({
                "model":"mapped-small", "max_tokens":1024, "stream":stream,
                "context_management":{"edits":[{"type":"compact_20260112","trigger":{"type":"input_tokens","value":50000}}]},
                "messages":[
                    {"role":"user","content":format!("{FACTS}\n{records}")},
                    {"role":"assistant","content":"Acknowledged."},
                    {"role":"user","content":records},
                    {"role":"assistant","content":"Acknowledged."},
                    {"role":"user","content":if native_summary { COMPACT_PROMPT } else { "Continue with the established durable facts." }}
                ]
            }))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        assert_eq!(status, reqwest::StatusCode::OK, "{body}");
        assert!(body.contains(CLIENT_SUMMARY), "{body}");
        assert_eq!(merges.load(Ordering::SeqCst), usize::from(!native_summary));
        let stats = expect_ok(
            daemon
                .call("stats", json!({"detail":true,"recent":20}))
                .await,
        );
        let logs = stats["stats"]["recent_requests"].as_array().unwrap();
        assert!(
            logs.iter().any(|log| {
                log["path"] == "/internal/compact"
                    && log["status"] == if fail_merge { 502 } else { 200 }
            }),
            "consolidation must settle its own usage and failure: {stats}"
        );
        let requests = mock.received_requests().await.unwrap();
        let tokenizer = crosslane_translate::TokenCountCache::new(16).unwrap();
        for request in requests
            .iter()
            .filter(|r| r.url.path() == "/generateAssistantResponse")
        {
            let payload: crosslane_translate::KiroPayload =
                serde_json::from_slice(&request.body).unwrap();
            assert!(tokenizer.estimate_kiro_payload(&payload).await.unwrap() <= 126_720);
        }
        daemon.stop().await;
    }
}

#[tokio::test]
async fn manual_compact_after_an_oversized_tool_result_reaches_the_summary_model() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(summary_response)
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    let tool_result = format!(
        "EARLY=cobalt-731\n{}\nMIDDLE=mango-482\n{}\nLATE=spruce-956",
        "tool output record\n".repeat(25_000),
        "tool output record\n".repeat(25_000),
    );
    let client = reqwest::Client::new();
    let tokenizer = crosslane_translate::TokenCountCache::new(16).unwrap();
    let mut seen_payloads = 0;
    for (stream, merged) in [(false, false), (false, true), (true, false), (true, true)] {
        let mut request = json!({
            "model":"mapped-small", "max_tokens":1024, "stream":stream,
            "system":"You are a helpful AI assistant tasked with summarizing conversations.",
            "tools":[{"name":"Read","input_schema":{"type":"object"}}],
            "messages":[
                {"role":"user","content":"Read the records"},
                {"role":"assistant","content":[{"type":"tool_use","id":"read-1","name":"Read","input":{}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"read-1","content":tool_result}]}
            ]
        });
        if merged {
            request["messages"][2]["content"]
                .as_array_mut()
                .unwrap()
                .push(json!({"type":"text","text":COMPACT_PROMPT}));
        } else {
            request["messages"]
                .as_array_mut()
                .unwrap()
                .push(json!({"role":"user","content":COMPACT_PROMPT}));
        }
        if !stream && !merged {
            let count = client
                .post(format!("http://127.0.0.1:{port}/v1/messages/count_tokens"))
                .header("x-api-key", daemon.api_key.as_deref().unwrap())
                .header("user-agent", "claude-cli/2.1.260 (external, e2e)")
                .json(&request)
                .send()
                .await
                .unwrap();
            assert_eq!(count.status(), reqwest::StatusCode::OK);
            let count: Value = count.json().await.unwrap();
            assert!(
                count["input_tokens"].as_u64().unwrap() > 128_000,
                "count_tokens must count the complete source without compacting it"
            );
        }
        let response = client
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("x-api-key", daemon.api_key.as_deref().unwrap())
            .header("user-agent", "claude-cli/2.1.260 (external, e2e)")
            .json(&request)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        assert_eq!(
            status,
            reqwest::StatusCode::OK,
            "stream={stream}, merged={merged}: {body}"
        );
        if stream {
            assert!(body.contains("\"type\":\"compaction\""), "{body}");
            assert!(body.contains(CLIENT_SUMMARY), "{body}");
            assert!(
                body.find("\"type\":\"compaction\"").unwrap() < body.find(CLIENT_SUMMARY).unwrap()
            );
            assert!(body.contains("\"type\":\"message_stop\""), "{body}");
        } else {
            let body: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["content"][0]["type"], "compaction");
            assert_eq!(body["content"][1]["text"], CLIENT_SUMMARY);
        }

        let requests = mock.received_requests().await.unwrap();
        let payloads: Vec<Value> = requests
            .iter()
            .filter_map(|request| serde_json::from_slice(&request.body).ok())
            .filter(|payload: &Value| payload.get("conversationState").is_some())
            .collect();
        let new_payloads = &payloads[seen_payloads..];
        assert!(
            new_payloads.len() >= 3,
            "partitioned summaries plus the client's summary request"
        );
        let mut parts: Vec<_> = new_payloads[..new_payloads.len() - 1]
            .iter()
            .filter(|payload| {
                !payload["conversationState"]["conversationId"]
                    .as_str()
                    .unwrap()
                    .ends_with("-merge")
            })
            .collect();
        parts.sort_by_key(|payload| {
            payload["conversationState"]["conversationId"]
                .as_str()
                .unwrap()
                .rsplit_once("-part-")
                .unwrap()
                .1
                .parse::<usize>()
                .unwrap()
        });
        let mut transcript = String::new();
        for part in parts {
            let payload: crosslane_translate::KiroPayload =
                serde_json::from_value(part.clone()).unwrap();
            assert!(tokenizer.estimate_kiro_payload(&payload).await.unwrap() <= 95_040);
            for message in &payload.conversation_state.history {
                if let Some(user) = &message.user_input_message {
                    transcript.push_str(&user.content);
                }
            }
        }
        assert!(
            transcript.contains(&tool_result),
            "partitioning lost source text"
        );
        let current = &new_payloads.last().unwrap()["conversationState"]["currentMessage"]
            ["userInputMessage"];
        assert_eq!(current["content"], COMPACT_PROMPT);
        assert!(current["userInputMessageContext"]["toolResults"]
            .as_array()
            .is_none_or(Vec::is_empty));
        seen_payloads = payloads.len();
    }
    daemon.stop().await;
}

#[tokio::test]
async fn client_summary_input_threshold_rejection_partitions_complete_source_and_retries() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let tool_result = format!(
        "EARLY=cobalt-731\n{}\nMIDDLE=mango-482\n{}\nLATE=spruce-956",
        "tool output record\n".repeat(85_000),
        "tool output record\n".repeat(85_000),
    );
    let prompt =
        format!("CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\n\n{COMPACT_PROMPT}");
    for (stream, automatic) in [(false, false), (true, true)] {
        let request = json!({
            "model":"source-large", "max_tokens":1024, "stream":stream,
            "system":"Preserve all governing requirements and exact identifiers.",
            "tools":[{"name":"Read", "input_schema":{"type":"object",
                "properties":{"path":{"type":"string"}}, "required":["path"],
                "additionalProperties":false}}],
            "messages":[
                {"role":"user","content":"Read the complete records"},
                {"role":"assistant","content":[{"type":"tool_use","id":"read-1","name":"Read","input":{"path":"approved.txt"}}]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"read-1","content":tool_result},
                    {"type":"text","text":prompt}
                ]}
            ]
        });
        assert!(serde_json::to_vec(&request).unwrap().len() > 3 * 1024 * 1024);
        let payloads = assert_client_summary_threshold_recovery(request, automatic).await;
        let tokenizer = crosslane_translate::TokenCountCache::new(16).unwrap();
        let initial = &payloads[0];
        let initial_tokens = tokenizer.estimate_kiro_payload(initial).await.unwrap();
        assert!(
            initial_tokens > 200_000 && initial_tokens < 950_000,
            "the published 1M window must admit the initial request"
        );
        assert!(serde_json::to_vec(initial).unwrap().len() < 8 * 1024 * 1024);
        let mut parts: Vec<_> = payloads[1..payloads.len() - 1]
            .iter()
            .filter(|payload| {
                !payload
                    .conversation_state
                    .conversation_id
                    .ends_with("-merge")
            })
            .collect();
        assert!(parts.len() > 1, "recovery must use partitioned summaries");
        parts.sort_by_key(|payload| {
            payload
                .conversation_state
                .conversation_id
                .rsplit_once("-part-")
                .unwrap()
                .1
                .parse::<usize>()
                .unwrap()
        });
        let mut transcript = String::new();
        let mut source_tool_call_found = false;
        for part in parts {
            assert!(tokenizer.estimate_kiro_payload(part).await.unwrap() <= 148_500);
            for message in &part.conversation_state.history {
                if let Some(user) = &message.user_input_message {
                    transcript.push_str(&user.content);
                }
                if let Some(assistant) = &message.assistant_response_message {
                    source_tool_call_found |= assistant.content.contains("approved.txt");
                }
            }
        }
        assert!(
            transcript.contains(&tool_result),
            "recovery lost source text"
        );
        assert!(transcript.contains("Preserve all governing requirements and exact identifiers."));
        assert!(transcript.contains("additionalProperties"));
        assert!(
            source_tool_call_found,
            "recovery lost the original tool call"
        );
        let retried = payloads.last().unwrap();
        assert!(tokenizer.estimate_kiro_payload(retried).await.unwrap() <= 190_000);
        assert_eq!(
            retried
                .conversation_state
                .current_message
                .user_input_message
                .content,
            prompt,
            "the client summary instruction must survive recovery"
        );
    }
}

async fn assert_client_summary_threshold_recovery(
    request: Value,
    automatic: bool,
) -> Vec<crosslane_translate::KiroPayload> {
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    let main_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&main_calls);
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(move |request: &wiremock::Request| {
            let payload: Value = serde_json::from_slice(&request.body).unwrap();
            let current = payload["conversationState"]["currentMessage"]["userInputMessage"]
                ["content"]
                .as_str()
                .unwrap();
            if !current.contains("durable conversation checkpoint")
                && calls.fetch_add(1, Ordering::SeqCst) == 0
            {
                return ResponseTemplate::new(400).set_body_json(json!({
                    "message":"Input content length exceeds threshold.",
                    "reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD"
                }));
            }
            summary_response(request)
        })
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    let config_path = daemon.home().join("config.toml");
    let mut config: Config =
        toml::from_str(&tokio::fs::read_to_string(&config_path).await.unwrap()).unwrap();
    config.context.auto_compact_on_overflow = automatic;
    config.pool.max_concurrent_per_account = 1;
    tokio::fs::write(&config_path, toml::to_string(&config).unwrap())
        .await
        .unwrap();
    expect_ok(daemon.call("config.reload", json!({})).await);
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("x-api-key", daemon.api_key.as_deref().unwrap())
        .header(
            "user-agent",
            "claude-cli/2.1.283 (external, claude-vscode, agent-sdk/0.3.283)",
        )
        .json(&request)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, reqwest::StatusCode::OK, "{body}");
    if request["stream"] == true {
        assert!(body.contains("\"type\":\"compaction\""), "{body}");
        assert!(body.contains(CLIENT_SUMMARY), "{body}");
        assert!(body.contains("\"type\":\"message_stop\""), "{body}");
        assert!(!body.contains("\"type\":\"error\""), "{body}");
    } else {
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["content"][0]["type"], "compaction");
        assert_eq!(body["content"][1]["text"], CLIENT_SUMMARY);
    }
    assert_eq!(main_calls.load(Ordering::SeqCst), 2, "one recovery retry");
    let payloads = mock
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/generateAssistantResponse")
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    daemon.stop().await;
    payloads
}

#[tokio::test]
async fn manual_compact_partitions_oversized_system_and_tool_definitions() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(summary_response)
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    let config_path = daemon.home().join("config.toml");
    let mut config: crosslane_core::config::Config =
        toml::from_str(&tokio::fs::read_to_string(&config_path).await.unwrap()).unwrap();
    config.context.auto_compact_on_overflow = false;
    tokio::fs::write(&config_path, toml::to_string(&config).unwrap())
        .await
        .unwrap();
    expect_ok(daemon.call("config.reload", json!({})).await);

    let system = format!(
        "POLICY_EARLY=cobalt-731\n{}\nPOLICY_LATE=spruce-956",
        "governing requirement record\n".repeat(25_000)
    );
    let docs: Vec<Value> = (0..2)
        .map(|i| {
            json!({
                "name":format!("Read{i}"), "description":"detail record\n".repeat(14_000),
                "input_schema":{"type":"object","properties":{"path":{"type":"string"}},
                    "required":["path"],"additionalProperties":false},
                "input_examples":[{"path":"approved.txt"}]
            })
        })
        .collect();
    let values: Vec<String> = (0..12_000).map(|i| format!("mode_{i:06}")).collect();
    let schemas: Vec<Value> = (0..2).map(|i| json!({
        "name":format!("Mode{i}"), "description":"Select an exact allowed mode.",
        "input_schema":{"type":"object","properties":{"mode":{"type":"string","enum":values}},
            "required":["mode"],"additionalProperties":false}
    })).collect();
    let client = reqwest::Client::new();
    let tokenizer = crosslane_translate::TokenCountCache::new(32).unwrap();
    let mut seen_payloads = 0;
    for (label, system, tools) in [
        ("system", system.clone(), vec![]),
        ("tool_docs", "Keep requirements intact.".into(), docs),
        ("tool_schemas", "Keep requirements intact.".into(), schemas),
    ] {
        for stream in [false, true] {
            let request = json!({
                "model":"resolved-small", "max_tokens":1024, "stream":stream,
                "system":system, "tools":tools,
                "messages":[{"role":"user","content":"Earlier work"},
                    {"role":"assistant","content":"Preserve all requirements"},
                    {"role":"user","content":COMPACT_PROMPT}]
            });
            let parsed: crosslane_translate::ClaudeRequest =
                serde_json::from_value(request.clone()).unwrap();
            crosslane_translate::validate_claude(&parsed).unwrap();
            let source = crosslane_translate::claude_to_kiro(
                &parsed,
                &crosslane_translate::TranslationOptions::new("resolved-small", "AI_EDITOR"),
            );
            assert!(
                tokenizer
                    .context_token_breakdown(&source)
                    .await
                    .unwrap()
                    .minimum_input_tokens
                    > 63_360,
                "{label} must reproduce an intrinsically oversized request"
            );
            let expected_sources: Vec<String> =
                std::iter::once(parsed.system.clone().unwrap().to_string())
                    .chain(parsed.tools.iter().map(|tool| json!(tool).to_string()))
                    .collect();

            if !stream {
                let counted = client
                    .post(format!("http://127.0.0.1:{port}/v1/messages/count_tokens"))
                    .header("x-api-key", daemon.api_key.as_deref().unwrap())
                    .header("user-agent", "claude-cli/2.1.260 (external, e2e)")
                    .json(&request)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(counted.status(), reqwest::StatusCode::OK);
                let counted: Value = counted.json().await.unwrap();
                assert!(
                    counted["input_tokens"].as_u64().unwrap() > 63_360,
                    "{label}: counting must include the full source, not a summary"
                );
                let dispatched = mock
                    .received_requests()
                    .await
                    .unwrap()
                    .iter()
                    .filter(|request| request.url.path() == "/generateAssistantResponse")
                    .count();
                assert_eq!(
                    dispatched, seen_payloads,
                    "counting must not invoke a model"
                );
            }

            let response = client
                .post(format!("http://127.0.0.1:{port}/v1/messages"))
                .header("x-api-key", daemon.api_key.as_deref().unwrap())
                .header("user-agent", "claude-cli/2.1.260 (external, e2e)")
                .json(&request)
                .send()
                .await
                .unwrap();
            let status = response.status();
            let body = response.text().await.unwrap();
            assert_eq!(
                status,
                reqwest::StatusCode::OK,
                "{label} stream={stream}: {body}"
            );
            if stream {
                assert!(
                    body.contains(CLIENT_SUMMARY) && body.contains("\"type\":\"message_stop\""),
                    "{body}"
                );
            } else {
                let body: Value = serde_json::from_str(&body).unwrap();
                assert_eq!(body["content"][1]["text"], CLIENT_SUMMARY);
            }
            let requests = mock.received_requests().await.unwrap();
            let payloads: Vec<Value> = requests
                .iter()
                .filter_map(|request| serde_json::from_slice(&request.body).ok())
                .filter(|payload: &Value| payload.get("conversationState").is_some())
                .collect();
            let new_payloads = &payloads[seen_payloads..];
            assert!(
                new_payloads.len() >= 3,
                "{label}: partitioned source plus the final client summary"
            );
            let mut parts: Vec<_> = new_payloads[..new_payloads.len() - 1]
                .iter()
                .filter(|payload| {
                    !payload["conversationState"]["conversationId"]
                        .as_str()
                        .unwrap()
                        .ends_with("-merge")
                })
                .collect();
            parts.sort_by_key(|payload| {
                payload["conversationState"]["conversationId"]
                    .as_str()
                    .unwrap()
                    .rsplit_once("-part-")
                    .unwrap()
                    .1
                    .parse::<usize>()
                    .unwrap()
            });
            let mut transcript = String::new();
            for part in parts {
                let payload: crosslane_translate::KiroPayload =
                    serde_json::from_value(part.clone()).unwrap();
                crosslane_translate::validate_kiro_tool_history(&payload).unwrap();
                assert!(tokenizer.estimate_kiro_payload(&payload).await.unwrap() <= 47_520);
                assert!(payload
                    .conversation_state
                    .current_message
                    .user_input_message
                    .user_input_message_context
                    .is_none());
                for message in payload.conversation_state.history {
                    if let Some(user) = message.user_input_message {
                        transcript.push_str(&user.content);
                    }
                }
            }
            for source in expected_sources {
                assert!(
                    transcript.contains(&source),
                    "{label}: system/tool source was truncated"
                );
            }
            let main: crosslane_translate::KiroPayload =
                serde_json::from_value(new_payloads.last().unwrap().clone()).unwrap();
            assert!(main
                .conversation_state
                .current_message
                .user_input_message
                .user_input_message_context
                .is_none());
            assert_eq!(
                main.conversation_state
                    .current_message
                    .user_input_message
                    .content,
                COMPACT_PROMPT
            );
            assert!(tokenizer.estimate_kiro_payload(&main).await.unwrap() < 63_360);
            seen_payloads = payloads.len();
        }
        let response = client
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("x-api-key", daemon.api_key.as_deref().unwrap())
            .header("user-agent", "claude-cli/2.1.260 (external, e2e)")
            .json(
                &json!({"model":"resolved-small","max_tokens":64,"system":system,"tools":tools,
                "messages":[{"role":"user","content":"Continue development"}]}),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::BAD_REQUEST,
            "ordinary {label}"
        );
        let body: Value = response.json().await.unwrap();
        let component = if label == "tool_schemas" {
            "tool_definition_tokens"
        } else {
            "protected_prefix_tokens"
        };
        assert!(
            body["error"]["context"][component].as_u64().unwrap() > 64_000,
            "ordinary {label} must retain its full contract: {body}"
        );
    }

    // Normal generation cannot silently summarize away its governing system.
    // Both the direct guard and automatic compaction's minimum guard report
    // a numerical breakdown, without invoking a model or echoing source text.
    for auto in [false, true] {
        config.context.auto_compact_on_overflow = auto;
        tokio::fs::write(&config_path, toml::to_string(&config).unwrap())
            .await
            .unwrap();
        expect_ok(daemon.call("config.reload", json!({})).await);
        let response = client
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("x-api-key", daemon.api_key.as_deref().unwrap())
            .header("user-agent", "claude-cli/2.1.260 (external, e2e)")
            .json(&json!({"model":"resolved-small", "max_tokens":64,
                "system":system,"messages":[{"role":"user","content":"Continue development"}]}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
        let body: Value = response.json().await.unwrap();
        let context = &body["error"]["context"];
        assert!(context["protected_prefix_tokens"].as_u64().unwrap() > 64_000);
        assert!(
            context["minimum_input_tokens"].as_u64().unwrap()
                > context["maximum_input_tokens"].as_u64().unwrap()
        );
        assert_eq!(context["tool_definition_tokens"], 0);
        assert!(!body.to_string().contains("POLICY_EARLY"));
        let stats = expect_ok(
            daemon
                .call("stats", json!({"detail":true,"recent":20}))
                .await,
        );
        let rejected = stats["stats"]["recent_requests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|request| request["trace_id"] == body["request_id"])
            .unwrap();
        assert_eq!(rejected["diagnostics"]["context_overflow"], *context);
        assert_eq!(rejected["diagnostics"]["account_error"], false);
    }
    let dispatched = mock
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/generateAssistantResponse")
        .count();
    assert_eq!(
        dispatched, seen_payloads,
        "ordinary oversized system requests must fail locally"
    );
    daemon.stop().await;
}
