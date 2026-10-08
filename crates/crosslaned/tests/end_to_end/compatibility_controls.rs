use super::*;
use serde_json::{json, Value};

#[tokio::test]
async fn reference_gateway_hints_succeed_without_leaking_into_kiro() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ListAvailableModels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":[{
            "modelId":"claude-haiku-4.5", "additionalModelRequestFieldsSchema":null
        }]})))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(|request: &wiremock::Request| {
            let wire: Value = serde_json::from_slice(&request.body).unwrap();
            assert!(wire.get("additionalModelRequestFields").is_none(), "{wire}");
            let serialized = wire.to_string();
            for field in [
                "output_format_sentinel",
                "output_config",
                "task_budget",
                "response_format",
                "strict",
                "future_hint",
                "future_summary",
                "future_context",
                "future.include",
            ] {
                assert!(!serialized.contains(field), "{field} leaked into {wire}");
            }
            assert!(serialized.contains("lookup"), "{wire}");
            assert!(serialized.contains("tool_argument"), "{wire}");
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(generation_body("pong"))
        })
        .expect(6)
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    expect_ok(daemon.call("account.import", json!({"accounts":[{
        "id":"acc_66666666", "email":"compatibility@example.com", "machine_id":"6".repeat(64),
        "credentials":{"access_token":"compatibility-test-token", "region":"us-east-1",
            "expires_at":4_000_000_000i64, "auth_method":"idc"},
        "usage":{"current":0.0,"limit":100.0,"percent_used":0.0,"updated_at":1}, "created_at":1
    }]})).await);
    assert_eq!(
        expect_ok(daemon.call("models", json!({})).await)
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let client = reqwest::Client::new();
    let tool_schema = json!({"type":"object","properties":{"tool_argument":{"type":"string"}}});
    let format_schema =
        json!({"type":"object","properties":{"output_format_sentinel":{"type":"string"}}});
    for stream in [false, true] {
        for route in ["messages", "chat/completions", "responses"] {
            let mut request = json!({
                "model":"claude-haiku-4-5-20251001","stream":stream,"future_hint":true,
                "service_tier":"priority"
            });
            if route == "responses" {
                request["input"] = json!("Reply pong");
                request["max_output_tokens"] = json!(4096);
                // Codex sends reasoning presentation hints whose values are not
                // in the published enum. They must not reach Kiro, and must not
                // be rejected: `reasoning.context` outside {auto} was a 400.
                request["reasoning"] = json!({
                    "effort":"medium","summary":"future_summary",
                    "context":"future_context","future_hint":true
                });
                request["prompt_cache_retention"] = json!("24h");
                request["include"] = json!(["reasoning.encrypted_content", "future.include"]);
                request["truncation"] = json!("disabled");
                request["text"] = json!({"format":{"type":"json_schema","schema":format_schema},"future_hint":true});
                request["tools"] = json!([{"type":"function","name":"lookup","parameters":tool_schema,"strict":true}]);
            } else {
                request["messages"] =
                    json!([{"role":"user","content":"Reply pong","future_hint":true}]);
                request["max_tokens"] = json!(4096);
                if route == "messages" {
                    request["output_config"] = json!({
                        "format":{"type":"json_schema","schema":format_schema},
                        "task_budget":{"type":"tokens","total":64000},"future_hint":true});
                    request["tools"] = json!([{"name":"lookup","input_schema":tool_schema,
                        "strict":true,"eager_input_streaming":true,"future_hint":true}]);
                } else {
                    request["response_format"] = json!({"type":"json_schema","json_schema":{
                        "name":"answer","schema":format_schema,"strict":true}});
                    request["tools"] = json!([{"type":"function","function":{
                        "name":"lookup","parameters":tool_schema,"strict":true}}]);
                }
            }
            let response = client
                .post(format!("http://127.0.0.1:{port}/v1/{route}"))
                .bearer_auth(daemon.api_key.as_deref().unwrap())
                .header("x-api-key", daemon.api_key.as_deref().unwrap())
                .header("anthropic-version", "2023-06-01")
                .header(
                    "user-agent",
                    if route == "messages" {
                        "claude-cli/2.1.235 (external, e2e)"
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
            assert_eq!(
                status,
                reqwest::StatusCode::OK,
                "{route} stream={stream}: {body}"
            );
            assert!(body.contains("pong"), "{body}");
            assert!(!body.contains("invalid_request"), "{body}");
        }
    }
    daemon.stop().await;
}
