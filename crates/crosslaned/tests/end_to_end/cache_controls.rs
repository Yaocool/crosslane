use super::*;

#[tokio::test]
async fn failed_account_retry_keeps_original_usage_and_emits_no_candidate_usage() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ListAvailableModels"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"models":[{"modelId":"claude-opus-5.5"}]})),
        )
        .mount(&mock)
        .await;
    let accounts_seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let observed = accounts_seen.clone();
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(move |request: &wiremock::Request| {
            let id = if request.headers["authorization"].to_str().unwrap().ends_with("owned-retry-a") {
                "acc_11111111"
            } else { "acc_22222222" };
            let mut seen = observed.lock().unwrap();
            seen.push(id.to_owned());
            if seen.len() == 1 {
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/vnd.amazon.eventstream")
                    .set_body_bytes([
                        event_stream_frame("messageMetadataEvent", json!({"tokenUsage":{"inputTokens":41,"outputTokens":3},"creditsConsumed":0.125})),
                        event_stream_frame("serviceUnavailableError", json!({"message":"Owned initial account failure."})),
                    ].concat())
            } else {
                ResponseTemplate::new(500).set_body_string("Owned candidate was not accepted.")
            }
        }).expect(2).mount(&mock).await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    let accounts = [("acc_11111111", "owned-retry-a"), ("acc_22222222", "owned-retry-b")].map(|(id, token)| json!({
        "id":id,"email":format!("{id}@example.com"),"machine_id":"1".repeat(64),
        "credentials":{"access_token":token,"region":"us-east-1","expires_at":4_000_000_000i64,"auth_method":"idc"},
        "usage":{"current":0.0,"limit":100.0,"percent_used":0.0,"updated_at":1},"created_at":1
    }));
    expect_ok(
        daemon
            .call("account.import", json!({"accounts":accounts}))
            .await,
    );
    expect_ok(daemon.call("models", json!({})).await);
    model_policy::configure(&daemon, |config| {
        config.features.enable_prompt_cache = true;
        config.features.enable_model_fallback = false;
        config.upstream.max_retries = 1;
        config.upstream.preferred_endpoint = Some(crosslane_core::config::Endpoint::Amazonq);
    })
    .await;
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("x-api-key", daemon.api_key.as_deref().unwrap())
        .header("user-agent", "claude-cli/2.1.316 (external, e2e)")
        .json(&json!({"model":"claude-opus-5.5","max_tokens":64,"stream":true,
            "system":[{"type":"text","text":"stable retry cache fixture ".repeat(600),"cache_control":{"type":"ephemeral"}}],
            "messages":[{"role":"user","content":"question"}]}))
        .send().await.unwrap();
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    assert!(body.contains("\"type\":\"error\""), "{body}");
    let starts: Vec<Value> = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["type"] == "message_start")
        .collect();
    assert_eq!(starts.len(), usize::from(cfg!(feature = "crs")), "{body}");
    // CRS repairs an otherwise start-less error stream with an empty start;
    // neither attempt's provisional cache allocation may leak into it.
    for start in starts {
        assert_eq!(
            start["message"]["usage"],
            json!({"input_tokens":0,"output_tokens":0}),
            "{body}"
        );
    }
    let seen = accounts_seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_ne!(seen[0], seen[1], "must exercise cross-account takeover");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let stats = expect_ok(
            daemon
                .call("stats", json!({"detail":true,"recent":10}))
                .await,
        );
        if let Some(row) = stats["stats"]["recent_requests"]
            .as_array()
            .unwrap()
            .first()
        {
            assert_eq!(
                row["account_id"], seen[0],
                "failed candidate took over accounting: {row}"
            );
            assert_eq!(row["kiro_model"], "claude-opus-5.5");
            assert_eq!(row["input_tokens"], 41, "{row}");
            assert_eq!(row["output_tokens"], 3, "{row}");
            assert_eq!(row["diagnostics"]["cache_read_tokens"], 0, "{row}");
            assert_eq!(row["diagnostics"]["cache_write_tokens"], 0, "{row}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "missing failure settlement: {stats}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    mock.verify().await;
    daemon.stop().await;
}

#[tokio::test]
async fn protocols_report_real_cache_usage_from_split_metadata_before_or_after_content() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ListAvailableModels"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"models":[{"modelId":"claude-opus-5"}]})),
        )
        .mount(&mock)
        .await;
    let conversations = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = conversations.clone();
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(move |request: &wiremock::Request| {
            let wire: Value = serde_json::from_slice(&request.body).unwrap();
            let content = wire["conversationState"]["currentMessage"]["userInputMessage"]
                ["content"]
                .as_str()
                .unwrap();
            let metadata_first = content.contains("metadata-first");
            assert!(!wire.to_string().contains("cache-key-fixture"));
            if content.contains("openai-cache-probe") {
                observed
                    .lock()
                    .unwrap()
                    .push(wire["conversationState"]["conversationId"].clone());
            }
            let metadata = [
                (
                    "messageMetadataEvent",
                    json!({"tokenUsage":{"inputTokens":1000,"outputTokens":20}}),
                ),
                (
                    "usageEvent",
                    json!({"tokenUsage":{"cacheReadInputTokens":600}}),
                ),
                (
                    "messageMetadataEvent",
                    json!({"tokenUsage":{"cacheWriteInputTokens":100}}),
                ),
            ]
            .into_iter()
            .flat_map(|(kind, body)| event_stream_frame(kind, body))
            .collect::<Vec<_>>();
            let text = event_stream_frame("assistantResponseEvent", json!({"content":"pong"}));
            let body = if metadata_first {
                [metadata, text].concat()
            } else {
                [text, metadata].concat()
            };
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(body)
        })
        .expect(16)
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    expect_ok(daemon.call("account.import", json!({"accounts":[{
        "id":"acc_66666666","email":"cache-test@example.com","machine_id":"6".repeat(64),
        "credentials":{"access_token":"synthetic-cache-test","region":"us-east-1",
            "expires_at":4_000_000_000i64,"auth_method":"idc"},
        "usage":{"current":0.0,"limit":100.0,"percent_used":0.0,"updated_at":1},"created_at":1
    }]})).await);
    expect_ok(daemon.call("models", json!({})).await);
    let client = reqwest::Client::new();
    // Keep enable_prompt_cache at its default false: reported upstream usage
    // must be forwarded even when this proxy does not emit cache hints.
    for route in ["v1/messages", "v1/chat/completions", "v1/responses"] {
        let claude = route.ends_with("messages");
        let chat = route.ends_with("completions");
        for metadata_first in [true, false] {
            for stream in [false, true] {
                for include_usage in if chat {
                    &[false, true][..]
                } else {
                    &[true][..]
                } {
                    // Vary the whole first message. The shared cache key must
                    // retain affinity independently of message fingerprints.
                    let content = format!(
                        "{} {} {stream} {include_usage}",
                        if metadata_first {
                            "metadata-first"
                        } else {
                            "metadata-last"
                        },
                        if claude {
                            "claude-cache-probe"
                        } else {
                            "openai-cache-probe"
                        }
                    );
                    let mut request = if route.ends_with("responses") {
                        json!({"model":"claude-opus-5","max_output_tokens":100,"stream":stream,"store":false,
                            "input":[{"role":"user","content":content}],"prompt_cache_key":"cache-key-fixture"})
                    } else {
                        json!({"model":"claude-opus-5","max_tokens":100,"stream":stream,
                            "messages":[{"role":"user","content":content}]})
                    };
                    if chat {
                        request["prompt_cache_key"] = json!("cache-key-fixture");
                        if stream && *include_usage {
                            request["stream_options"] = json!({"include_usage":true});
                        }
                    }
                    let response = client
                        .post(format!("http://127.0.0.1:{port}/{route}"))
                        .header("x-api-key", daemon.api_key.as_deref().unwrap())
                        .bearer_auth(daemon.api_key.as_deref().unwrap())
                        .header("anthropic-version", "2023-06-01")
                        .header(
                            "user-agent",
                            if claude {
                                "claude-cli/2.1.266 (external, cli)"
                            } else {
                                "codex_cli_rs/0.1.0"
                            },
                        )
                        .json(&request)
                        .send()
                        .await
                        .unwrap();
                    let status = response.status();
                    let body = response.text().await.unwrap();
                    assert_eq!(status, reqwest::StatusCode::OK, "{route}: {body}");
                    let usage = if stream {
                        let events = body
                            .lines()
                            .filter_map(|line| line.strip_prefix("data: "))
                            .filter(|data| *data != "[DONE]")
                            .map(|data| serde_json::from_str::<Value>(data).unwrap())
                            .collect::<Vec<_>>();
                        if claude {
                            let start = events
                                .iter()
                                .find(|event| event["type"] == "message_start")
                                .unwrap();
                            assert_eq!(
                                start["message"]["usage"]["cache_read_input_tokens"],
                                if metadata_first { 600 } else { 0 }
                            );
                            assert_eq!(
                                start["message"]["usage"]["cache_creation_input_tokens"],
                                if metadata_first { 100 } else { 0 }
                            );
                            if metadata_first {
                                assert_eq!(start["message"]["usage"]["input_tokens"], 300);
                            }
                            events
                                .iter()
                                .find(|event| event["type"] == "message_delta")
                                .unwrap()["usage"]
                                .clone()
                        } else if chat {
                            assert!(body.contains("data: [DONE]"));
                            if !include_usage {
                                assert!(
                                    events.iter().all(|event| event["usage"].is_null()),
                                    "{body}"
                                );
                                continue;
                            }
                            let event = events
                                .iter()
                                .find(|event| event["usage"].is_object())
                                .unwrap();
                            assert_eq!(event["choices"], json!([]));
                            event["usage"].clone()
                        } else {
                            events
                                .iter()
                                .find(|event| event["type"] == "response.completed")
                                .unwrap()["response"]["usage"]
                                .clone()
                        }
                    } else {
                        serde_json::from_str::<Value>(&body).unwrap()["usage"].clone()
                    };
                    if claude {
                        assert_eq!(usage["input_tokens"], 300, "{body}");
                        assert_eq!(usage["cache_read_input_tokens"], 600, "{body}");
                        assert_eq!(usage["cache_creation_input_tokens"], 100, "{body}");
                    } else {
                        let (input, details) = if chat {
                            ("prompt_tokens", "prompt_tokens_details")
                        } else {
                            ("input_tokens", "input_tokens_details")
                        };
                        assert_eq!(usage[input], 1000, "{body}");
                        assert_eq!(usage["total_tokens"], 1020, "{body}");
                        assert_eq!(usage[details]["cached_tokens"], 600, "{body}");
                        assert_eq!(usage[details]["cache_write_tokens"], 100, "{body}");
                    }
                    assert_eq!(
                        usage[if chat {
                            "completion_tokens"
                        } else {
                            "output_tokens"
                        }],
                        20,
                        "{body}"
                    );
                }
            }
        }
    }
    let conversations = conversations.lock().unwrap();
    assert_eq!(conversations.len(), 12);
    assert!(conversations[0].is_string());
    assert!(
        conversations.iter().all(|id| id == &conversations[0]),
        "cache affinity changed: {conversations:?}"
    );
}

