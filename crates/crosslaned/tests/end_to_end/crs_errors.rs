use super::*;
use serde_json::{json, Value};
use std::sync::atomic::AtomicU16;

const FIELD_ERROR: &str = "messages.1.output_config: Extra inputs are not permitted";

#[tokio::test]
async fn none_preserves_historical_pairs_without_publishing_callable_tools() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let upstream = MockServer::start().await;
    mount_context_alignment_models(&upstream).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(|request: &wiremock::Request| {
            let wire: Value = serde_json::from_slice(&request.body).unwrap();
            let state = &wire["conversationState"];
            if state["currentMessage"]["userInputMessage"]["content"].as_str().is_some_and(|text| text.contains("OWNED_CONTENT_FILTERED")) {
                return ResponseTemplate::new(200)
                    .insert_header("content-type", "application/vnd.amazon.eventstream")
                    .set_body_bytes(event_stream_frame("metadataEvent",json!({
                        "stopReason":"CONTENT_FILTERED", "usage":{"inputTokens":17,"outputTokens":0},
                        "stopDetails":{"refusal":{"category":"CYBER","explanation":"Provider declined the request."}}
                    })));
            }
            let calls = state["history"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|item| item["assistantResponseMessage"]["toolUses"].as_array())
                .flatten()
                .collect::<Vec<_>>();
            let context = &state["currentMessage"]["userInputMessage"]["userInputMessageContext"];
            assert!(
                context.get("tools").is_none(),
                "none must not publish callable tools"
            );
            let paired = calls.len() == 1
                && calls[0]["toolUseId"] == "past-search"
                && context["toolResults"][0]["toolUseId"] == "past-search";
            let body = if paired {
                generation_body("已移除工具续答正常 中文😀")
            } else {
                // Recorded legacy-Kiro failure: transport success containing
                // context accounting only, with no assistant output frame.
                event_stream_frame("contextUsageEvent", json!({"contextUsagePercentage":0.7}))
            };
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(body)
        })
        .mount(&upstream)
        .await;
    let port = unused_tcp_port();
    let daemon = Daemon::start_http(
        port,
        &format!("{}/generateAssistantResponse", upstream.uri()),
    )
    .await;
    import_context_alignment_account(&daemon, 0.0).await;
    for removed in [false, true] {
        let mut body = json!({"model":"resolved-small","max_tokens":128,
        "tool_choice":{"type":"none"},"tools":[
            {"name":"catalog_lookup","input_schema":{"type":"object"}},
            {"name":"retired_lookup","input_schema":{"type":"object"},"defer_loading":true}
        ],"messages":[
            {"role":"user","content":"历史工具发现记录"},
            {"role":"assistant","content":[{"type":"tool_use","id":"past-search","name":"catalog_lookup","input":{"query":"retired_lookup"}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"past-search","content":[
                {"type":"tool_reference","tool_name":"retired_lookup"},{"type":"text","text":"发现工具 retired_lookup 中文😀"}
            ]}]}
        ]});
        let mut current = vec![json!({"type":"text","text":"保持历史结果，禁止新工具调用。"})];
        if removed {
            current.insert(0, json!({"type":"tool_removal","tool":{"type":"tool_reference","name":"retired_lookup"}}));
        }
        body["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role":"user","content":current}));
        for (endpoint, streaming) in [
            ("messages/count_tokens", false),
            ("messages", false),
            ("messages", true),
        ] {
            body["stream"] = streaming.into();
            let response = reqwest::Client::new()
                .post(format!("http://127.0.0.1:{port}/v1/{endpoint}"))
                .bearer_auth(daemon.api_key.as_deref().unwrap())
                .header("user-agent", "claude-cli/2.1.300 (external, e2e)")
                .json(&body)
                .send()
                .await
                .unwrap();
            let status = response.status();
            let text = response.text().await.unwrap();
            assert_eq!(status.as_u16(), 200, "{endpoint}: {text}");
            assert!(!text.contains("\"type\":\"error\""), "{text}");
            if !endpoint.ends_with("count_tokens") {
                assert!(
                    text.contains("\\u5df2") || text.contains("已移除工具续答正常"),
                    "{text}"
                );
                if streaming {
                    assert!(text.contains("message_stop"));
                }
            }
        }
    }
    for streaming in [false, true] {
        let response = reqwest::Client::new().post(format!("http://127.0.0.1:{port}/v1/messages"))
            .bearer_auth(daemon.api_key.as_deref().unwrap())
            .header("user-agent","claude-cli/2.1.300 (external, e2e)")
            .json(&json!({"model":"resolved-small","max_tokens":128,"stream":streaming,
                "tool_choice":{"type":"none"},"messages":[{"role":"user","content":"OWNED_CONTENT_FILTERED"}]}))
            .send().await.unwrap();
        assert_eq!(
            response.status().as_u16(),
            if streaming { 200 } else { 400 }
        );
        let text = response.text().await.unwrap();
        assert!(text.contains("upstream_content_filtered"), "{text}");
        assert!(text.contains("\"retryable\":false"), "{text}");
        assert!(!text.contains("message_stop"), "{text}");
    }
    let generated = upstream
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/generateAssistantResponse")
        .collect::<Vec<_>>();
    assert_eq!(
        generated.len(),
        6,
        "successful replies and explicit refusals must not use an empty-output retry"
    );
    for request in generated.iter().take(4) {
        let wire: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(wire["inferenceConfig"]["maxTokens"], 128);
        assert!(!wire.to_string().contains("archived_tool_call"));
        assert!(!wire.to_string().contains("crs_tool_choice_none"));
        assert!(wire.to_string().contains("发现工具 retired_lookup 中文😀"));
    }
    daemon.stop().await;
}

