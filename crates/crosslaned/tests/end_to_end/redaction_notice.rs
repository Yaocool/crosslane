//! Synthetic credentials, a real daemon and a local Kiro wire mock.

use std::sync::Mutex;

use serde_json::{json, Value};

use super::*;

const SECRET: &str = "notice-fixture-opaque-token";
const ANSWER: &str = "原回答 CONTROL_OK 😀";
const NOTICE: &str = "⚠️ Crosslane 安全提示：";

#[derive(Clone)]
enum Reply {
    Text(String),
    Tool { name: String, input: Value },
    Error,
}

#[derive(Clone)]
struct NoticeUpstream {
    requests: Arc<Mutex<Vec<Value>>>,
    reply: Reply,
}

impl wiremock::Respond for NoticeUpstream {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let payload: Value = serde_json::from_slice(&request.body).unwrap();
        let user = &payload["conversationState"]["currentMessage"]["userInputMessage"];
        let content = user["content"].as_str().unwrap();
        let has_result = user["userInputMessageContext"]["toolResults"]
            .as_array()
            .is_some_and(|results| !results.is_empty());
        let mut body = Vec::new();
        match &self.reply {
            Reply::Tool { name, input } if !has_result => {
                body.extend(event_stream_frame(
                    "assistantResponseEvent",
                    json!({"content":"调用工具"}),
                ));
                let mut input = input.clone();
                if input["token"] == "echo_token" {
                    let token = content
                        .split("token=")
                        .nth(1)
                        .unwrap()
                        .split_whitespace()
                        .next()
                        .unwrap();
                    input["token"] = json!(token);
                }
                let input = input.to_string();
                let middle = input.len() / 2;
                body.extend(event_stream_frame("toolUseEvent", json!({"toolUseId":"notice-tool","name":name,"input":&input[..middle],"stop":false})));
                body.extend(event_stream_frame(
                    "toolUseEvent",
                    json!({"toolUseId":"notice-tool","input":&input[middle..],"stop":true}),
                ));
            }
            Reply::Error => {
                body.extend(event_stream_frame(
                    "assistantResponseEvent",
                    json!({"content":"partial answer"}),
                ));
                body.extend(event_stream_frame(
                    "invalidStateEvent",
                    json!({"reason":"SERVICE_UNAVAILABLE","message":"local synthetic failure"}),
                ));
            }
            reply => {
                let text = match reply {
                    Reply::Text(text) => text.as_str(),
                    _ => ANSWER,
                };
                body.extend(event_stream_frame(
                    "assistantResponseEvent",
                    json!({"content":text}),
                ));
            }
        }
        body.extend(event_stream_frame(
            "messageMetadataEvent",
            json!({"usage":{"inputTokens":100,"outputTokens":3,"creditsConsumed":0.1}}),
        ));
        self.requests.lock().unwrap().push(payload);
        ResponseTemplate::new(200)
            .insert_header("content-type", "application/vnd.amazon.eventstream")
            .set_body_bytes(body)
    }
}

async fn configure(daemon: &Daemon, notice: bool, redaction: bool) {
    super::model_policy::configure(daemon, |config| {
        config.features.enable_request_redaction_notice = notice;
        config.features.enable_request_redaction = redaction;
        config.features.enhance_system_prompt = false;
        config.features.enable_model_fallback = false;
        config.upstream.max_retries = 0;
    })
    .await;
}

fn request(stream: bool) -> Value {
    json!({"model":"source-large","max_tokens":256,"stream":stream,"messages":[{"role":"user","content":format!("token={SECRET}\nReturn an answer.")}]})
}

async fn call(daemon: &Daemon, port: u16, request: &Value) -> reqwest::Response {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .bearer_auth(daemon.api_key.as_deref().unwrap())
        .header("user-agent", "claude-cli/2.1.309 (external, cli)")
        .json(request)
        .send()
        .await
        .unwrap()
}

fn events(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect()
}