use serde_json::{json, Value};

fn request() -> Value {
    json!({
        "model":"claude-opus-5","max_tokens":128,"cache_control":null,
        "system":[{"type":"text","text":"System context","cache_control":{
            "type":"ephemeral","ttl":"1h","scope":"global"}}],
        "tools":[{"name":"lookup","input_schema":{"type":"object"},"cache_control":{
            "type":"ephemeral","ttl":"1h","evict_on_complete":true}}],
        "messages":[
            {"role":"user","content":"Hello","cache_control":null},
            {"role":"assistant","content":[{"type":"text","text":"Ready",
                "cache_control":null}]},
            {"role":"user","content":[
                {"type":"text","text":"Reply pong","cache_control":{"type":"ephemeral"}},
                {"type":"text","text":"Please","cache_control":null}
            ]}
        ]
    })
}

#[tokio::test]
async fn cache_hints_work_across_messages_aliases_counting_and_openai_http_modes() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ListAvailableModels"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"models":[{"modelId":"claude-opus-5"}]})),
        )
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(|request: &wiremock::Request| {
            let wire: Value = serde_json::from_slice(&request.body).unwrap();
            let serialized = wire.to_string();
            for hint in [
                "cache_control",
                "ephemeral",
                "scope",
                "evict_on_complete",
                "ttl",
            ] {
                assert!(
                    !serialized.contains(hint),
                    "{hint} leaked into {serialized}"
                );
            }
            assert!(
                serialized.contains(r#""cachePoint":{"type":"default"}"#),
                "{serialized}"
            );
            let assistant = wire["conversationState"]["history"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|turn| turn.get("assistantResponseMessage"))
                .find(|message| message["content"] == "Ready")
                .unwrap();
            assert!(assistant.get("cachePoint").is_none(), "{assistant}");
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(generation_body("pong"))
        })
        .expect(10)
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    let config_path = daemon.home().join("config.toml");
    let config = tokio::fs::read_to_string(&config_path)
        .await
        .unwrap()
        .replace("enable_prompt_cache = false", "enable_prompt_cache = true");
    tokio::fs::write(&config_path, config).await.unwrap();
    let reload = expect_ok(daemon.call("config.reload", json!({})).await);
    assert_eq!(reload["applied"], true, "{reload}");
    expect_ok(daemon.call("account.import", json!({"accounts":[{
        "id":"acc_66666666","email":"cache-test@example.com","machine_id":"6".repeat(64),
        "credentials":{"access_token":"synthetic-cache-test","region":"us-east-1",
            "expires_at":4_000_000_000i64,"auth_method":"idc"},
        "usage":{"current":0.0,"limit":100.0,"percent_used":0.0,"updated_at":1},"created_at":1
    }]})).await);
    expect_ok(daemon.call("models", json!({})).await);
    let client = reqwest::Client::new();
    for route in [
        "v1/messages",
        "messages",
        "anthropic/v1/messages",
        "v1/chat/completions",
        "chat/completions",
    ] {
        let claude = route.ends_with("messages");
        for stream in [false, true] {
            let mut body = request();
            body["stream"] = json!(stream);
            if !claude {
                let system = body.as_object_mut().unwrap().remove("system").unwrap();
                body["messages"]
                    .as_array_mut()
                    .unwrap()
                    .insert(0, json!({"role":"system","content":system}));
                body["tools"] = json!([{"type":"function","cache_control":{"type":"ephemeral","ttl":"1h","scope":"global"},
                    "function":{"name":"lookup","parameters":{"type":"object"}}}]);
            }
            let response = client
                .post(format!("http://127.0.0.1:{port}/{route}"))
                .header("x-api-key", daemon.api_key.as_deref().unwrap())
                .bearer_auth(daemon.api_key.as_deref().unwrap())
                .header("anthropic-version", "2023-06-01")
                .header(
                    "user-agent",
                    if claude {
                        "claude-cli/2.1.266 (external, cli)"
                    } else {
                        "codex_cli_rs/0.1.0"
                    },
                )
                .json(&body)
                .send()
                .await
                .unwrap();
            let status = response.status();
            let text = response.text().await.unwrap();
            assert_eq!(
                status,
                reqwest::StatusCode::OK,
                "{route} stream={stream}: {text}"
            );
            assert!(text.contains("pong"), "{text}");
        }
    }
    for prefix in ["v1/messages", "messages", "anthropic/v1/messages"] {
        let response = client
            .post(format!("http://127.0.0.1:{port}/{prefix}/count_tokens"))
            .header("x-api-key", daemon.api_key.as_deref().unwrap())
            .header("user-agent", "claude-cli/2.1.266 (external, cli)")
            .json(&request())
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        assert_eq!(status, reqwest::StatusCode::OK, "{body}");
        assert!(body["input_tokens"]
            .as_u64()
            .is_some_and(|tokens| tokens > 0));
    }
    let mut automatic = request();
    automatic["cache_control"] = json!({"type":"ephemeral","ttl":"1h"});
    automatic["messages"][2]["content"][0]["cache_control"] = Value::Null;
    automatic["messages"][2]["content"][1]["cache_control"] =
        json!({"type":"ephemeral","ttl":"1h"});
    let response = client
        .post(format!("http://127.0.0.1:{port}/v1/messages/count_tokens"))
        .header("x-api-key", daemon.api_key.as_deref().unwrap())
        .header("user-agent", "claude-cli/2.1.266 (external, cli)")
        .json(&automatic)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert_eq!(status, reqwest::StatusCode::OK, "{body}");
    assert!(body["input_tokens"]
        .as_u64()
        .is_some_and(|tokens| tokens > 0));

    let mut invalid_requests = Vec::new();
    for (control, field) in [
        (
            json!({"type":"default"}),
            "messages.1.content.0.cache_control.type",
        ),
        (json!({}), "messages.1.content.0.cache_control.type"),
        (
            json!({"type":null}),
            "messages.1.content.0.cache_control.type",
        ),
        (
            json!({"type":""}),
            "messages.1.content.0.cache_control.type",
        ),
        (
            json!({"type":42}),
            "messages.1.content.0.cache_control.type",
        ),
        (
            json!({"type":"ephemeral","ttl":"24h"}),
            "messages.1.content.0.cache_control.ttl",
        ),
        (
            json!({"type":"ephemeral","ttl":null}),
            "messages.1.content.0.cache_control.ttl",
        ),
    ] {
        let mut invalid = request();
        invalid["messages"][1]["content"][0]["cache_control"] = control;
        invalid_requests.push((invalid, field));
    }
    let mut invalid_order = request();
    invalid_order["tools"][0]["cache_control"]["ttl"] = json!("5m");
    invalid_requests.push((invalid_order, "system.0.cache_control.ttl"));

    automatic["messages"][2]["content"][1]["cache_control"]["ttl"] = json!("5m");
    invalid_requests.push((automatic, "messages.2.content.1.cache_control.ttl"));

    for (invalid, field) in invalid_requests {
        for stream in [false, true] {
            let mut invalid = invalid.clone();
            invalid["stream"] = json!(stream);
            let response = client
                .post(format!("http://127.0.0.1:{port}/v1/messages"))
                .header("x-api-key", daemon.api_key.as_deref().unwrap())
                .header("user-agent", "claude-cli/2.1.266 (external, cli)")
                .json(&invalid)
                .send()
                .await
                .unwrap();
            let status = response.status();
            let text = response.text().await.unwrap();
            let body: Value = if cfg!(feature = "crs") && stream {
                sse_error_payload(&text)
            } else {
                serde_json::from_str(&text).unwrap()
            };
            assert_eq!(
                status,
                if cfg!(feature = "crs") && stream {
                    reqwest::StatusCode::OK
                } else {
                    reqwest::StatusCode::BAD_REQUEST
                },
                "{invalid}: {body}"
            );
            assert_eq!(body["error"]["type"], "invalid_request_error", "{body}");
            assert!(
                body["error"]["message"].as_str().unwrap().contains(field),
                "{invalid}: {body}"
            );
        }
        let response = client
            .post(format!("http://127.0.0.1:{port}/v1/messages/count_tokens"))
            .header("x-api-key", daemon.api_key.as_deref().unwrap())
            .header("user-agent", "claude-cli/2.1.266 (external, cli)")
            .json(&invalid)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        assert_eq!(
            status,
            reqwest::StatusCode::BAD_REQUEST,
            "{invalid}: {body}"
        );
        assert_eq!(body["error"]["type"], "invalid_request_error", "{body}");
        assert!(
            body["error"]["message"].as_str().unwrap().contains(field),
            "{invalid}: {body}"
        );
    }
    mock.verify().await;
    daemon.stop().await;
}