#[tokio::test]
async fn removed_tool_discoveries_allow_counting_and_kiro_continuation() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let (upstream, daemon, port) = start_input_overflow_daemon().await;
    for official in [false, true] {
        let search = if official {
            json!({"type":"tool_search_tool_regex_20251119","name":"tool_search_tool_regex"})
        } else {
            json!({"name":"custom_search","input_schema":{"type":"object"}})
        };
        let history = if official {
            json!([
                {"role":"user","content":"find old_tool"},
                {"role":"assistant","content":[
                    {"type":"server_tool_use","id":"search1","name":"tool_search_tool_regex","input":{"pattern":"old_tool"}},
                    {"type":"tool_search_tool_result","tool_use_id":"search1","content":{"type":"tool_search_tool_search_result","tool_references":[{"type":"tool_reference","tool_name":"old_tool"}]}}
                ]}
            ])
        } else {
            json!([
                {"role":"user","content":"find old_tool"},
                {"role":"assistant","content":[{"type":"tool_use","id":"search1","name":"custom_search","input":{}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"search1","content":[{"type":"tool_reference","tool_name":"old_tool"}]}]}
            ])
        };
        let mut body = json!({"model":"resolved-small","max_tokens":32,"tools":[
            search,{"name":"old_tool","input_schema":{"type":"object"},"defer_loading":true}
        ],"messages":history});
        body["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role":"user","content":[
                {"type":"tool_removal","tool":{"type":"tool_reference","name":"old_tool"}},
                {"type":"text","text":"Continue in Chinese 中文😀"}
            ]}));
        for (endpoint, stream) in [
            ("messages/count_tokens", false),
            ("messages", false),
            ("messages", true),
        ] {
            body["stream"] = stream.into();
            let response = reqwest::Client::new()
                .post(format!("http://127.0.0.1:{port}/v1/{endpoint}"))
                .bearer_auth(daemon.api_key.as_deref().unwrap())
                .header("user-agent", "claude-cli/2.1.300 (external, e2e)")
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), 200);
            let text = response.text().await.unwrap();
            assert!(!text.contains("\"type\":\"error\""), "{text}");
        }
    }
    let generated = upstream
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/generateAssistantResponse")
        .collect::<Vec<_>>();
    assert_eq!(generated.len(), 4);
    for request in generated {
        let value: Value = serde_json::from_slice(&request.body).unwrap();
        let text = value.to_string();
        assert!(text.contains("Historical tool reference"));
        assert!(text.contains("old_tool"));
        if let Some(tools) = value
            .pointer(
                "/conversationState/currentMessage/userInputMessage/userInputMessageContext/tools",
            )
            .and_then(Value::as_array)
        {
            assert!(tools
                .iter()
                .all(|tool| tool["toolSpecification"]["name"] != "old_tool"));
        }
    }
    daemon.stop().await;
}