fn assert_answer(body: &str, stream: bool, notice: bool, answer: &str) {
    let (text, usage, stop) = if stream {
        let values = events(body);
        let starts = values
            .iter()
            .filter(|event| event["type"] == "content_block_start")
            .collect::<Vec<_>>();
        assert_eq!(starts.len(), 1, "notice must reuse the original text block");
        assert_eq!(starts[0]["index"], 0);
        let text = values
            .iter()
            .filter(|event| event["delta"]["type"] == "text_delta")
            .map(|event| {
                assert_eq!(event["index"], 0);
                event["delta"]["text"].as_str().unwrap()
            })
            .collect::<String>();
        let stop_index = values
            .iter()
            .position(|event| event["type"] == "content_block_stop")
            .unwrap();
        let last_text = values
            .iter()
            .rposition(|event| event["delta"]["type"] == "text_delta")
            .unwrap();
        assert!(last_text < stop_index);
        assert_eq!(values.last().unwrap()["type"], "message_stop");
        let terminal = values
            .iter()
            .find(|event| event["type"] == "message_delta")
            .unwrap();
        (
            text,
            terminal["usage"].clone(),
            terminal["delta"]["stop_reason"].clone(),
        )
    } else {
        let response: Value = serde_json::from_str(body).unwrap();
        assert_eq!(response["content"].as_array().unwrap().len(), 1);
        (
            response["content"][0]["text"].as_str().unwrap().to_owned(),
            response["usage"].clone(),
            response["stop_reason"].clone(),
        )
    };
    assert_eq!(stop, "end_turn");
    assert_eq!(usage["output_tokens"], 3);
    assert!(text.starts_with(answer), "original answer must be retained");
    assert_eq!(text.matches(NOTICE).count(), usize::from(notice));
    if notice {
        assert!(text.contains("\n\n---\n\n> **⚠️ Crosslane 安全提示："));
        assert!(text.contains("\n>\n> **请注意凭证的使用方式：**"));
        assert!(!text.contains('\u{1b}'));
    } else {
        assert_eq!(text, answer);
    }
    assert!(!text.contains(SECRET));
    assert!(!text.contains("__CROSSLANE_SECRET_"));
}

#[tokio::test]
async fn notice_reload_preserves_answers_and_reuses_the_original_text_block() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(NoticeUpstream {
            requests: Arc::clone(&requests),
            reply: Reply::Text(ANSWER.into()),
        })
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    for notice in [false, true, false] {
        configure(&daemon, notice, true).await;
        for stream in [false, true] {
            let response = call(&daemon, port, &request(stream)).await;
            assert_eq!(response.status(), 200);
            assert_answer(
                &response.text().await.unwrap(),
                stream,
                notice && cfg!(feature = "crs"),
                ANSWER,
            );
            let wire = requests.lock().unwrap().last().unwrap().to_string();
            assert!(!wire.contains(SECRET));
            assert!(wire.contains("__CROSSLANE_SECRET_"));
        }
    }
    daemon.stop().await;
}

