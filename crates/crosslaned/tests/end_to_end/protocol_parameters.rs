use super::*;
use serde_json::{json, Value};

async fn start(mock: &MockServer) -> (Daemon, u16) {
    start_with_models(mock, json!({"models":[{"modelId":"claude-haiku-4.5","tokenLimits":{"maxInputTokens":1000,"maxOutputTokens":16384}}]})).await
}

async fn start_with_models(mock: &MockServer, models: Value) -> (Daemon, u16) {
    Mock::given(method("GET"))
        .and(path("/ListAvailableModels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(models))
        .mount(mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    expect_ok(daemon.call("account.import", json!({"accounts":[{
        "id":"acc_66666666","email":"parameter-test@example.com","machine_id":"6".repeat(64),
        "credentials":{"access_token":"synthetic-parameter-test","region":"us-east-1",
            "expires_at":4_000_000_000i64,"auth_method":"idc"},
        "usage":{"current":0.0,"limit":100.0,"percent_used":0.0,"updated_at":1},"created_at":1
    }]})).await);
    expect_ok(daemon.call("models", json!({})).await);
    (daemon, port)
}

async fn post(daemon: &Daemon, port: u16, route: &str, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/{route}"))
        .bearer_auth(daemon.api_key.as_deref().unwrap())
        .header("x-api-key", daemon.api_key.as_deref().unwrap())
        .header("anthropic-version", "2023-06-01")
        .header(
            "user-agent",
            if route.contains("messages") {
                "claude-cli/2.1.266 (external, cli)"
            } else {
                "codex_cli_rs/0.1.0"
            },
        )
        .json(&body)
        .send()
        .await
        .unwrap()
}

fn stream_values(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).unwrap())
        .collect()
}

#[tokio::test]
async fn chat_stop_candidates_and_completion_limit_apply_to_json_and_sse() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(|request: &wiremock::Request| {
            let wire: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(wire["inferenceConfig"]["maxTokens"], 64, "{wire}");
            let mut bytes =
                event_stream_frame("assistantResponseEvent", json!({"content":"BEFORE C"}));
            bytes.extend(generation_body("UT AFTER"));
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(bytes)
        })
        .expect(6)
        .mount(&mock)
        .await;
    let (daemon, port) = start(&mock).await;
    for streaming in [false, true] {
        for count in [1, 2] {
            let response = post(&daemon, port, "v1/chat/completions", json!({
                "model":"claude-haiku-4.5","messages":[{"role":"user","content":"Reply BEFORE CUT AFTER"}],
                "stream":streaming,"stream_options":{"include_usage":true},"n":count,"stop":" CUT",
                "max_tokens":256,"max_completion_tokens":64,"reasoning_effort":"none"
            })).await;
            let status = response.status();
            let text = response.text().await.unwrap();
            assert_eq!(status, 200, "{text}");
            if streaming {
                assert_eq!(text.matches("data: [DONE]").count(), 1, "{text}");
                let values = stream_values(&text);
                assert!(
                    values.iter().all(|value| value.get("error").is_none()),
                    "{text}"
                );
                let usage = values
                    .iter()
                    .filter(|value| value["choices"] == json!([]))
                    .collect::<Vec<_>>();
                assert_eq!(usage.len(), 1, "{text}");
                assert!(
                    values
                        .iter()
                        .filter(|value| value["choices"] != json!([]))
                        .all(|value| value.get("usage") == Some(&Value::Null)),
                    "{text}"
                );
                assert!(usage[0]["usage"]["total_tokens"].as_u64().unwrap() > 0);
                for index in 0..count {
                    let choices = values
                        .iter()
                        .filter_map(|value| value["choices"].as_array())
                        .flatten()
                        .filter(|choice| choice["index"] == index)
                        .collect::<Vec<_>>();
                    let output = choices
                        .iter()
                        .filter_map(|choice| choice["delta"]["content"].as_str())
                        .collect::<String>();
                    assert_eq!(output, "BEFORE", "{text}");
                    assert_eq!(
                        choices
                            .iter()
                            .filter(|choice| choice["finish_reason"] == "stop")
                            .count(),
                        1,
                        "{text}"
                    );
                }
                assert!(values.iter().all(|value| value["id"] == values[0]["id"]));
            } else {
                let value: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(value["choices"].as_array().unwrap().len(), count as usize);
                for (index, choice) in value["choices"].as_array().unwrap().iter().enumerate() {
                    assert_eq!(choice["index"], index);
                    assert_eq!(choice["message"]["content"], "BEFORE");
                    assert_eq!(choice["finish_reason"], "stop");
                }
                assert!(value["usage"]["total_tokens"].as_u64().unwrap() > 0);
            }
        }
    }
    daemon.stop().await;
}