#[tokio::test]
async fn local_cache_accounting_cold_warm_and_growing_usage_matches_all_protocols() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ListAvailableModels"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"models":[{"modelId":"claude-opus-5.5"}]})),
        )
        .mount(&mock)
        .await;
    let frames = [
        event_stream_frame("assistantResponseEvent", json!({"content":"pong"})),
        event_stream_frame("contextUsageEvent", json!({"contextUsagePercentage":3.0})),
        event_stream_frame("meteringEvent", json!({"unit":"credit","usage":0.05})),
    ]
    .concat();
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(frames),
        )
        .expect(18)
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    expect_ok(daemon.call("account.import", json!({"accounts":[{
        "id":"acc_77777777","email":"local-cache-test@example.com","machine_id":"7".repeat(64),
        "credentials":{"access_token":"synthetic-cache-test","region":"us-east-1","expires_at":4_000_000_000i64,"auth_method":"idc"},
        "usage":{"current":0.0,"limit":100.0,"percent_used":0.0,"updated_at":1},"created_at":1
    }]})).await);
    expect_ok(daemon.call("models", json!({})).await);
    let config_path = daemon.home().join("config.toml");
    let config = tokio::fs::read_to_string(&config_path)
        .await
        .unwrap()
        .replace("enable_prompt_cache = false", "enable_prompt_cache = true");
    tokio::fs::write(&config_path, config).await.unwrap();
    assert_eq!(
        expect_ok(daemon.call("config.reload", json!({})).await)["applied"],
        true
    );
    let client = reqwest::Client::new();
    for route in ["v1/messages", "v1/chat/completions", "v1/responses"] {
        for stream in [false, true] {
            let prefix = format!("{route}-{stream} stable library reference ").repeat(1200);
            let mut previous_write = 0;
            for turn in 0..3 {
                let mut messages = json!([{"role":"user","content":"first question"}]);
                if turn == 2 {
                    messages.as_array_mut().unwrap().extend([
                        json!({"role":"assistant","content":"pong"}),
                        json!({"role":"user","content":"next question"}),
                    ]);
                }
                let mut body = match route {
                    "v1/messages" => {
                        json!({"model":"claude-opus-5.5","max_tokens":64,"stream":stream,"system":[{"type":"text","text":prefix,"cache_control":{"type":"ephemeral"}}],"messages":messages})
                    }
                    "v1/chat/completions" => {
                        messages
                            .as_array_mut()
                            .unwrap()
                            .insert(0, json!({"role":"system","content":prefix}));
                        json!({"model":"claude-opus-5.5","max_tokens":64,"stream":stream,"stream_options":{"include_usage":true},"prompt_cache_key":format!("{route}-{stream}"),"messages":messages})
                    }
                    _ => {
                        json!({"model":"claude-opus-5.5","max_output_tokens":64,"stream":stream,"instructions":prefix,"input":messages,"prompt_cache_key":format!("{route}-{stream}")})
                    }
                };
                if !stream && route == "v1/chat/completions" {
                    body.as_object_mut().unwrap().remove("stream_options");
                }
                let response = client
                    .post(format!("http://127.0.0.1:{port}/{route}"))
                    .header("x-api-key", daemon.api_key.as_deref().unwrap())
                    .header(
                        "authorization",
                        format!("Bearer {}", daemon.api_key.as_deref().unwrap()),
                    )
                    .header("anthropic-version", "2023-06-01")
                    .header(
                        "user-agent",
                        if route == "v1/messages" {
                            "claude-cli/2.1.316 (external, e2e)"
                        } else {
                            "codex_cli_rs/0.1.0"
                        },
                    )
                    .json(&body)
                    .send()
                    .await
                    .unwrap();
                let status = response.status();
                let text = response.text().await.unwrap();
                assert_eq!(status, 200, "{route} stream={stream} turn={turn}: {text}");
                let events: Vec<Value> = text
                    .lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .filter_map(|line| serde_json::from_str(line).ok())
                    .collect();
                let value = if stream {
                    Value::Null
                } else {
                    serde_json::from_str::<Value>(&text).unwrap()
                };
                let usage = if !stream {
                    value["usage"].clone()
                } else {
                    match route {
                        "v1/messages" => events
                            .iter()
                            .find(|e| e["type"] == "message_delta")
                            .unwrap()["usage"]
                            .clone(),
                        "v1/chat/completions" => events
                            .iter()
                            .find(|e| e.get("usage").is_some_and(Value::is_object))
                            .unwrap()["usage"]
                            .clone(),
                        _ => events
                            .iter()
                            .find(|e| e["type"] == "response.completed")
                            .unwrap()["response"]["usage"]
                            .clone(),
                    }
                };
                let (total, read, write) = match route {
                    "v1/messages" => {
                        let read = usage["cache_read_input_tokens"].as_u64().unwrap();
                        let write = usage["cache_creation_input_tokens"].as_u64().unwrap();
                        if stream {
                            let start = &events
                                .iter()
                                .find(|e| e["type"] == "message_start")
                                .unwrap()["message"]["usage"];
                            assert_eq!(start["cache_read_input_tokens"], read);
                            assert_eq!(start["cache_creation_input_tokens"], write);
                        }
                        (
                            usage["input_tokens"].as_u64().unwrap() + read + write,
                            read,
                            write,
                        )
                    }
                    "v1/chat/completions" => (
                        usage["prompt_tokens"].as_u64().unwrap(),
                        usage["prompt_tokens_details"]["cached_tokens"]
                            .as_u64()
                            .unwrap(),
                        usage["prompt_tokens_details"]["cache_write_tokens"]
                            .as_u64()
                            .unwrap(),
                    ),
                    _ => (
                        usage["input_tokens"].as_u64().unwrap(),
                        usage["input_tokens_details"]["cached_tokens"]
                            .as_u64()
                            .unwrap(),
                        usage["input_tokens_details"]["cache_write_tokens"]
                            .as_u64()
                            .unwrap(),
                    ),
                };
                assert!(read + write <= total, "{route} {stream} {turn}: {usage}");
                if turn == 0 {
                    assert_eq!(read, 0);
                    assert!(write > 512, "{route}: {usage}");
                    previous_write = write;
                } else {
                    assert_eq!(read, previous_write, "{route} {stream} {turn}: {usage}");
                    if turn == 1 {
                        assert_eq!(write, 0);
                    }
                }
                let logs = expect_ok(
                    daemon
                        .call(crosslane_ipc::protocol::method::LOGS, json!({"tail":1}))
                        .await,
                );
                let log = &logs["entries"][0];
                assert_eq!(log["input_tokens"], total, "{route}: {logs}");
                assert_eq!(log["diagnostics"]["token_usage_source"], "estimated");
                assert_eq!(log["diagnostics"]["cache_usage_source"], "estimated");
                assert_eq!(log["diagnostics"]["credits_source"], "server");
                assert_eq!(log["diagnostics"]["cache_read_tokens"], read);
                assert_eq!(log["diagnostics"]["cache_write_tokens"], write);
            }
        }
    }
}