#[tokio::test]
async fn late_tool_catalog_updates_preserve_kiro_generation_and_token_counting() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let (upstream, daemon, port) = start_input_overflow_daemon().await;
    let body = json!({"model":"resolved-small","max_tokens":32,"tools":[
        {"name":"Read","input_schema":{"type":"object"}},
        {"name":"late_reader","input_schema":{"type":"object"},"defer_loading":true}
    ],"messages":[
        {"role":"user","content":"read the source"},
        {"role":"assistant","content":[{"type":"tool_use","id":"once","name":"Read","input":{"file_path":"/tmp/中文.txt"}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"once","content":"中文工具结果😀"}]},
        {"role":"system","content":[{"type":"text","text":"keep this policy"},
            {"type":"tool_addition","tool":{"type":"tool_reference","name":"late_reader"}},
            {"type":"tool_addition","tool":{"type":"tool_definition","definition":{"name":"extra_new","description":"late definition","input_schema":{"type":"object"}}}}
        ]}
    ]});
    for (endpoint, stream) in [
        ("messages/count_tokens", false),
        ("messages", false),
        ("messages", true),
    ] {
        let mut body = body.clone();
        body["stream"] = stream.into();
        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/{endpoint}"))
            .bearer_auth(daemon.api_key.as_deref().unwrap())
            .header("user-agent", "claude-cli/2.1.300 (external, e2e)")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let text = response.text().await.unwrap();
        assert!(!text.contains("\"type\":\"error\""), "{text}");
        if endpoint.ends_with("count_tokens") {
            assert!(
                serde_json::from_str::<Value>(&text).unwrap()["input_tokens"]
                    .as_u64()
                    .unwrap()
                    > 0
            );
        }
    }
    let generated = upstream
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/generateAssistantResponse")
        .collect::<Vec<_>>();
    assert_eq!(generated.len(), 2);
    for request in generated {
        let text = std::str::from_utf8(&request.body).unwrap();
        assert!(
            text.contains("late_reader")
                && text.contains("extra_new")
                && text.contains("keep this policy")
        );
        assert!(!text.contains("tool_addition"));
        assert!(
            text.contains("once"),
            "tool result identity must be retained"
        );
    }
    daemon.stop().await;
}

async fn start_copilot_error() -> (MockServer, Daemon, u16) {
    start_copilot_error_with_status(Arc::new(AtomicU16::new(400))).await
}

async fn start_copilot_error_with_status(status: Arc<AtomicU16>) -> (MockServer, Daemon, u16) {
    let upstream = MockServer::start().await;
    for (endpoint, body) in [
        ("/user", json!({"id":7,"login":"crs-error-test"})),
        (
            "/copilot_internal/v2/token",
            json!({"token":"fake-api-token","expires_at":4_000_000_000i64,"endpoints":{"api":upstream.uri()}}),
        ),
        (
            "/models",
            json!({"data":[{"id":"claude-opus-5.5","capabilities":{"supported_endpoints":["/v1/messages"]}}]}),
        ),
    ] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&upstream)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |_: &wiremock::Request| {
            ResponseTemplate::new(status.load(Ordering::SeqCst)).set_body_json(json!({
                "type":"error",
                "error":{"type":"invalid_request_error","message":FIELD_ERROR}
            }))
        })
        .mount(&upstream)
        .await;
    let port = unused_tcp_port();
    let daemon = Daemon::start_copilot(port, &upstream.uri()).await;
    expect_ok(
        daemon
            .call(
                "v2.account.importToken",
                json!({
                    "provider":"copilot","token":"fake-github-token"
                }),
            )
            .await,
    );
    (upstream, daemon, port)
}

async fn start_input_overflow_daemon() -> (MockServer, Daemon, u16) {
    let upstream = MockServer::start().await;
    mount_context_alignment_models(&upstream).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(|request: &wiremock::Request| {
            let payload: Value = serde_json::from_slice(&request.body).unwrap();
            let current = payload["conversationState"]["currentMessage"]["userInputMessage"]
                ["content"]
                .as_str()
                .unwrap();
            let text = if current.contains("durable conversation checkpoint")
                || current.contains("summary")
            {
                "<summary>Recovery source retained: project=cobalt-731; parameter=77 replaces 42.</summary>"
            } else {
                "Same session continued successfully."
            };
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(generation_body(text))
        })
        .mount(&upstream)
        .await;
    let port = unused_tcp_port();
    let daemon = Daemon::start_http(
        port,
        &format!("{}/generateAssistantResponse", upstream.uri()),
    )
    .await;
    import_context_alignment_account(&daemon, 0.0).await;
    super::model_policy::configure(&daemon, |config| {
        // A small fixture window exercises actual admission without allocating
        // 1M tokens or making any real inference request.
        config.model_mapping = vec![ModelMappingRule {
            name: "recovery-guidance-fixture".into(),
            source_models: vec![
                "claude-sonnet-5.5".into(),
                "claude-sonnet-5-5".into(),
                "claude-opus-5.5".into(),
                "claude-opus-5-5".into(),
            ],
            target_models: vec!["resolved-small".into()],
            ..ModelMappingRule::default()
        }];
    })
    .await;
    (upstream, daemon, port)
}

