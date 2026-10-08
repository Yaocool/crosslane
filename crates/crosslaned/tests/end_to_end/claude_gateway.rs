use super::*;
use serde_json::{json, Value};

#[tokio::test]
async fn xml_tool_recovery_keeps_reasoning_and_string_parameters_safe_in_http_modes() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    let text = concat!(
        "<thinking>\n<tool_use name=\"write_file\">{\"content\":\"hidden example\"}</tool_use>\n</thinking>\n",
        "<function_calls><invoke name=\"write_file\">",
        "<parameter name=\"content\">  true\n</parameter>",
        "<parameter name=\"overwrite\">true</parameter></invoke></function_calls>"
    );
    let upstream_body: Vec<u8> = text
        .chars()
        .flat_map(|character| {
            event_stream_frame(
                "assistantResponseEvent",
                json!({"content":character.to_string()}),
            )
        })
        .collect();
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(upstream_body),
        )
        .expect(8)
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    assert_eq!(expect_ok(daemon.call("account.import", json!({"accounts":[{
        "id":"acc_77777778", "email":"tool-test@example.com", "machine_id":"7".repeat(64),
        "credentials":{"access_token":"ksk_e2e-tools", "region":"us-east-1",
            "expires_at":0, "auth_method":"api_key"}, "created_at":1
    }]})).await)["imported"], 1);
    let config_path = daemon.home().join("config.toml");
    let mut config: crosslane_core::config::Config =
        toml::from_str(&tokio::fs::read_to_string(&config_path).await.unwrap()).unwrap();
    config.features.tool_call_buffer_delay_ms = 0;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    for buffered in [false, true] {
        config.features.buffer_tool_calls = buffered;
        tokio::fs::write(&config_path, toml::to_string(&config).unwrap())
            .await
            .unwrap();
        assert_eq!(
            expect_ok(daemon.call("config.reload", json!({})).await)["applied"],
            true
        );
        for thinking in [false, true] {
            for stream in [false, true] {
                let mut request = json!({
                    "model":"claude-sonnet-4.5", "max_tokens":2048, "stream":stream,
                    "messages":[{"role":"user","content":"Write the requested file."}],
                    "tools":[{"name":"write_file", "input_schema":{"type":"object","$defs":{"text":{"type":"string"}},"properties":{
                        "content":{"$ref":"#/$defs/text"}, "overwrite":{"type":"boolean"}
                    }}}]
                });
                request["thinking"] = if thinking {
                    json!({"type":"enabled","budget_tokens":1024})
                } else {
                    json!({"type":"disabled"})
                };
                let response = client
                    .post(format!("http://127.0.0.1:{port}/v1/messages"))
                    .header("x-api-key", daemon.api_key.as_deref().unwrap())
                    .header("user-agent", "claude-cli/2.1.235 (external, e2e)")
                    .header("anthropic-version", "2023-06-01")
                    .json(&request)
                    .send()
                    .await
                    .unwrap();
                let status = response.status();
                let body = response.text().await.unwrap();
                let context =
                    format!("buffered={buffered}, thinking={thinking}, stream={stream}: {body}");
                assert_eq!(status, reqwest::StatusCode::OK, "{context}");
                let input = if stream {
                    let events = body
                        .lines()
                        .filter_map(|line| line.strip_prefix("data: "))
                        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                        .collect::<Vec<_>>();
                    let tools = events
                        .iter()
                        .filter(|event| event["content_block"]["type"] == "tool_use")
                        .collect::<Vec<_>>();
                    assert_eq!(tools.len(), 1, "{context}");
                    assert_eq!(tools[0]["content_block"]["name"], "write_file", "{context}");
                    let index = &tools[0]["index"];
                    let arguments = events
                        .iter()
                        .filter(|event| &event["index"] == index)
                        .filter_map(|event| {
                            event.pointer("/delta/partial_json").and_then(Value::as_str)
                        })
                        .collect::<String>();
                    assert!(
                        events.iter().any(|event| event["type"] == "message_stop"),
                        "{context}"
                    );
                    assert!(
                        events
                            .iter()
                            .any(|event| event["delta"]["stop_reason"] == "tool_use"),
                        "{context}"
                    );
                    serde_json::from_str::<Value>(&arguments).expect(&context)
                } else {
                    let response: Value = serde_json::from_str(&body).unwrap();
                    assert_eq!(response["stop_reason"], "tool_use", "{context}");
                    let tools = response["content"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|block| block["type"] == "tool_use")
                        .collect::<Vec<_>>();
                    assert_eq!(tools.len(), 1, "{context}");
                    assert_eq!(tools[0]["name"], "write_file", "{context}");
                    tools[0]["input"].clone()
                };
                assert_eq!(
                    input,
                    json!({"content":"  true\n","overwrite":true}),
                    "{context}"
                );
            }
        }
    }
    daemon.stop().await;
}