#[tokio::test]
async fn failed_stream_does_not_seed_local_cache_entries() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ListAvailableModels"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"models":[{"modelId":"claude-opus-5.5"}]})),
        )
        .mount(&mock)
        .await;
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(move |_: &wiremock::Request| {
            let mut body = event_stream_frame("assistantResponseEvent", json!({"content":"pong"}));
            if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                body.extend_from_slice(b"broken");
            }
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(body)
        })
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    expect_ok(daemon.call("account.import",json!({"accounts":[{
        "id":"acc_88888888","email":"failed-cache-test@example.com","machine_id":"8".repeat(64),
        "credentials":{"access_token":"synthetic-cache-test","region":"us-east-1","expires_at":4_000_000_000i64,"auth_method":"idc"},
        "usage":{"current":0.0,"limit":100.0,"percent_used":0.0,"updated_at":1},"created_at":1
    }]})).await);
    expect_ok(daemon.call("models", json!({})).await);
    let path = daemon.home().join("config.toml");
    let mut config: Config =
        toml::from_str(&tokio::fs::read_to_string(&path).await.unwrap()).unwrap();
    config.features.enable_prompt_cache = true;
    config.upstream.max_retries = 0;
    tokio::fs::write(&path, toml::to_string_pretty(&config).unwrap())
        .await
        .unwrap();
    assert_eq!(
        expect_ok(daemon.call("config.reload", json!({})).await)["applied"],
        true
    );
    let client = reqwest::Client::new();
    let body = json!({"model":"claude-opus-5.5","max_tokens":64,"stream":true,"system":[{"type":"text","text":"failure must not warm this cache ".repeat(1200),"cache_control":{"type":"ephemeral"}}],"messages":[{"role":"user","content":"question"}]});
    for turn in 0..2 {
        let response = client
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("x-api-key", daemon.api_key.as_deref().unwrap())
            .header("anthropic-version", "2023-06-01")
            .header("user-agent", "claude-cli/2.1.316 (external, e2e)")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let text = response.text().await.unwrap();
        let events: Vec<Value> = text
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        if turn == 0 {
            assert!(
                events.iter().any(|event| event["type"] == "error"),
                "{text}"
            );
        } else {
            let usage = &events
                .iter()
                .find(|event| event["type"] == "message_delta")
                .unwrap()["usage"];
            assert_eq!(usage["cache_read_input_tokens"], 0);
            assert!(
                usage["cache_creation_input_tokens"].as_u64().unwrap() > 512,
                "{usage}"
            );
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn stop_does_not_seed_cache_in_either_response_mode() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ListAvailableModels"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"models":[{"modelId":"claude-opus-5.5"}]})),
        )
        .mount(&mock)
        .await;
    let frames = [
        event_stream_frame(
            "assistantResponseEvent",
            json!({"content":"before <STOP> unconsumed response"}),
        ),
        event_stream_frame("meteringEvent", json!({"unit":"credit","usage":0.05})),
    ]
    .concat();
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(frames),
        )
        .expect(4)
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    expect_ok(daemon.call("account.import",json!({"accounts":[{
        "id":"acc_77777777","email":"review@example.com","machine_id":"7".repeat(64),
        "credentials":{"access_token":"synthetic-review","region":"us-east-1","expires_at":4_000_000_000i64,"auth_method":"idc"},
        "usage":{"current":0.0,"limit":100.0,"percent_used":0.0,"updated_at":1},"created_at":1
    }]})).await);
    expect_ok(daemon.call("models", json!({})).await);
    let path = daemon.home().join("config.toml");
    let config = tokio::fs::read_to_string(&path)
        .await
        .unwrap()
        .replace("enable_prompt_cache = false", "enable_prompt_cache = true");
    tokio::fs::write(&path, config).await.unwrap();
    assert_eq!(
        expect_ok(daemon.call("config.reload", json!({})).await)["applied"],
        true
    );
    let mut request = json!({
        "model":"claude-opus-5.5","max_tokens":64,"stream":false,"stop_sequences":["<STOP>"],
        "system":[{"type":"text","text":"stable reference ".repeat(1000),"cache_control":{"type":"ephemeral"}}],
        "messages":[{"role":"user","content":"stop at delimiter"}]
    });
    let client = reqwest::Client::new();
    let mut bodies = Vec::new();
    for stream in [false, true] {
        request["stream"] = json!(stream);
        for _ in 0..2 {
            let response = client
                .post(format!("http://127.0.0.1:{port}/v1/messages"))
                .header("x-api-key", daemon.api_key.as_deref().unwrap())
                .header("anthropic-version", "2023-06-01")
                .header("user-agent", "claude-cli/2.1.318 (external, review)")
                .json(&request)
                .send()
                .await
                .unwrap();
            let status = response.status();
            let text = response.text().await.unwrap();
            assert_eq!(status, reqwest::StatusCode::OK, "response={text}");
            let body = if stream {
                let delta = text
                    .lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    .find(|event| event["type"] == "message_delta")
                    .unwrap();
                json!({"stop_reason":delta["delta"]["stop_reason"],"usage":delta["usage"]})
            } else {
                serde_json::from_str::<Value>(&text).unwrap()
            };
            assert_eq!(body["stop_reason"], "stop_sequence");
            assert_eq!(
                body["usage"]["cache_read_input_tokens"], 0,
                "stop must not publish: {body}"
            );
            bodies.push(body);
        }
    }
    daemon.stop().await;
    assert_eq!(bodies[0]["stop_reason"], "stop_sequence");
    assert_eq!(
        bodies[1]["usage"]["cache_read_input_tokens"],
        0,
        "a response cut at a stop sequence must not seed the next request: {:?}",
        bodies.iter().map(|b| &b["usage"]).collect::<Vec<_>>()
    );
}

