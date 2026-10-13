use super::*;
use serde_json::{json, Value};

#[tokio::test]
async fn upstream_context_error_variants_compact_without_a_client_trigger() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    for rejection in [
        "CONTEXT_LENGTH_EXCEEDED",
        "INPUT_TOO_LONG",
        "CONTENT_LENGTH_EXCEEDS_THRESHOLD",
        "Input content length exceeds threshold.",
        "The context window has been exceeded",
    ] {
        for stream in [false, true] {
            let mock = MockServer::start().await;
            mount_context_alignment_models(&mock).await;
            let responder = UpstreamOverflowCompactionResponder {
                rejection_message: Some(rejection.into()),
                ..Default::default()
            };
            Mock::given(method("POST"))
                .and(path("/generateAssistantResponse"))
                .respond_with(responder.clone())
                .mount(&mock)
                .await;
            let port = unused_tcp_port();
            let daemon =
                Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri()))
                    .await;
            import_context_alignment_account(&daemon, 0.0).await;

            // No context_management edit or /compact prompt: the local estimate
            // fits the advertised window, but the upstream rejects the input.
            let response = reqwest::Client::new()
                .post(format!("http://127.0.0.1:{port}/v1/messages"))
                .header("x-api-key", daemon.api_key.as_deref().unwrap())
                .header("user-agent", "claude-cli/2.1.260 (external, e2e)")
                .json(&json!({
                    "model":"source-large", "max_tokens":64, "stream":stream,
                    "system":"Keep this governing instruction.",
                    "messages":[
                        {"role":"user","content":"earlier context ".repeat(2_000)},
                        {"role":"assistant","content":"Keep the earlier decision"},
                        {"role":"user","content":"Continue after the upstream rejection"}
                    ]
                }))
                .send()
                .await
                .unwrap();
            let status = response.status();
            let body = response.text().await.unwrap();
            assert_eq!(
                status,
                reqwest::StatusCode::OK,
                "{rejection}, stream={stream}: {body}"
            );
            if stream {
                assert!(body.contains("\"type\":\"compaction\""), "{body}");
                assert!(
                    body.contains("main request completed after overflow retry"),
                    "{body}"
                );
                assert!(body.contains("\"type\":\"message_stop\""), "{body}");
            } else {
                let body: Value = serde_json::from_str(&body).unwrap();
                assert_eq!(body["content"][0]["type"], "compaction");
                assert_eq!(
                    body["content"][1]["text"],
                    "main request completed after overflow retry"
                );
            }
            assert_eq!(
                responder.main_calls.load(Ordering::SeqCst),
                2,
                "one recovery retry"
            );
            let payloads: Vec<Value> = mock
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|request| request.url.path() == "/generateAssistantResponse")
                .map(|request| serde_json::from_slice(&request.body).unwrap())
                .collect();
            assert_eq!(payloads.len(), 3, "initial generation, summary, retry");
            let retried = payloads.last().unwrap();
            assert_eq!(
                retried["conversationState"]["currentMessage"]["userInputMessage"]["content"],
                "Continue after the upstream rejection"
            );
            assert!(retried
                .to_string()
                .contains("Keep this governing instruction."));
            assert!(retried.to_string().len() < payloads[0].to_string().len());
            daemon.stop().await;
        }
    }
}