#[tokio::test]
async fn image_requests_with_public_documentation_only_warn_for_actual_credentials() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(NoticeUpstream {
            requests: Arc::clone(&requests),
            reply: Reply::Text(ANSWER.into()),
        })
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon = Daemon::start_http_with_log_level(
        port,
        &format!("{}/generateAssistantResponse", mock.uri()),
        "info",
    )
    .await;
    import_context_alignment_account(&daemon, 0.0).await;
    configure(&daemon, true, true).await;
    let image = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a04cAAAAASUVORK5CYII=";
    let actual_secret = "ksk_notice_actual_secret_2026";
    let documentation = concat!(
        "const DIMENSIONS = [{key: 'bugs'}, {key: 'perf'}]; perform a performance review\n",
        "Use Bearer authentication. Bearer authorization: see the guide.\n",
        "interface Options { apiKey: string; password: string; }\n",
        "api_key=YOUR_OPENAI_API_KEY\nsecret={{API_SECRET}}\n",
        "curl --key ./certs/client.key https://example.invalid/\n",
        "next_page_token=cursor-page-2\nshortcut_key=Ctrl+K\n",
        "Response: {\"pdf_key\":\"PDF存储Key\",\"output_file_key\":\"objects/report.pdf\"}"
    );
    for has_secret in [false, true] {
        for stream in [false, true] {
            let body = json!({
                "model":"source-large", "max_tokens":256, "stream":stream,
                "system":format!("Example: const apiKey = \"sk-proj-xxxxx\";\n{documentation}"),
                "tools":[{
                    "name":"Workflow",
                    "description":documentation,
                    "input_schema":{"type":"object","properties":{
                        "key":{"type":"string","enum":["bugs","perf"]},
                        "api_key":{"type":"string","default":"sk-proj-xxxxx"},
                        "hotKey":{"type":"string","enum":["Enter","Escape"]},
                        "nextPageToken":{"type":"string","default":"cursor-page-2"},
                        "signature":{"type":"string","default":"fn inspect(input: &str) -> bool"},
                        "private_key":{"type":"string","default":"./fixtures/id_ed25519"},
                        "pdf_key":{"type":"string","default":"PDF存储Key"},
                        "documentKey":{"type":"string","default":"objects/report.pdf"}
                    }}
                }],
                "messages":[
                    {"role":"user","content":"Read the documentation."},
                    {"role":"assistant","content":[{"type":"tool_use","id":"read-docs","name":"Workflow","input":{"key":"perf"}}]},
                    {"role":"user","content":[{"type":"tool_result","tool_use_id":"read-docs","content":if has_secret {format!("{documentation}\napi_key={actual_secret}")} else {documentation.into()}}]},
                    {"role":"assistant","content":"Read the documentation."},
                    {"role":"user","content":[
                    {"type":"text","text":"Describe the image."},
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data":image}}
                ]}]
            });
            let response = call(&daemon, port, &body).await;
            assert_eq!(response.status(), 200);
            assert_answer(
                &response.text().await.unwrap(),
                stream,
                has_secret && cfg!(feature = "crs"),
                ANSWER,
            );
            let wire = requests.lock().unwrap().last().unwrap().clone();
            let current = &wire["conversationState"]["currentMessage"]["userInputMessage"];
            assert_eq!(current["images"][0]["source"]["bytes"], image);
            let specification =
                &current["userInputMessageContext"]["tools"][0]["toolSpecification"];
            assert_eq!(specification["description"], documentation);
            let key_schema = &current["userInputMessageContext"]["tools"][0]["toolSpecification"]
                ["inputSchema"]["json"]["properties"]["key"];
            let variants = key_schema.get("enum").or_else(|| {
                key_schema["anyOf"]
                    .as_array()?
                    .iter()
                    .find_map(|branch| branch.get("enum"))
            });
            assert_eq!(variants, Some(&json!(["bugs", "perf"])));
            let text = wire.to_string();
            assert!(text.contains("perform a performance review"));
            assert!(text.contains("sk-proj-xxxxx"));
            assert!(text.contains("PDF存储Key"));
            assert!(!text.contains(actual_secret));
            assert_eq!(text.contains("__CROSSLANE_SECRET_"), has_secret);
        }
    }
    let mut records = Vec::new();
    for _ in 0..60 {
        records.clear();
        let mut files = tokio::fs::read_dir(daemon.home().join("logs"))
            .await
            .unwrap();
        while let Some(file) = files.next_entry().await.unwrap() {
            if file.file_type().await.unwrap().is_file() {
                let content = tokio::fs::read_to_string(file.path()).await.unwrap();
                records.extend(
                    content
                        .lines()
                        .filter_map(|line| serde_json::from_str::<Value>(line).ok()),
                );
            }
        }
        if records
            .iter()
            .filter(|row| row["fields"]["event"] == "upstream.response.accepted")
            .count()
            >= 4
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for event in ["proxy.upstream.redaction", "upstream.response.accepted"] {
        let entries = records
            .iter()
            .filter(|row| row["fields"]["event"] == event)
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 4);
        assert_eq!(
            entries
                .iter()
                .filter(|row| row["fields"]["redaction_triggered"] == true)
                .count(),
            2
        );
        for row in entries {
            let fields = &row["fields"];
            assert_eq!(fields["redaction_enabled"], true);
            let sources: Value =
                serde_json::from_str(fields["redaction_sources"].as_str().unwrap()).unwrap();
            if fields["redaction_triggered"] == true {
                assert_eq!(fields["redacted_values"], 1);
                assert!(sources
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|source| source["source"] == "tool_result"
                        && source["path"].as_str().unwrap().contains("/history/")));
            } else {
                assert_eq!(sources, json!([]));
            }
            if event == "upstream.response.accepted" {
                assert!(fields["trace_id"].as_str().unwrap().starts_with("trace_"));
            }
        }
    }
    let logs = serde_json::to_string(&records).unwrap();
    assert!(!logs.contains(actual_secret));
    assert!(!logs.contains("__CROSSLANE_SECRET_"));
    daemon.stop().await;
}