/// Exercise the independent rollout switches through the public HTTP path,
/// including a hot reload while the accepted request is awaiting its body.
#[tokio::test]
async fn accounting_switch_matrix_and_inflight_zero_policy_are_frozen() {
    use crosslane_core::config::{KiroUsageZeroPolicy, PromptCacheAccounting};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ListAvailableModels"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"models":[{"modelId":"claude-opus-5.5"}]})),
        )
        .mount(&mock)
        .await;
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    Mock::given(method("POST")).and(path("/generateAssistantResponse"))
        .respond_with(move |_: &wiremock::Request| {
            seen.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_delay(Duration::from_millis(400))
                .set_body_bytes([
                    event_stream_frame("assistantResponseEvent", json!({"content":"pong"})),
                    event_stream_frame("messageMetadataEvent", json!({"tokenUsage":{"inputTokens":0,"outputTokens":0,"cacheReadInputTokens":0,"cacheWriteInputTokens":0}})),
                ].concat())
        }).mount(&mock).await;
    let port = unused_tcp_port();
    let daemon = Daemon::start_http_with_log_level(
        port,
        &format!("{}/generateAssistantResponse", mock.uri()),
        "info",
    )
    .await;
    expect_ok(daemon.call("account.import", json!({"accounts":[{
        "id":"acc_77777777","email":"switches@example.com","machine_id":"7".repeat(64),
        "credentials":{"access_token":"synthetic-switches","region":"us-east-1","expires_at":4_000_000_000i64,"auth_method":"idc"},
        "usage":{"current":0.0,"limit":100.0,"percent_used":0.0,"updated_at":1},"created_at":1
    }]})).await);
    expect_ok(daemon.call("models", json!({})).await);
    let config_path = daemon.home().join("config.toml");
    let mut config: Config =
        toml::from_str(&tokio::fs::read_to_string(&config_path).await.unwrap()).unwrap();
    let client = reqwest::Client::new();
    let cases = [
        (
            true,
            PromptCacheAccounting::V3,
            KiroUsageZeroPolicy::Fallback,
        ),
        (true, PromptCacheAccounting::V3, KiroUsageZeroPolicy::Legacy),
        (
            true,
            PromptCacheAccounting::Legacy,
            KiroUsageZeroPolicy::Fallback,
        ),
        (
            true,
            PromptCacheAccounting::Legacy,
            KiroUsageZeroPolicy::Legacy,
        ),
        (
            false,
            PromptCacheAccounting::V3,
            KiroUsageZeroPolicy::Fallback,
        ),
    ];
    for (case, (enabled, accounting, zero)) in cases.into_iter().enumerate() {
        for stream in [false, true] {
            config.features.enable_prompt_cache = enabled;
            config.features.prompt_cache_accounting = accounting;
            config.features.kiro_usage_zero_policy = zero;
            tokio::fs::write(&config_path, toml::to_string_pretty(&config).unwrap())
                .await
                .unwrap();
            assert_eq!(
                expect_ok(daemon.call("config.reload", json!({})).await)["applied"],
                true
            );
            let before = calls.load(Ordering::SeqCst);
            let request = json!({"model":"claude-opus-5.5","max_tokens":64,"stream":stream,
                "system":[{"type":"text","text":format!("switch case {case} stream {stream} stable reference ").repeat(240),"cache_control":{"type":"ephemeral"}}],
                "messages":[{"role":"user","content":"hello"}]});
            let pending = tokio::spawn(
                client
                    .post(format!("http://127.0.0.1:{port}/v1/messages"))
                    .header("x-api-key", daemon.api_key.as_deref().unwrap())
                    .header("anthropic-version", "2023-06-01")
                    .header("user-agent", "claude-cli/2.1.318 (external, e2e)")
                    .json(&request)
                    .send(),
            );
            tokio::time::timeout(Duration::from_secs(30), async {
                // CRS may return the downstream SSE headers before dispatch;
                // completion of send() is not evidence that upstream ran.
                while calls.load(Ordering::SeqCst) == before {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            // A policy reload cannot alter an already-admitted request.
            config.features.kiro_usage_zero_policy = if zero == KiroUsageZeroPolicy::Fallback {
                KiroUsageZeroPolicy::Legacy
            } else {
                KiroUsageZeroPolicy::Fallback
            };
            config.features.enable_prompt_cache = !enabled;
            config.features.prompt_cache_accounting = if accounting == PromptCacheAccounting::V3 {
                PromptCacheAccounting::Legacy
            } else {
                PromptCacheAccounting::V3
            };
            tokio::fs::write(&config_path, toml::to_string_pretty(&config).unwrap())
                .await
                .unwrap();
            assert_eq!(
                expect_ok(daemon.call("config.reload", json!({})).await)["applied"],
                true
            );
            let response = pending.await.unwrap().unwrap();
            let status = response.status();
            let text = response.text().await.unwrap();
            assert_eq!(status, 200, "{text}");
            let usage = if stream {
                text.lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    .find(|event| event["type"] == "message_delta")
                    .unwrap()["usage"]
                    .clone()
            } else {
                serde_json::from_str::<Value>(&text).unwrap()["usage"].clone()
            };
            let read = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
            let write = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            let input = usage["input_tokens"].as_u64().unwrap() + read + write;
            if zero == KiroUsageZeroPolicy::Fallback {
                assert!(
                    input > 512 && usage["output_tokens"].as_u64().unwrap() > 0,
                    "case {case} stream={stream}: {usage}"
                );
                if enabled {
                    if write <= 512 {
                        let files = expect_ok(daemon.call("logs.files", json!({})).await);
                        for file in files["files"].as_array().into_iter().flatten() {
                            if let Some(path) = file["path"].as_str() {
                                let text =
                                    tokio::fs::read_to_string(path).await.unwrap_or_default();
                                for line in
                                    text.lines().filter(|line| line.contains("prompt_cache."))
                                {
                                    eprintln!("{line}");
                                }
                            }
                        }
                    }
                    assert!(write > 512, "case {case} stream={stream}: {usage}");
                } else {
                    assert_eq!(read + write, 0);
                }
            } else {
                assert_eq!(input, 0, "case {case}: {usage}");
                assert_eq!(usage["output_tokens"], 0);
            }
        }
    }
    daemon.stop().await;
}