#[tokio::test]
async fn claude_discovery_alias_and_client_env_work_with_headless_api_keys() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(|request: &wiremock::Request| {
            assert_eq!(request.headers["tokentype"], "API_KEY");
            assert_eq!(request.headers["authorization"], "Bearer ksk_e2e-synthetic");
            let wire: Value = serde_json::from_slice(&request.body).unwrap();
            assert!(wire.get("profileArn").is_none());
            let current = &wire["conversationState"]["currentMessage"]["userInputMessage"];
            assert_eq!(current["modelId"], "deepseek-3.2");
            assert_eq!(current["origin"], "KIRO_CLI");
            assert_eq!(
                current["userInputMessageContext"]["envState"],
                json!({
                    "currentWorkingDirectory":"/client/project", "operatingSystem":"linux"
                })
            );
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(generation_body("pong"))
        })
        .expect(2)
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    let account = json!({
        "id":"acc_77777777", "email":"headless@example.com", "machine_id":"7".repeat(64),
        "credentials":{"access_token":"ksk_e2e-synthetic", "region":"eu-central-1",
            "expires_at":0, "auth_method":"api_key"}, "created_at":1
    });
    for field in ["refresh_token", "client_id", "client_secret"] {
        let mut invalid = account.clone();
        invalid["credentials"][field] = json!("stale-oauth-field");
        assert!(matches!(
            daemon
                .call("account.import", json!({"accounts":[invalid]}))
                .await,
            Response::Err { .. }
        ));
    }
    assert_eq!(
        expect_ok(
            daemon
                .call("account.import", json!({"accounts":[account]}))
                .await
        )["imported"],
        1
    );
    let client = reqwest::Client::new();
    let key = daemon.api_key.as_deref().unwrap();
    let models: Value = client
        .get(format!("http://127.0.0.1:{port}/v1/models?limit=1000"))
        .header("x-api-key", key)
        .header("user-agent", "claude-cli/2.1.235 (external, e2e)")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(models["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|model| model["id"] == "anthropic.deepseek-3.2"
            && model["display_name"] == "DeepSeek 3.2"));
    for stream in [false, true] {
        let response = client
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("x-api-key", key)
            .header("user-agent", "claude-cli/2.1.235 (external, e2e)")
            .header("anthropic-version", "2023-06-01")
            .json(
                &json!({"model":"anthropic.deepseek-3.2", "max_tokens":128, "stream":stream,
                "system":"<env>\nWorking directory: /client/project\nPlatform: linux\n</env>",
                "messages":[{"role":"user","content":"Reply pong"}]}),
            )
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        assert_eq!(status, reqwest::StatusCode::OK, "{body}");
        assert!(body.contains("pong"), "{body}");
        assert!(body.contains("anthropic.deepseek-3.2"), "{body}");
    }
    assert!(!mock
        .received_requests()
        .await
        .unwrap()
        .iter()
        .any(
            |request| request.url.path().contains("ListAvailableProfiles")
                || request.url.path().contains("ListAvailableModels")
        ));
    let exported = expect_ok(daemon.call("account.export", json!({"redact":true})).await);
    assert!(!exported.to_string().contains("ksk_e2e-synthetic"));
    daemon.stop().await;
}