#[tokio::test]
async fn context_recovery_respects_disable_and_stops_after_one_retry() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    for (automatic, stream) in [(false, false), (true, false), (false, true), (true, true)] {
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
                if current.contains("durable conversation checkpoint") {
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "application/vnd.amazon.eventstream")
                        .set_body_bytes(generation_body(
                            "<summary>Preserve the earlier decision.</summary>",
                        ))
                } else {
                    calls.fetch_add(1, Ordering::SeqCst);
                    ResponseTemplate::new(400).set_body_json(json!({
                        "message":"Input content length exceeds threshold.",
                        "reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD"
                    }))
                }
            })
            .mount(&mock)
            .await;
        let port = unused_tcp_port();
        let daemon =
            Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
        import_context_alignment_account(&daemon, 0.0).await;
        if !automatic {
            let config_path = daemon.home().join("config.toml");
            let mut config: Config =
                toml::from_str(&tokio::fs::read_to_string(&config_path).await.unwrap()).unwrap();
            config.context.auto_compact_on_overflow = false;
            tokio::fs::write(&config_path, toml::to_string(&config).unwrap())
                .await
                .unwrap();
            expect_ok(daemon.call("config.reload", json!({})).await);
        }
        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("x-api-key", daemon.api_key.as_deref().unwrap())
            .header("user-agent", "claude-cli/2.1.260 (external, e2e)")
            .json(&json!({
                "model":"source-large", "max_tokens":64, "stream":stream,
                "messages":[
                    {"role":"user","content":"earlier context ".repeat(2_000)},
                    {"role":"assistant","content":"Keep the earlier decision"},
                    {"role":"user","content":"Continue"}
                ]
            }))
            .send()
            .await
            .unwrap();
        let sse_error = cfg!(feature = "crs") && stream;
        assert_eq!(
            response.status(),
            if sse_error {
                reqwest::StatusCode::OK
            } else {
                reqwest::StatusCode::BAD_REQUEST
            }
        );
        assert_eq!(
            response.headers()["x-crosslane-error-code"],
            "context_length_exceeded"
        );
        assert_eq!(response.headers()["x-crosslane-account-error"], "false");
        let trace_id = response.headers()["request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let text = response.text().await.unwrap();
        let body: Value = if sse_error {
            assert_eq!(text.matches("event: error").count(), 1);
            sse_error_payload(&text)
        } else {
            serde_json::from_str(&text).unwrap()
        };
        #[cfg(not(feature = "crs"))]
        assert_eq!(
            body["error"]["message"],
            "prompt is too long: context length exceeded"
        );
        #[cfg(feature = "crs")]
        {
            let message = body["error"]["message"].as_str().unwrap();
            assert!(message.contains("upstream input limit exceeded"));
            assert!(message.contains("the exact limit was not reported"));
            assert!(message.contains("Run /compact"));
            assert!(message.contains(&format!("request ID: {trace_id}")));
            assert_eq!(
                body["error"]["upstream_reason"],
                "CONTENT_LENGTH_EXCEEDS_THRESHOLD"
            );
            assert_eq!(body["error"]["stage"], "upstream_dispatch");
            assert_eq!(body["error"]["upstream_status"], 400);
            assert_eq!(message.contains("Compaction was attempted"), automatic);
            assert_eq!(body["error"]["retryable"], false);
        }
        assert_eq!(body["request_id"], trace_id);
        assert_eq!(
            main_calls.load(Ordering::SeqCst),
            if automatic { 2 } else { 1 }
        );
        let stats = expect_ok(
            daemon
                .call("stats", json!({"detail":true,"recent":10}))
                .await,
        );
        let recorded = stats["stats"]["recent_requests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|request| request["trace_id"] == trace_id && request["path"] == "/v1/messages")
            .expect("correlated request record");
        assert_eq!(recorded["status"], 400);
        assert_eq!(
            recorded["diagnostics"]["client_status"],
            if sse_error { 200 } else { 400 }
        );
        assert_eq!(recorded["diagnostics"]["upstream_status"], 400);
        assert_eq!(recorded["diagnostics"]["account_error"], false);
        daemon.stop().await;
    }
}

async fn update_compaction_config(daemon: &Daemon, edit: impl FnOnce(&mut Config)) {
    let config_path = daemon.home().join("config.toml");
    let mut config: Config =
        toml::from_str(&tokio::fs::read_to_string(&config_path).await.unwrap()).unwrap();
    edit(&mut config);
    tokio::fs::write(&config_path, toml::to_string(&config).unwrap())
        .await
        .unwrap();
    assert_eq!(
        expect_ok(daemon.call("config.reload", json!({})).await)["applied"],
        true
    );
}