#[tokio::test]
async fn indivisible_input_guidance_preserves_protocol_errors_in_both_modes() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let (upstream, daemon, port) = start_input_overflow_daemon().await;
    let client = reqwest::Client::new();
    let oversized = "synthetic recovery record ".repeat(5_000);
    for endpoint in ["messages", "chat/completions", "responses"] {
        for stream in [false, true] {
            for scope in ["current_message", "required_context"] {
                let current = if scope == "current_message" {
                    &oversized
                } else {
                    "hello"
                };
                let system = if scope == "required_context" {
                    &oversized
                } else {
                    "Be brief."
                };
                let body = match endpoint {
                    "messages" => json!({"model":"resolved-tiny","max_tokens":32,"stream":stream,
                        "system":system,"messages":[{"role":"user","content":current}]}),
                    "responses" => {
                        json!({"model":"resolved-tiny","max_output_tokens":32,"stream":stream,
                        "instructions":system,"input":current})
                    }
                    _ => json!({"model":"resolved-tiny","max_tokens":32,"stream":stream,
                        "messages":[{"role":"system","content":system},{"role":"user","content":current}]}),
                };
                let response = client
                    .post(format!("http://127.0.0.1:{port}/v1/{endpoint}"))
                    .bearer_auth(daemon.api_key.as_deref().unwrap())
                    .header(
                        "user-agent",
                        if endpoint == "messages" {
                            "claude-cli/2.1.296 (external, e2e)"
                        } else {
                            "codex_cli_rs/0.1.0"
                        },
                    )
                    .json(&body)
                    .send()
                    .await
                    .unwrap();
                let sse = cfg!(feature = "crs") && endpoint == "messages" && stream;
                let compat = cfg!(feature = "crs") && endpoint == "messages";
                assert_eq!(response.status().as_u16(), if compat { 200 } else { 400 });
                if compat {
                    assert_eq!(response.headers()["x-crosslane-error-status"], "400");
                    if !stream {
                        assert!(response.headers()["content-type"]
                            .to_str()
                            .unwrap()
                            .starts_with("application/json"));
                        assert_eq!(response.headers()["cache-control"], "no-store");
                    }
                }
                let text = response.text().await.unwrap();
                let error: Value = if sse {
                    sse_error_payload(&text)
                } else {
                    serde_json::from_str(&text).unwrap()
                };
                assert_eq!(error["error"]["type"], "invalid_request_error", "{error}");
                if endpoint != "messages" {
                    // OpenAI's existing hard context guard and error envelope
                    // retain their native wording, including stream requests.
                    assert!(error["error"]["code"].is_null(), "{error}");
                    assert!(error["error"]["context"].is_null(), "{error}");
                    assert!(
                        error["error"]["message"]
                            .as_str()
                            .unwrap()
                            .starts_with("prompt is too long"),
                        "{error}"
                    );
                    assert!(!text.contains("synthetic recovery record"));
                    continue;
                }
                assert_eq!(error["error"]["code"], "context_length_exceeded", "{error}");
                assert_eq!(error["error"]["retryable"], false, "{error}");
                if compat {
                    assert_eq!(error["type"], "error");
                    assert_eq!(error["error"]["status"], 400);
                    assert!(error.get("usage").is_none());
                    assert!(error.get("content").is_none());
                }
                assert_eq!(
                    error["error"]["context"]["overflow_scope"], scope,
                    "{error}"
                );
                let message = error["error"]["message"].as_str().unwrap();
                let hint = error["error"]["hint"].as_str().unwrap();
                assert!(
                    message.contains(hint),
                    "guidance must be visible in error.message: {error}"
                );
                assert!(!message.contains("prompt is too long"), "{error}");
                if scope == "current_message" {
                    assert!(
                        message.contains("try /compact") && message.contains("Esc Esc or /rewind"),
                        "{error}"
                    );
                } else {
                    assert!(
                        message.contains("reduce required system instructions"),
                        "{error}"
                    );
                    assert!(message.contains("Run /compact only if"), "{error}");
                }
                assert!(!text.contains("synthetic recovery record"));
                let stats = expect_ok(
                    daemon
                        .call("stats", json!({"detail":true,"recent":20}))
                        .await,
                );
                let recorded = stats["stats"]["recent_requests"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|record| record["trace_id"] == error["request_id"])
                    .expect("context failure remains in error statistics");
                assert_eq!(recorded["status"], 400);
                assert_eq!(
                    recorded["diagnostics"]["client_status"],
                    if compat { 200 } else { 400 }
                );
                assert!(recorded["diagnostics"]["upstream_status"].is_null());
                if sse {
                    assert_eq!(error["error"]["status"], 400);
                    assert!(!text.contains("event: message_stop"));
                }
            }
        }
    }
    assert!(
        !upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .any(|request| { request.url.path() == "/generateAssistantResponse" }),
        "indivisible input must be rejected before inference or compaction"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn copilot_error_statuses_are_preserved_for_both_protocols_and_modes() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let status = Arc::new(AtomicU16::new(400));
    let (_upstream, daemon, port) = start_copilot_error_with_status(Arc::clone(&status)).await;
    let client = reqwest::Client::new();
    for code in [
        400, 401, 402, 403, 404, 408, 409, 413, 422, 429, 500, 502, 503, 504, 529, 599,
    ] {
        status.store(code, Ordering::SeqCst);
        for endpoint in ["messages", "chat/completions"] {
            for stream in [false, true] {
                let sse_error = cfg!(feature = "crs") && endpoint == "messages" && stream;
                let response = client
                    .post(format!("http://127.0.0.1:{port}/v1/{endpoint}"))
                    .bearer_auth(daemon.api_key.as_deref().unwrap())
                    .header("user-agent", "claude-cli/2.1.295 (external, e2e)")
                    .json(
                        &json!({"model":"copilot/claude-opus-5-5","max_tokens":16,"stream":stream,
                        "messages":[{"role":"user","content":"hello"}]}),
                    )
                    .send()
                    .await
                    .unwrap();
                assert_eq!(
                    response.status().as_u16(),
                    if sse_error { 200 } else { code },
                    "{endpoint} stream={stream}"
                );
                let text = response.text().await.unwrap();
                let body: Value = if sse_error {
                    sse_error_payload(&text)
                } else {
                    serde_json::from_str(&text).unwrap()
                };
                assert!(body["error"]["message"].is_string(), "{body}");
                if cfg!(feature = "crs") && endpoint == "messages" {
                    assert_eq!(body["error"]["status"], code);
                    assert_eq!(body["error"]["upstream_status"], code);
                    assert_eq!(body["error"]["detail"], FIELD_ERROR);
                }
            }
        }
    }
    daemon.stop().await;
}

