use super::*;
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