fn claude_content_from_sse(body: &str) -> Vec<Value> {
    let mut blocks = Vec::<Value>::new();
    let mut inputs = std::collections::HashMap::<usize, String>::new();
    let mut stopped = false;
    for data in body.lines().filter_map(|line| line.strip_prefix("data: ")) {
        let event: Value = serde_json::from_str(data).unwrap();
        match event["type"].as_str().unwrap() {
            "error" => panic!("unexpected stream error: {event}"),
            "content_block_start" => {
                let index = event["index"].as_u64().unwrap() as usize;
                assert_eq!(
                    index,
                    blocks.len(),
                    "SSE indices must follow emission order"
                );
                blocks.push(event["content_block"].clone());
            }
            "content_block_delta" => {
                let index = event["index"].as_u64().unwrap() as usize;
                let delta = &event["delta"];
                match delta["type"].as_str().unwrap() {
                    "input_json_delta" => inputs
                        .entry(index)
                        .or_default()
                        .push_str(delta["partial_json"].as_str().unwrap()),
                    "text_delta" => {
                        let text = blocks[index]["text"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned();
                        blocks[index]["text"] = json!(text + delta["text"].as_str().unwrap());
                    }
                    "compaction_delta" => panic!(
                        "Claude Code must receive the complete checkpoint in the start block"
                    ),
                    other => panic!("unexpected fixture delta: {other}"),
                }
            }
            "message_stop" => stopped = true,
            _ => {}
        }
    }
    assert!(stopped, "{body}");
    for (index, input) in inputs {
        blocks[index]["input"] = serde_json::from_str(&input).unwrap();
    }
    blocks
}

#[tokio::test]
async fn byte_overflow_compacts_history_before_admission_and_preserves_hard_limits() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    for (automatic, oversized_current, stream) in [
        (true, false, false),
        (true, false, true),
        (false, false, false),
        (true, true, false),
    ] {
        let mock = MockServer::start().await;
        mount_context_alignment_models(&mock).await;
        Mock::given(method("POST"))
            .and(path("/generateAssistantResponse"))
            .respond_with(CompactionResponder)
            .mount(&mock)
            .await;
        let port = unused_tcp_port();
        let daemon =
            Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
        import_context_alignment_account(&daemon, 0.0).await;
        update_compaction_config(&daemon, |config| {
            config.context.max_upstream_payload_bytes = 32 * 1024;
            config.context.auto_compact_on_overflow = automatic;
            config.pool.max_concurrent_per_account = 1;
        })
        .await;
        let long = "historical record ".repeat(10000);
        let request = json!({
            "model":"source-large", "max_tokens":64, "stream":stream,
            "system":"Keep the governing instruction.",
            "tools":[{"name":"Read", "description":"Read records", "input_schema":{"type":"object"}}],
            "messages":[
                {"role":"user","content":long},
                {"role":"assistant","content":"Keep the earlier decision"},
                {"role":"user","content":if oversized_current {"current record ".repeat(10000)} else {"Continue".into()}}
            ]
        });
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap();
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
            count["input_tokens"].as_u64().unwrap() < 950000,
            "token estimate fits; bytes overflow"
        );
        assert!(
            mock.received_requests()
                .await
                .unwrap()
                .iter()
                .all(|request| request.method != "POST"),
            "count_tokens must not generate a summary"
        );
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
        let generations: Vec<_> = mock
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| request.url.path() == "/generateAssistantResponse")
            .collect();
        if automatic && !oversized_current {
            assert_eq!(status, reqwest::StatusCode::OK, "{body}");
            let content = if stream {
                claude_content_from_sse(&body)
            } else {
                serde_json::from_str::<Value>(&body).unwrap()["content"]
                    .as_array()
                    .unwrap()
                    .clone()
            };
            assert_eq!(content[0]["type"], "compaction");
            assert_eq!(
                generations.len(),
                8,
                "byte recovery covers two complete parts, consolidation and generation"
            );
            assert!(generations
                .iter()
                .all(|request| request.body.len() <= 32 * 1024));
            let payload: Value = serde_json::from_slice(&generations.last().unwrap().body).unwrap();
            let current = &payload["conversationState"]["currentMessage"]["userInputMessage"];
            assert_eq!(current["content"], "Continue");
            assert_eq!(
                current["userInputMessageContext"]["tools"][0]["toolSpecification"]["name"],
                "Read"
            );
            assert!(payload
                .to_string()
                .contains("Keep the governing instruction."));
        } else {
            assert_eq!(
                status,
                if cfg!(feature = "crs") && stream {
                    reqwest::StatusCode::OK
                } else {
                    reqwest::StatusCode::BAD_REQUEST
                },
                "{body}"
            );
            assert!(
                body.contains("translated upstream payload is too large"),
                "{body}"
            );
            assert!(
                generations.is_empty(),
                "uncompressible or disabled requests must fail before dispatch"
            );
        }
        daemon.stop().await;
    }
}

#[tokio::test]
async fn default_byte_limit_allows_compaction_of_history_larger_than_eight_mib() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(CompactionResponder)
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    // Short whitespace runs keep the estimate well inside the 1M model window
    // while exceeding the real, default 8 MiB serialized payload limit.
    let history = format!(
        "OLD_FACT=cobalt-731\n{}\nLATEST_FACT=mango-482",
        format!("{}\n", " ".repeat(127)).repeat(66000)
    );
    let response = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().unwrap()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("x-api-key", daemon.api_key.as_deref().unwrap())
        .header("user-agent", "claude-cli/2.1.260 (external, e2e)")
        .json(&json!({"model":"source-large", "max_tokens":64,
            "messages":[{"role":"user","content":history}, {"role":"assistant","content":"Earlier decision"}, {"role":"user","content":"Continue"}]
        })).send().await.unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, reqwest::StatusCode::OK, "{body}");
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["content"][0]["type"], "compaction");
    let original = body["context_management"]["applied_edits"][0]["original_input_tokens"]
        .as_u64()
        .unwrap();
    assert!(
        original < 950000,
        "byte overflow must trigger even without token overflow"
    );
    let generations: Vec<_> = mock
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/generateAssistantResponse")
        .collect();
    assert!(
        generations.len() >= 4,
        "complete parts, consolidation and generation"
    );
    assert!(generations
        .iter()
        .all(|request| request.body.len() <= 8 * 1024 * 1024));
    daemon.stop().await;
}