async fn start_crs_console(daemon: &Daemon, port: u16) -> (Child, String) {
    let crs = std::env::var("CROSSLANE_CRS_SOURCE").expect("set CROSSLANE_CRS_SOURCE");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/crs_console.cjs");
    let mut relay = Command::new("node")
        .arg(script)
        .arg(crs)
        .arg(format!("http://127.0.0.1:{port}"))
        .arg(daemon.api_key.as_deref().unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("start CRS relay fixture");
    let mut output = BufReader::new(relay.stdout.take().unwrap()).lines();
    let address = tokio::time::timeout(Duration::from_secs(10), output.next_line())
        .await
        .expect("CRS fixture ready")
        .unwrap()
        .unwrap();
    (relay, address)
}

#[cfg(feature = "crs")]
#[tokio::test]
#[ignore = "requires CROSSLANE_CRS_SOURCE; executes unmodified local CRS source and loopback services only"]
async fn local_context_json_and_sse_errors_survive_unmodified_crs_console() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let (upstream, daemon, port) = start_input_overflow_daemon().await;
    let (mut relay, address) = start_crs_console(&daemon, port).await;
    let client = reqwest::Client::new();
    let oversized = "synthetic recovery record ".repeat(5_000);
    for stream in [false, true] {
        for scope in ["current_message", "required_context"] {
            let response = client.post(format!("{address}/v1/messages"))
                .header("user-agent", "claude-cli/2.1.300 (external, e2e)")
                .json(&json!({"model":"resolved-tiny","max_tokens":32,"stream":stream,
                    "system":if scope == "required_context" { &oversized } else { "Be brief." },
                    "messages":[{"role":"user","content":if scope == "current_message" { &oversized } else { "hello" }}]}))
                .send().await.unwrap();
            assert_eq!(response.status(), 200);
            if !stream {
                assert!(response.headers()["content-type"]
                    .to_str()
                    .unwrap()
                    .starts_with("application/json"));
                assert_eq!(response.headers()["x-crosslane-error-status"], "400");
            }
            let text = response.text().await.unwrap();
            let error: Value = if stream {
                sse_error_payload(&text)
            } else {
                serde_json::from_str(&text).unwrap()
            };
            assert_eq!(error["type"], "error", "{error}");
            assert_eq!(error["error"]["code"], "context_length_exceeded");
            assert_eq!(error["error"]["status"], 400);
            assert_eq!(error["error"]["stage"], "context_validation");
            assert_eq!(error["error"]["retryable"], false);
            assert_eq!(error["error"]["context"]["overflow_scope"], scope);
            assert!(
                error["error"]["context"]["minimum_input_tokens"]
                    .as_u64()
                    .unwrap()
                    > error["error"]["context"]["maximum_input_tokens"]
                        .as_u64()
                        .unwrap()
            );
            let message = error["error"]["message"].as_str().unwrap();
            assert!(message.contains("Recovery:"), "{error}");
            assert!(
                message.contains(error["request_id"].as_str().unwrap()),
                "{error}"
            );
            assert!(error.get("usage").is_none());
            assert!(error.get("content").is_none());
            assert!(!text.contains("E015"));
            assert!(!text.contains("synthetic recovery record"));
        }
    }
    assert!(!upstream
        .received_requests()
        .await
        .unwrap()
        .iter()
        .any(|request| request.url.path() == "/generateAssistantResponse"));
    relay.kill().await.unwrap();
    daemon.stop().await;
}