#[tokio::test]
async fn notice_skips_disabled_redaction_clean_requests_structured_output_and_truncation() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(NoticeUpstream {
            requests: Arc::default(),
            reply: Reply::Text(ANSWER.into()),
        })
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    for stream in [false, true] {
        configure(&daemon, true, false).await;
        assert_answer(
            &call(&daemon, port, &request(stream))
                .await
                .text()
                .await
                .unwrap(),
            stream,
            false,
            ANSWER,
        );
        configure(&daemon, true, true).await;
        for content in ["hello", "token=$CLOUD_TOKEN"] {
            let mut plain = request(stream);
            plain["messages"][0]["content"] = json!(content);
            assert_answer(
                &call(&daemon, port, &plain).await.text().await.unwrap(),
                stream,
                false,
                ANSWER,
            );
        }
        let mut summary = request(stream);
        summary["messages"] = json!([
            {"role":"user","content":format!("token={SECRET}\nKeep the work state.")},
            {"role":"assistant","content":"Acknowledged."},
            {"role":"user","content":"Your task is to create a detailed summary of the conversation so far, preserving requirements. Return <summary>...</summary>."}
        ]);
        assert_answer(
            &call(&daemon, port, &summary).await.text().await.unwrap(),
            stream,
            false,
            ANSWER,
        );
        for extra in [
            json!({"output_config":{"format":{"type":"json_schema","schema":{"type":"object"}}}}),
            json!({"output_format":{"type":"json_schema","schema":{"type":"object"}}}),
            json!({"response_format":{"type":"json_object"}}),
        ] {
            let mut structured = request(stream);
            structured
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert_answer(
                &call(&daemon, port, &structured).await.text().await.unwrap(),
                stream,
                false,
                ANSWER,
            );
        }
        let mut truncated = request(stream);
        truncated["max_tokens"] = json!(1);
        let body = call(&daemon, port, &truncated).await.text().await.unwrap();
        assert!(!body.contains(NOTICE));
        assert!(body.contains("max_tokens"));
    }
    daemon.stop().await;
}

#[tokio::test]
async fn notice_preserves_tool_parameters_and_only_appears_after_the_tool_result() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(NoticeUpstream {
            requests: Arc::default(),
            reply: Reply::Tool {
                name: "request".into(),
                input: json!({"token":"echo_token","count":3,"literal":"$CLOUD_TOKEN"}),
            },
        })
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    configure(&daemon, true, true).await;
    for (stream, buffered) in [(false, true), (true, true), (true, false)] {
        super::model_policy::configure(&daemon, |config| {
            config.features.buffer_tool_calls = buffered;
        })
        .await;
        let mut req = request(stream);
        req["tools"] = json!([{"name":"request","description":"use token","input_schema":{"type":"object","properties":{"token":{"type":"string"},"count":{"type":"integer"},"literal":{"type":"string"}}}}]);
        let body = call(&daemon, port, &req).await.text().await.unwrap();
        assert!(!body.contains(NOTICE));
        let input = if stream {
            let values = events(&body);
            let start = values
                .iter()
                .find(|event| event["content_block"]["type"] == "tool_use")
                .unwrap();
            assert_eq!(start["content_block"]["id"], "notice-tool");
            assert_eq!(start["content_block"]["name"], "request");
            assert!(values
                .iter()
                .any(|event| event["delta"]["stop_reason"] == "tool_use"));
            let input = values
                .iter()
                .filter_map(|event| event["delta"]["partial_json"].as_str())
                .collect::<String>();
            serde_json::from_str::<Value>(&input).unwrap()
        } else {
            let response: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(response["stop_reason"], "tool_use");
            response["content"]
                .as_array()
                .unwrap()
                .iter()
                .find(|block| block["type"] == "tool_use")
                .unwrap()["input"]
                .clone()
        };
        assert_eq!(
            input,
            json!({"token":SECRET,"count":3,"literal":"$CLOUD_TOKEN"})
        );
        req["messages"].as_array_mut().unwrap().extend([
            json!({"role":"assistant","content":[{"type":"text","text":"调用工具"},{"type":"tool_use","id":"notice-tool","name":"request","input":input}]}),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"notice-tool","content":"ok"}]})
        ]);
        let body = call(&daemon, port, &req).await.text().await.unwrap();
        assert_answer(&body, stream, cfg!(feature = "crs"), ANSWER);
    }
    daemon.stop().await;
}