#[tokio::test]
async fn legacy_function_responses_keep_the_deprecated_wire_contract() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(event_stream_frame(
                    "toolUseEvent",
                    json!({"toolUseId":"tooluse_example","name":"lookup","input":"{}","stop":true}),
                )),
        )
        .expect(2)
        .mount(&mock)
        .await;
    let (daemon, port) = start(&mock).await;
    for stream in [false, true] {
        let response = post(&daemon, port, "v1/chat/completions", json!({"model":"claude-haiku-4.5","max_tokens":128,
            "stream":stream,"functions":[{"name":"lookup","parameters":{"type":"object"}}],"function_call":{"name":"lookup"},
            "messages":[{"role":"user","content":"Look up the value"}]
        })).await;
        let status = response.status();
        let text = response.text().await.unwrap();
        assert_eq!(status, 200, "{text}");
        let values = if stream {
            stream_values(&text)
        } else {
            vec![serde_json::from_str(&text).unwrap()]
        };
        assert!(
            values
                .iter()
                .any(|value| value["choices"][0]["finish_reason"] == "function_call"),
            "{text}"
        );
        assert!(
            values.iter().any(|value| value["choices"][0]
                [if stream { "delta" } else { "message" }]["function_call"]["name"]
                == "lookup"),
            "{text}"
        );
        assert!(!text.contains("tool_calls"), "{text}");
    }
    daemon.stop().await;
}