#[tokio::test]
async fn tool_search_continuations_preserve_history_when_a_complete_checkpoint_cannot_fit() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    for stream in [false, true] {
        let request = json!({
            "model":"resolved-tiny", "max_tokens":256, "stream":stream,
            "messages":[
                {"role":"user","content":"old context ".repeat(2000)},
                {"role":"assistant","content":"Remember the previous decision"},
                {"role":"user","content":"Find and use the lookup tool"}
            ],
            "tools":[
                {"type":"tool_search_tool_regex_20251119","name":"tool_search_tool_regex"},
                {"name":"tiny_helper","description":"A tiny helper","input_schema":{"type":"object"},"defer_loading":true},
                {"name":"lookup_records","description":"Lookup records. Documentation entry. ".repeat(250),
                 "input_schema":{"type":"object","properties":{"query":{"type":"string"}}},"defer_loading":true}
            ]
        });
        let translated = crosslane_translate::claude_to_kiro(
            &serde_json::from_value(request.clone()).unwrap(),
            &crosslane_translate::TranslationOptions::new("resolved-tiny", "AI_EDITOR"),
        );
        let tokenizer = crosslane_translate::TokenCountCache::new(8).unwrap();
        let initial_tokens = tokenizer.estimate_kiro_payload(&translated).await.unwrap() as u32;
        let window = (initial_tokens + 512) * 100 / 95 + 1;
        let mock = MockServer::start().await;
        mount_context_alignment_models_with_tiny_limit(&mock, window).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let generated = Arc::clone(&calls);
        Mock::given(method("POST")).and(path("/generateAssistantResponse"))
            .respond_with(move |request: &wiremock::Request| {
                let payload: Value = serde_json::from_slice(&request.body).unwrap();
                assert!(!payload["conversationState"]["currentMessage"]["userInputMessage"]["content"].as_str().unwrap().contains("durable conversation checkpoint"), "continuations must not acquire another account for a summary");
                let mut body = Vec::new();
                match generated.fetch_add(1, Ordering::SeqCst) {
                    0 => {
                        body.extend(event_stream_frame("assistantResponseEvent", json!({"content":"Finding the relevant tool."})));
                        body.extend(event_stream_frame("toolUseEvent", json!({"toolUseId":"search-1","name":"tool_search_tool_regex","input":"{\"pattern\":\"tiny_helper\"}","stop":true})));
                    }
                    1 => {
                        body.extend(event_stream_frame("toolUseEvent", json!({"toolUseId":"search-2","name":"tool_search_tool_regex","input":"{\"pattern\":\"lookup_records\"}","stop":true})));
                    }
                    2 => {
                        body.extend(event_stream_frame("assistantResponseEvent", json!({"content":"Tool budget reached; existing facts intact."})));
                    }
                    _ => body.extend(event_stream_frame("assistantResponseEvent", json!({"content":"Completed the client tool result."}))),
                }
                body.extend(event_stream_frame("messageMetadataEvent", json!({"usage":{"inputTokens":100,"outputTokens":20,"creditsConsumed":0.25}})));
                ResponseTemplate::new(200).insert_header("content-type", "application/vnd.amazon.eventstream").set_body_bytes(body)
            }).mount(&mock).await;
        let port = unused_tcp_port();
        let daemon =
            Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
        import_context_alignment_account(&daemon, 0.0).await;
        update_compaction_config(&daemon, |config| config.pool.max_concurrent_per_account = 1)
            .await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap();
        let response = client
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("x-api-key", daemon.api_key.as_deref().unwrap())
            .header("user-agent", "claude-cli/2.1.260 (external, e2e)")
            .header("x-claude-code-session-id", "tool-compaction-session")
            .json(&request)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        assert_eq!(status, reqwest::StatusCode::OK, "{body}");
        assert!(
            body.contains("Tool budget reached; existing facts intact."),
            "{body}"
        );
        assert!(
            body.contains("matching tool definitions exceed the remaining request budget"),
            "{body}"
        );
        assert!(!body.contains("\"type\":\"compaction\""), "{body}");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        let requests = mock.received_requests().await.unwrap();
        let payloads: Vec<Value> = requests
            .iter()
            .filter(|request| request.url.path() == "/generateAssistantResponse")
            .map(|request| serde_json::from_slice(&request.body).unwrap())
            .collect();
        assert!(payloads.iter().all(|payload| payload
            .to_string()
            .contains("Remember the previous decision")));
        daemon.stop().await;
    }
}