#[tokio::test]
async fn notice_never_changes_json_answers_or_partial_failures() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    for reply in [
        Reply::Text("{\"ok\":true}".into()),
        Reply::Text("[1,2]".into()),
        Reply::Error,
    ] {
        let mock = MockServer::start().await;
        mount_context_alignment_models(&mock).await;
        Mock::given(method("POST"))
            .and(path("/generateAssistantResponse"))
            .respond_with(NoticeUpstream {
                requests: Arc::default(),
                reply: reply.clone(),
            })
            .mount(&mock)
            .await;
        let port = unused_tcp_port();
        let daemon =
            Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
        import_context_alignment_account(&daemon, 0.0).await;
        configure(&daemon, true, true).await;
        for stream in [false, true] {
            let body = call(&daemon, port, &request(stream))
                .await
                .text()
                .await
                .unwrap();
            assert!(!body.contains(NOTICE));
            if let Reply::Text(answer) = &reply {
                assert_answer(&body, stream, false, answer);
            } else {
                assert!(body.contains("error"));
            }
        }
        daemon.stop().await;
    }
}

#[cfg(feature = "crs")]
#[tokio::test]
#[ignore = "requires CROSSLANE_CRS_SOURCE and CROSSLANE_CLAUDE_CODE_BIN; only local mock requests"]
async fn real_claude_code_retains_answer_notice_and_read_tool_through_unmodified_crs() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let claude = std::env::var("CROSSLANE_CLAUDE_CODE_BIN").unwrap();
    let isolated = tempfile::tempdir().unwrap();
    let file = isolated.path().join("fixture.txt");
    std::fs::write(&file, "LOCAL_READ_TOOL_RESULT\n").unwrap();
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(NoticeUpstream {
            requests: Arc::clone(&requests),
            reply: Reply::Tool {
                name: "Read".into(),
                input: json!({"file_path":file}),
            },
        })
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    configure(&daemon, true, true).await;
    let (mut relay, address) = super::crs_errors::start_crs_console(&daemon, port).await;
    let mut client = super::crs_errors::claude_code_command(&claude, isolated.path(), &address);
    client.args([
        "--no-session-persistence",
        "--strict-mcp-config",
        "--mcp-config",
        "{\"mcpServers\":{}}",
        "--max-turns",
        "3",
        "--model",
        "source-large",
        "--tools",
        "Read",
        "--allowedTools=Read",
    ]);
    client.arg(format!(
        "token={SECRET}\nRead {} and return the final answer.",
        file.display()
    ));
    let output = tokio::time::timeout(Duration::from_secs(45), client.output())
        .await
        .expect("Claude Code completed")
        .unwrap();
    let result: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "client JSON: {error}; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert!(output.status.success(), "{result}");
    assert_eq!(result["is_error"], false, "{result}");
    let answer = result["result"].as_str().unwrap();
    assert!(answer.starts_with(ANSWER), "{answer}");
    assert_eq!(answer.matches(NOTICE).count(), 1);
    assert!(!answer.contains(SECRET));
    let captured = requests.lock().unwrap().clone();
    assert_eq!(captured.len(), 2, "one tool turn and one final answer");
    assert!(
        captured[1].to_string().contains("LOCAL_READ_TOOL_RESULT"),
        "the real Read tool must execute successfully"
    );
    for wire in captured {
        assert!(!wire.to_string().contains(SECRET));
    }
    relay.kill().await.unwrap();
    daemon.stop().await;
}