#[tokio::test]
async fn streaming_candidates_release_the_previous_accounting_slot() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(generation_body("OK")),
        )
        .expect(3)
        .mount(&mock)
        .await;
    let (daemon, port) = start(&mock).await;
    let config_path = daemon.home().join("config.toml");
    let raw = tokio::fs::read_to_string(&config_path).await.unwrap();
    let mut config: crosslane_core::config::Config = toml::from_str(&raw).unwrap();
    config.server.max_concurrent_requests = 1;
    config.server.max_connections = 1;
    config.pool.max_concurrent_per_account = 1;
    tokio::fs::write(&config_path, toml::to_string(&config).unwrap())
        .await
        .unwrap();
    assert_eq!(
        expect_ok(daemon.call("config.reload", json!({})).await)["applied"],
        true
    );
    let response = post(
        &daemon,
        port,
        "v1/chat/completions",
        json!({
            "model":"claude-haiku-4.5", "messages":[{"role":"user","content":"Reply OK"}],
            "n":3, "stream":true, "stream_options":{"include_usage":true}
        }),
    )
    .await;
    assert_eq!(response.status(), 200);
    let text = tokio::time::timeout(Duration::from_secs(10), response.text())
        .await
        .unwrap()
        .unwrap();
    let values = stream_values(&text);
    assert!(
        values.iter().all(|value| value.get("error").is_none()),
        "{text}"
    );
    for index in 0..3 {
        assert!(
            values
                .iter()
                .any(|value| value["choices"][0]["index"] == index
                    && value["choices"][0]["finish_reason"] == "stop"),
            "{text}"
        );
    }
    assert_eq!(text.matches("data: [DONE]").count(), 1);
    let usage = values
        .iter()
        .find(|value| value["choices"] == json!([]))
        .unwrap();
    assert_eq!(usage["usage"]["prompt_tokens"], 300, "{text}");
    assert_eq!(usage["usage"]["completion_tokens"], 60, "{text}");
    assert_eq!(usage["usage"]["total_tokens"], 360, "{text}");
    let stats = expect_ok(
        daemon
            .call("stats", json!({"detail":true,"recent":10}))
            .await,
    );
    let requests = stats["stats"]["recent_requests"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|request| request["path"] == "/v1/chat/completions")
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 3, "{stats}");
    assert!(
        requests
            .iter()
            .all(|request| request["status"] == 200 && request["credits"] == 0.25),
        "{stats}"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn streaming_candidate_failure_is_recorded_and_stops_further_calls() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(move |_: &wiremock::Request| {
            if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/vnd.amazon.eventstream")
                    .set_body_bytes(generation_body("OK"))
            } else {
                ResponseTemplate::new(400)
                    .set_body_json(json!({"message":"invalid synthetic request"}))
            }
        })
        .mount(&mock)
        .await;
    let (daemon, port) = start(&mock).await;
    let response = post(&daemon, port, "v1/chat/completions", json!({
        "model":"claude-haiku-4.5","messages":[{"role":"user","content":"Reply OK"}],"n":3,"stream":true
    })).await;
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    let values = stream_values(&text);
    assert_eq!(
        values
            .iter()
            .filter(|value| value.get("error").is_some())
            .count(),
        1,
        "{text}"
    );
    assert_eq!(text.matches("data: [DONE]").count(), 1, "{text}");
    assert!(
        values.iter().all(|value| value.get("usage").is_none()),
        "{text}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "no third candidate after failure"
    );
    let stats = expect_ok(
        daemon
            .call("stats", json!({"detail":true,"recent":10}))
            .await,
    );
    let requests = stats["stats"]["recent_requests"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|request| request["path"] == "/v1/chat/completions")
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 2, "{stats}");
    assert!(
        requests.iter().any(|request| request["status"] == 400
            && request["diagnostics"]["upstream_status"] == 400
            && request["diagnostics"]["client_status"] == 200),
        "{stats}"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn native_nullable_fields_and_inline_files_pass_http_validation() {
    use base64::Engine as _;
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(generation_body("OK")),
        )
        .expect(3)
        .mount(&mock)
        .await;
    let (daemon, port) = start(&mock).await;
    for route in [
        "v1/messages/count_tokens",
        "messages/count_tokens",
        "anthropic/v1/messages/count_tokens",
    ] {
        let response = post(&daemon, port, route, json!({"model":"claude-haiku-4.5","thinking":{"type":"enabled","budget_tokens":1024},"messages":[{"role":"user","content":"Hello"}]})).await;
        let status = response.status();
        let text = response.text().await.unwrap();
        assert_eq!(status, 200, "{route}: {text}");
    }
    let data = base64::engine::general_purpose::STANDARD.encode("The answer is cobalt.");
    for (route, body) in [
        (
            "v1/chat/completions",
            json!({"model":"claude-haiku-4.5","stream":null,"tools":[{"type":"function","function":{"name":"lookup","strict":null}}],
            "messages":[{"role":"assistant","content":null,"refusal":"Cannot perform this action."},{"role":"user","content":[{"type":"file","file":{"filename":"evidence.txt","file_data":data}}]}]}),
        ),
        (
            "v1/responses",
            json!({"model":"claude-haiku-4.5","input":[{"role":"user","content":[{"type":"input_file","filename":"evidence.txt","file_data":data}]}]}),
        ),
        (
            "v1/messages",
            json!({"model":"claude-haiku-4.5","max_tokens":128,"messages":[{"role":"user","content":[{"type":"document","title":"","citations":null,"source":{"type":"text","media_type":"text/plain","data":"The answer is cobalt."}}]}]}),
        ),
    ] {
        let response = post(&daemon, port, route, body).await;
        let status = response.status();
        let text = response.text().await.unwrap();
        assert_eq!(status, 200, "{route}: {text}");
    }
    for request in mock
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/generateAssistantResponse")
    {
        let wire: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(
            wire["conversationState"]["currentMessage"]["userInputMessage"]["documents"][0]
                ["source"]["bytes"],
            data,
            "{wire}"
        );
    }
    daemon.stop().await;
}

#[tokio::test]
async fn responses_auto_truncation_uses_the_resolved_model_window() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(|request: &wiremock::Request| {
            let wire: Value = serde_json::from_slice(&request.body).unwrap();
            assert!(
                !wire.to_string().contains("Old discarded evidence."),
                "{wire}"
            );
            assert!(
                wire.to_string().contains("Keep these instructions."),
                "{wire}"
            );
            assert!(wire.to_string().contains("Reply OK."), "{wire}");
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(generation_body("OK"))
        })
        .expect(2)
        .mount(&mock)
        .await;
    let (daemon, port) = start(&mock).await;
    for (truncation, stream) in [("disabled", false), ("auto", false), ("auto", true)] {
        let response = post(&daemon, port, "v1/responses", json!({"model":"claude-haiku-4.5", "max_output_tokens":128,
            "truncation":truncation, "max_tool_calls":1, "stream":stream, "instructions":"Keep these instructions.","input":[
                {"role":"user","content":"Old discarded evidence. ".repeat(1000)},
                {"role":"assistant","content":"Acknowledged."},
                {"role":"user","content":"Reply OK."}
            ]})).await;
        let status = response.status();
        let text = response.text().await.unwrap();
        assert_eq!(
            status,
            if truncation == "auto" { 200 } else { 400 },
            "{text}"
        );
        if truncation == "auto" {
            let value: Value = if stream {
                stream_values(&text)
                    .into_iter()
                    .find(|value| value["type"] == "response.completed")
                    .unwrap()["response"]
                    .clone()
            } else {
                serde_json::from_str(&text).unwrap()
            };
            assert_eq!(value["truncation"], "auto");
            assert_eq!(value["max_tool_calls"], 1);
        }
    }
    daemon.stop().await;
}

