use super::*;
use serde_json::{json, Value};

#[tokio::test]
async fn copilot_client_aliases_use_canonical_ids_across_protocols_and_permissions() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let upstream = MockServer::start().await;
    for (endpoint, body) in [
        ("/user", json!({"id":7,"login":"alias-test"})),
        (
            "/copilot_internal/v2/token",
            json!({"token":"fake-api-token","expires_at":4_000_000_000i64,"endpoints":{"api":upstream.uri()}}),
        ),
        (
            "/models",
            json!({"data":(["claude-opus-4.8","claude-opus-5.5","claude-sonnet-4.8"].map(|id| json!({
                "id":id,"capabilities":{"supported_endpoints":["/v1/messages","/chat/completions","/responses"]}
            })))}),
        ),
    ] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&upstream)
            .await;
    }
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer fake-api-token"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let model = body["model"].as_str().unwrap();
            assert!(matches!(model, "claude-opus-4.8" | "claude-sonnet-4.8"), "{body}");
            if body["stream"] == true {
                return ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(format!(
                        "event: message_start\ndata: {}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
                        json!({"type":"message_start","message":{"id":"msg-alias","type":"message","model":model,"role":"assistant","content":[],"usage":{"input_tokens":7,"output_tokens":0}}})
                    ));
            }
            let response = match request.url.path() {
                "/v1/messages/count_tokens" => json!({"input_tokens":7}),
                "/v1/messages" => json!({"id":"msg-alias","type":"message","model":model,"role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":7,"output_tokens":1}}),
                "/chat/completions" => json!({"id":"chat-alias","object":"chat.completion","model":model,"choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":1,"total_tokens":8}}),
                "/responses" => json!({"id":"resp-alias","object":"response","model":model,"status":"completed","output":[],"usage":{"input_tokens":7,"output_tokens":1,"total_tokens":8}}),
                path => panic!("unexpected upstream path {path}"),
            };
            ResponseTemplate::new(200).set_body_json(response)
        })
        .expect(7)
        .mount(&upstream)
        .await;
    let port = unused_tcp_port();
    let daemon = Daemon::start_copilot(port, &upstream.uri()).await;
    expect_ok(
        daemon
            .call(
                "v2.account.importToken",
                json!({"provider":"copilot","token":"fake-github-token"}),
            )
            .await,
    );
    model_policy::configure(&daemon, |config| {
        config.api_key[0].allowed_models = vec!["copilot/claude-opus-4.8".into()];
    })
    .await;
    let client = reqwest::Client::new();
    for (endpoint, model, stream) in [
        ("/v1/messages", "claude-opus-4-8", false),
        ("/v1/messages", "copilot/claude-opus-4-8", false),
        ("/v1/chat/completions", "claude-opus-4-8", false),
        ("/v1/responses", "claude-opus-4-8", false),
        ("/v1/messages/count_tokens", "claude-opus-4-8", false),
        ("/v1/messages", "claude-opus-4-8", true),
    ] {
        let body = if endpoint == "/v1/responses" {
            json!({"model":model,"input":"hello","stream":stream})
        } else {
            json!({"model":model,"messages":[{"role":"user","content":"hello"}],"max_tokens":16,"stream":stream})
        };
        let response = client
            .post(format!("http://127.0.0.1:{port}{endpoint}"))
            .bearer_auth(daemon.api_key.as_deref().unwrap())
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        assert_eq!(status, 200, "{endpoint} {model}: {text}");
        if stream {
            assert!(text.contains("claude-opus-4.8"), "{text}");
        } else {
            let response: Value = serde_json::from_str(&text).unwrap();
            if endpoint.ends_with("count_tokens") {
                assert_eq!(response["input_tokens"], 7);
            } else {
                assert_eq!(response["model"], "claude-opus-4.8");
            }
        }
    }
    for (model, status, error) in [
        ("claude-opus-4-9", 400, "model_not_available"),
        ("claude-sonnet-4-8", 403, "not allowed"),
    ] {
        let response = client.post(format!("http://127.0.0.1:{port}/v1/messages"))
            .bearer_auth(daemon.api_key.as_deref().unwrap())
            .json(&json!({"model":model,"messages":[{"role":"user","content":"hello"}],"max_tokens":16}))
            .send().await.unwrap();
        assert_eq!(response.status(), status);
        assert!(response.text().await.unwrap().contains(error));
    }
    model_policy::configure(&daemon, |config| {
        config.models.disabled = vec!["claude-opus-4.8".into()];
    })
    .await;
    let response = client
        .post(format!("http://127.0.0.1:{port}/v1/messages/count_tokens"))
        .bearer_auth(daemon.api_key.as_deref().unwrap())
        .json(&json!({"model":"claude-opus-4-8","messages":[{"role":"user","content":"hello"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert!(response.text().await.unwrap().contains("model_disabled"));
    model_policy::configure(&daemon, |config| {
        config.models.disabled.clear();
        config.api_key[0].allowed_models = vec!["copilot/claude-sonnet-4.8".into()];
        config.model_mapping.push(ModelMappingRule {
            name: "force-sonnet".into(),
            source_models: vec!["claude-opus-4-8".into()],
            target_models: vec!["claude-sonnet-4-8".into()],
            providers: vec!["copilot".into()],
            ..ModelMappingRule::default()
        });
    })
    .await;
    let response = client.post(format!("http://127.0.0.1:{port}/v1/messages"))
        .bearer_auth(daemon.api_key.as_deref().unwrap())
        .json(&json!({"model":"claude-opus-4-8","messages":[{"role":"user","content":"hello"}],"max_tokens":16}))
        .send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<Value>().await.unwrap()["model"],
        "claude-sonnet-4.8"
    );
    daemon.stop().await;
}