#[cfg(feature = "crs")]
#[tokio::test]
#[ignore = "requires CROSSLANE_CRS_SOURCE; executes local source and loopback services only"]
async fn supplier_status_and_stream_diagnostics_survive_unmodified_crs_console() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let status = Arc::new(AtomicU16::new(400));
    let (_upstream, daemon, port) = start_copilot_error_with_status(Arc::clone(&status)).await;
    let (mut relay, address) = start_crs_console(&daemon, port).await;
    let client = reqwest::Client::new();
    for code in [
        400, 401, 402, 403, 404, 408, 409, 413, 422, 429, 500, 502, 503, 504, 529, 599,
    ] {
        status.store(code, Ordering::SeqCst);
        for stream in [false, true] {
            let response = client
                .post(format!("{address}/v1/messages"))
                .header("user-agent", "claude-cli/2.1.295 (external, e2e)")
                .json(
                    &json!({"model":"copilot/claude-opus-5-5","max_tokens":16,"stream":stream,
                    "messages":[{"role":"user","content":"hello"}]}),
                )
                .send()
                .await
                .unwrap();
            let actual = response.status().as_u16();
            let text = response.text().await.unwrap();
            assert_eq!(
                actual,
                if stream { 200 } else { code },
                "stream={stream}: {text}"
            );
            if stream {
                let body = sse_error_payload(&text);
                assert_eq!(body["error"]["status"], code);
                assert_eq!(body["error"]["upstream_status"], code);
                assert_eq!(body["error"]["detail"], FIELD_ERROR);
                assert!(body["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains(body["request_id"].as_str().unwrap()));
            } else {
                // Unmodified Console sanitizes non-2xx bodies, independently
                // of Crosslane's rich diagnostic JSON and original status.
                let body: Value = serde_json::from_str(&text).unwrap();
                assert!(body["error"]["message"].is_string(), "{body}");
                assert!(body.get("request_id").is_none(), "{body}");
                assert!(body["error"].get("detail").is_none(), "{body}");
                assert!(!text.contains(FIELD_ERROR), "{text}");
            }
        }
    }
    relay.kill().await.unwrap();
    daemon.stop().await;
}

#[tokio::test]
async fn copilot_errors_preserve_crs_diagnostics_and_report_build_features() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let version = Command::new(env!("CARGO_BIN_EXE_crosslaned"))
        .arg("--version")
        .output()
        .await
        .unwrap();
    assert!(version.status.success());
    let version = String::from_utf8(version.stdout).unwrap();
    assert!(version.contains(&format!("crs={}", cfg!(feature = "crs"))));
    assert!(version.contains(&format!("sso={}", cfg!(feature = "sso"))));

    let (upstream, daemon, port) = start_copilot_error().await;
    let client = reqwest::Client::new();
    for stream in [false, true] {
        let response = client
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .bearer_auth(daemon.api_key.as_deref().unwrap())
            .header("user-agent", "claude-cli/2.1.295 (external, cli)")
            .json(
                &json!({"model":"claude-opus-5-5","max_tokens":16,"stream":stream,
                "messages":[{"role":"user","content":"hello"}]}),
            )
            .send()
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.text().await.unwrap();
        let sse_error = cfg!(feature = "crs") && stream;
        assert_eq!(status, if sse_error { 200 } else { 400 }, "{body}");
        assert_eq!(
            headers["content-type"],
            if sse_error {
                "text/event-stream"
            } else {
                "application/json"
            }
        );
        let error: Value = if sse_error {
            sse_error_payload(&body)
        } else {
            serde_json::from_str(&body).unwrap()
        };
        assert_eq!(error["error"]["type"], "invalid_request_error");
        if cfg!(feature = "crs") {
            assert_eq!(error["error"]["detail"], FIELD_ERROR);
            assert_eq!(error["error"]["stage"], "provider_upstream");
            assert_eq!(error["error"]["upstream_status"], 400);
            assert_eq!(error["error"]["retryable"], false);
            let message = error["error"]["message"].as_str().unwrap();
            assert!(message.starts_with(FIELD_ERROR));
            assert!(message.contains(error["request_id"].as_str().unwrap()));
        } else {
            assert_eq!(error["error"]["message"], FIELD_ERROR);
        }
    }
    // This probe is entirely local, including when no model was supplied.
    let probe = client
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .bearer_auth(daemon.api_key.as_deref().unwrap())
        .json(&json!({"stream":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        probe.status().as_u16(),
        if cfg!(feature = "crs") { 200 } else { 400 }
    );
    let unauthorized = client
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .json(&json!({"stream":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);
    assert_eq!(
        upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == "/v1/messages")
            .count(),
        2
    );
    daemon.stop().await;
}

fn claude_code_command(claude: &str, isolated: &Path, address: &str) -> Command {
    let mut client = Command::new(claude);
    client.env_clear();
    for name in ["PATH", "HOME", "TMPDIR"] {
        if let Some(value) = std::env::var_os(name) {
            client.env(name, value);
        }
    }
    client
        .current_dir(isolated)
        .env("ANTHROPIC_BASE_URL", address)
        .env("ANTHROPIC_API_KEY", "local-fake-key")
        .env("CLAUDE_CONFIG_DIR", isolated.join("claude"))
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("DISABLE_TELEMETRY", "1")
        .env("DISABLE_ERROR_REPORTING", "1")
        .args([
            "--bare",
            "--print",
            "--output-format",
            "json",
            "--tools",
            "",
            "--setting-sources",
            "",
        ])
        .kill_on_drop(true);
    client
}

#[tokio::test]
#[ignore = "requires CROSSLANE_CRS_SOURCE and CROSSLANE_CLAUDE_CODE_BIN; executes local clients only"]
async fn claude_code_displays_copilot_error_through_unmodified_crs_console() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let claude = std::env::var("CROSSLANE_CLAUDE_CODE_BIN").expect("set CROSSLANE_CLAUDE_CODE_BIN");
    let (_upstream, daemon, port) = start_copilot_error().await;
    let (mut relay, address) = start_crs_console(&daemon, port).await;
    let isolated = tempfile::tempdir().unwrap();
    let mut client = claude_code_command(&claude, isolated.path(), &address);
    client.args([
        "--no-session-persistence",
        "--max-turns",
        "1",
        "--model",
        "claude-opus-5-5",
        "hello",
    ]);
    let output = tokio::time::timeout(Duration::from_secs(30), client.output())
        .await
        .expect("Claude Code completed")
        .unwrap();
    assert!(!output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "Claude Code result: {error}; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert_eq!(result["is_error"], true, "{result}");
    assert_eq!(result["terminal_reason"], "api_error", "{result}");
    assert_eq!(result["total_cost_usd"], 0, "{result}");
    assert_eq!(result["usage"]["input_tokens"], 0, "{result}");
    assert_eq!(result["usage"]["output_tokens"], 0, "{result}");
    let message = result["result"].as_str().unwrap();
    if cfg!(feature = "crs") {
        assert!(message.contains(FIELD_ERROR), "{message}");
        assert!(message.contains("upstream HTTP 400"), "{message}");
        assert!(message.contains("stage: provider_upstream"), "{message}");
        assert!(message.contains("request ID: trace_"), "{message}");
        let requests: Value = reqwest::get(format!("{address}/__requests"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            requests,
            json!([{"stream":true}]),
            "no non-streaming fallback"
        );
    } else {
        assert_eq!(message, "API Error: 400 Internal server error");
    }
    relay.kill().await.unwrap();
    relay.wait().await.unwrap();
    daemon.stop().await;
}

#[cfg(feature = "crs")]
#[tokio::test]
#[ignore = "requires CROSSLANE_CRS_SOURCE and CROSSLANE_CLAUDE_CODE_BIN; executes local clients only"]
async fn claude_code_displays_input_recovery_and_can_continue_the_same_session() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let claude = std::env::var("CROSSLANE_CLAUDE_CODE_BIN").expect("set CROSSLANE_CLAUDE_CODE_BIN");
    let (upstream, daemon, port) = start_input_overflow_daemon().await;
    let (mut relay, address) = start_crs_console(&daemon, port).await;
    let oversized = format!(
        "project=cobalt-731; parameter=42.\n{}\nCorrection: parameter=77 replaces 42.",
        "synthetic source record ".repeat(25_000)
    );
    for model in ["claude-sonnet-5.5", "claude-opus-5.5"] {
        let isolated = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let mut seed_boundary = String::new();
        for (index, prompt) in [
            "Remember project=cobalt-731; parameter=42, later corrected to 77. Reply briefly.",
            oversized.as_str(),
            "Continue briefly without adding new source input.",
            "/compact",
            "Continue after compact.",
            "Reduced source part: parameter=77 replaces 42. Continue briefly.",
            "/compact",
            "Continue after compacting the reduced source.",
        ]
        .into_iter()
        .enumerate()
        {
            let before = upstream.received_requests().await.unwrap().len();
            let mut client = claude_code_command(&claude, isolated.path(), &address);
            client.args([
                "--model",
                model,
                "--system-prompt",
                "Keep replies short.",
                if index == 0 {
                    "--session-id"
                } else {
                    "--resume"
                },
                &session,
            ]);
            if index == 5 {
                // Equivalent to conversation-only rewind to the last successful
                // turn: preserve earlier facts and the session ID, then replace
                // the rejected input with a smaller part. Never clear a session.
                client.args(["--resume-session-at", &seed_boundary]);
            }
            client
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = client.spawn().unwrap();
            let mut stdin = child.stdin.take().unwrap();
            stdin.write_all(prompt.as_bytes()).await.unwrap();
            stdin.shutdown().await.unwrap();
            drop(stdin);
            let output = tokio::time::timeout(Duration::from_secs(45), child.wait_with_output())
                .await
                .expect("Claude Code completed")
                .unwrap();
            let result: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
                panic!(
                    "Claude Code result: {error}; stdout: {}; stderr: {}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                )
            });
            println!(
                "local recovery fixture: {}",
                json!({
                    "model":model,"step":index,"session_id":result["session_id"],
                    "is_error":result["is_error"],"result":result["result"],
                    "terminal_reason":result["terminal_reason"]
                })
            );
            assert_eq!(result["session_id"], session, "{result}");
            if index == 1 || index == 2 || index == 4 {
                assert!(!output.status.success());
                assert_eq!(result["is_error"], true, "{result}");
                assert_eq!(result["terminal_reason"], "api_error", "{result}");
                let message = result["result"].as_str().unwrap();
                assert!(message.contains("Current input exceeds"), "{message}");
                assert!(
                    message.contains("Stop retrying the same oversized input"),
                    "{message}"
                );
                assert!(message.contains("send a short follow-up"), "{message}");
                assert!(
                    message.contains("try /compact") && message.contains("Esc Esc or /rewind"),
                    "{message}"
                );
                assert!(
                    message.contains("code: context_length_exceeded"),
                    "{message}"
                );
                assert_eq!(result["usage"]["input_tokens"], 0, "{result}");
                assert_eq!(result["usage"]["output_tokens"], 0, "{result}");
                assert_eq!(
                    upstream.received_requests().await.unwrap().len(),
                    before,
                    "no inference or futile summary retry on oversized current input"
                );
            } else {
                assert!(output.status.success(), "{result}");
                assert_eq!(result["is_error"], false, "{result}");
            }
            if index == 5 {
                let requests = upstream.received_requests().await.unwrap();
                let resumed: Value =
                    serde_json::from_slice(&requests.last().unwrap().body).unwrap();
                let source = resumed.to_string();
                assert!(
                    source.contains("Remember project=cobalt-731"),
                    "earlier user context survives rewind"
                );
                assert!(
                    source.contains("parameter=77 replaces 42"),
                    "reduced input preserves the correction"
                );
                assert!(
                    !source.contains("synthetic source record"),
                    "only the oversized turn was replaced"
                );
            }
            if index == 0 {
                let mut projects = tokio::fs::read_dir(isolated.path().join("claude/projects"))
                    .await
                    .unwrap();
                while let Some(project) = projects.next_entry().await.unwrap() {
                    let transcript = project.path().join(format!("{session}.jsonl"));
                    if let Ok(text) = tokio::fs::read_to_string(transcript).await {
                        seed_boundary = text
                            .lines()
                            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                            .filter(|event| event["type"] == "assistant")
                            .filter_map(|event| event["uuid"].as_str().map(str::to_owned))
                            .next_back()
                            .unwrap();
                        break;
                    }
                }
                assert!(
                    !seed_boundary.is_empty(),
                    "saved seed turn is available for conversation-only rewind"
                );
            }
        }
    }
    let requests: Value = reqwest::get(format!("{address}/__requests"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        requests
            .as_array()
            .unwrap()
            .iter()
            .all(|request| request["stream"] == true),
        "Claude Code must retain streaming through unmodified CRS: {requests}"
    );
    relay.kill().await.unwrap();
    relay.wait().await.unwrap();
    daemon.stop().await;
}
