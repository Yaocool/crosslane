use super::*;
use serde_json::{json, Value};

pub(super) async fn configure(daemon: &Daemon, mutate: impl FnOnce(&mut Config)) {
    let file = daemon.home().join("config.toml");
    let mut config: Config =
        toml::from_str(&tokio::fs::read_to_string(&file).await.unwrap()).unwrap();
    mutate(&mut config);
    tokio::fs::write(file, toml::to_string_pretty(&config).unwrap())
        .await
        .unwrap();
    let reload = expect_ok(daemon.call("config.reload", json!({})).await);
    assert_eq!(reload["applied"], true, "{reload}");
}

async fn assert_hidden_and_rejected(daemon: &Daemon, port: u16, models: &[&str], hidden: &str) {
    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");
    for endpoint in ["/models", "/v1/models"] {
        for anthropic in [false, true] {
            let mut request = client
                .get(format!("{base}{endpoint}"))
                .bearer_auth(daemon.api_key.as_deref().unwrap());
            if anthropic {
                request = request.header("anthropic-version", "2023-06-01");
            }
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), 200);
            let text = response.text().await.unwrap();
            assert!(!text.contains(hidden), "disabled model leaked: {text}");
        }
    }
    for model in models {
        for endpoint in [
            "/v1/chat/completions",
            "/v1/responses",
            "/v1/messages",
            "/v1/messages/count_tokens",
        ] {
            for stream in [false, true] {
                let mut body = json!({"model":model,"stream":stream,"messages":[{"role":"user","content":"hello"}],"max_tokens":32});
                if endpoint == "/v1/responses" {
                    body = json!({"model":model,"stream":stream,"input":"hello"});
                }
                let response = client
                    .post(format!("{base}{endpoint}"))
                    .bearer_auth(daemon.api_key.as_deref().unwrap())
                    .header("user-agent", "claude-cli/2.1.0 (external, e2e)")
                    .header("anthropic-version", "2023-06-01")
                    .json(&body)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    400,
                    "{model} {endpoint} stream={stream}: {}",
                    response.text().await.unwrap()
                );
                let body = response.text().await.unwrap();
                assert!(
                    body.contains("model_disabled"),
                    "{model} {endpoint} stream={stream}: {body}"
                );
            }
        }
    }
}

#[tokio::test]
async fn disabled_models_copilot_reload_hides_catalog_and_blocks_mappings_then_reenables() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let upstream = MockServer::start().await;
    for (endpoint, body) in [
        ("/user", json!({"id":7,"login":"policy-test"})),
        (
            "/copilot_internal/v2/token",
            json!({"token":"fake-api-token","expires_at":4_000_000_000i64,"endpoints":{"api":upstream.uri()}}),
        ),
        (
            "/models",
            json!({"data":[{"id":"gpt-test","capabilities":{"supported_endpoints":["/chat/completions"]}}]}),
        ),
    ] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&upstream)
            .await;
    }
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"test","model":"gpt-test","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]})))
        .expect(1).mount(&upstream).await;
    let port = unused_tcp_port();
    let daemon = Daemon::start_copilot(port, &upstream.uri()).await;
    let capabilities = expect_ok(daemon.call("v2.capabilities", json!({})).await);
    assert!(capabilities["features"]
        .as_array()
        .unwrap()
        .contains(&json!(
            crosslane_ipc::protocol::GLOBAL_MODEL_DISABLE_FEATURE
        )));
    expect_ok(
        daemon
            .call(
                "v2.account.importToken",
                json!({"provider":"copilot","token":"fake-github-token"}),
            )
            .await,
    );
    configure(&daemon, |config| {
        config.models.disabled = vec!["gpt-test".into()]
    })
    .await;
    let admin = expect_ok(
        daemon
            .call("v2.models.list", json!({"provider":"copilot"}))
            .await,
    );
    assert_eq!(admin["disabled_models"], json!(["gpt-test"]));
    assert_eq!(admin["models"][0]["id"], "gpt-test");
    let accounts = expect_ok(
        daemon
            .call("v2.account.list", json!({"provider":"copilot"}))
            .await,
    );
    assert_eq!(accounts["accounts"][0]["supported_models"], json!([]));
    assert_hidden_and_rejected(
        &daemon,
        port,
        &["gpt-test", "copilot/gpt-test", "team-fast"],
        "gpt-test",
    )
    .await;
    assert!(upstream
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|request| request.method.as_str() == "GET"));
    // Unchanged provider settings reuse the original adapter: its live policy
    // must be updated, not just a newly constructed runtime.
    configure(&daemon, |config| config.models.disabled.clear()).await;
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .bearer_auth(daemon.api_key.as_deref().unwrap())
        .json(&json!({"model":"team-fast","messages":[{"role":"user","content":"hello"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    daemon.stop().await;
}

#[tokio::test]
async fn disabled_models_kiro_blocks_aliases_defaults_mapping_and_internal_probe() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ListAvailableModels"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"models":[{"modelId":"claude-opus-5"}]})),
        )
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(generation_body("ok")),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let port = unused_tcp_port();
    let daemon = Daemon::start_http(
        port,
        &format!("{}/generateAssistantResponse", upstream.uri()),
    )
    .await;
    expect_ok(daemon.call("account.import", json!({"accounts":[{
        "id":"acc_11111111","email":"policy@example.com","machine_id":"1".repeat(64),
        "credentials":{"access_token":"fake-token","region":"us-east-1","expires_at":4_000_000_000i64,"auth_method":"idc"},
        "usage":{"current":0.0,"limit":100.0,"percent_used":0.0,"updated_at":1},"subscription":{"kind":"pro"},"created_at":1
    }]})).await);
    expect_ok(daemon.call("models", json!({})).await);
    configure(&daemon, |config| {
        config.server.enforce_user_agent_check = false;
        config.models.disabled = vec!["claude-opus-5".into()];
        config.features.default_model_id = "claude-opus-5".into();
        config.model_mapping.push(ModelMappingRule {
            name: "denied-target".into(),
            source_models: vec!["friendly".into()],
            target_models: vec!["claude-opus-5".into()],
            ..Default::default()
        });
    })
    .await;
    assert_hidden_and_rejected(
        &daemon,
        port,
        &[
            "claude-opus-5",
            "opus5",
            "kiro/claude-opus-5",
            "friendly",
            "unknown-model",
        ],
        "claude-opus-5",
    )
    .await;
    let probe = expect_ok(
        daemon
            .call("account.probe", json!({"id":"acc_11111111"}))
            .await,
    );
    assert!(
        probe
            .to_string()
            .contains("all probe models are globally disabled"),
        "{probe}"
    );
    let pool = expect_ok(daemon.call("pool", json!({"model":"opus5"})).await);
    for account in pool["accounts"].as_array().unwrap() {
        assert_eq!(account["eligible"], false);
        assert_eq!(account["reason"], "model_disabled");
    }
    assert!(upstream
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|request| request.method.as_str() == "GET"));
    // The static catalog must obey the same rule.
    configure(&daemon, |config| config.models.dynamic_discovery = false).await;
    let list: Value = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/v1/models"))
        .bearer_auth(daemon.api_key.as_deref().unwrap())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!list.to_string().contains("claude-opus-5"));
    configure(&daemon, |config| config.models.disabled.clear()).await;
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .bearer_auth(daemon.api_key.as_deref().unwrap())
        .json(&json!({"model":"opus5","messages":[{"role":"user","content":"hello"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    daemon.stop().await;
}