#[tokio::test]
async fn responses_auto_truncation_rechecks_a_smaller_fallback_model() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    let (daemon, port) = start_with_models(
        &mock,
        json!({"models":[
            {"modelId":"claude-haiku-4.5","tokenLimits":{"maxInputTokens":32000}},
            {"modelId":"claude-haiku-3.5","tokenLimits":{"maxInputTokens":1000}}
        ]}),
    )
    .await;
    let config_path = daemon.home().join("config.toml");
    let raw = tokio::fs::read_to_string(&config_path).await.unwrap();
    let mut config: crosslane_core::config::Config = toml::from_str(&raw).unwrap();
    config.upstream.max_retries = 1;
    config.features.enable_model_fallback = true;
    tokio::fs::write(&config_path, toml::to_string(&config).unwrap())
        .await
        .unwrap();
    expect_ok(daemon.call("config.reload", json!({})).await);
    let mode = Arc::new(AtomicUsize::new(0));
    let response_mode = mode.clone();
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(move |request: &wiremock::Request| {
            let wire: Value = serde_json::from_slice(&request.body).unwrap();
            let model = wire["conversationState"]["currentMessage"]["userInputMessage"]["modelId"]
                .as_str()
                .unwrap();
            if model == "claude-haiku-4.5" {
                assert!(wire.to_string().contains("Old discarded evidence."));
                if response_mode.load(Ordering::SeqCst) == 0 {
                    return ResponseTemplate::new(429)
                        .set_body_json(json!({"message":"model capacity exceeded"}));
                }
                return ResponseTemplate::new(200)
                    .insert_header("content-type", "application/vnd.amazon.eventstream")
                    .set_body_bytes(event_stream_frame(
                        "throttlingException",
                        json!({"__type":"ThrottlingException","message":"model capacity exceeded"}),
                    ));
            }
            assert_eq!(model, "claude-haiku-3.5");
            assert!(!wire.to_string().contains("Old discarded evidence."));
            assert!(wire.to_string().contains("Keep these instructions."));
            assert!(wire.to_string().contains("Reply OK."));
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(generation_body("OK"))
        })
        .mount(&mock)
        .await;
    for (stream, error_mode) in [(false, 0), (true, 0), (true, 1)] {
        mode.store(error_mode, Ordering::SeqCst);
        let response = post(
            &daemon,
            port,
            "v1/responses",
            json!({"model":"claude-haiku-4.5",
            "max_output_tokens":128,"stream":stream,"store":false,"truncation":"auto",
            "instructions":"Keep these instructions.","input":[
                {"role":"user","content":"Old discarded evidence. ".repeat(1000)},
                {"role":"assistant","content":"Acknowledged."},
                {"role":"user","content":"Reply OK."}
            ]}),
        )
        .await;
        let status = response.status();
        let text = response.text().await.unwrap();
        assert_eq!(status, 200, "{text}");
        if stream {
            assert!(
                stream_values(&text)
                    .iter()
                    .any(|value| value["type"] == "response.completed"),
                "{text}"
            );
            assert!(!text.contains("response.failed"), "{text}");
        } else {
            assert_eq!(
                serde_json::from_str::<Value>(&text).unwrap()["status"],
                "completed"
            );
        }
    }
    let requests = mock.received_requests().await.unwrap();
    let models = requests
        .iter()
        .filter(|request| request.url.path() == "/generateAssistantResponse")
        .map(|request| {
            serde_json::from_slice::<Value>(&request.body).unwrap()["conversationState"]
                ["currentMessage"]["userInputMessage"]["modelId"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        models
            .iter()
            .filter(|model| *model == "claude-haiku-3.5")
            .count(),
        3,
        "{models:?}"
    );
    daemon.stop().await;
}
