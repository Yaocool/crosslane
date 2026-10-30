use std::collections::BTreeSet;

use super::*;

pub(super) fn assert_health_contract(health: &serde_json::Value) {
    let expected = [
        "status",
        "total_accounts",
        "available_accounts",
        "protected_accounts",
        "cooling_accounts",
        "exhausted_accounts",
        "banned_accounts",
        "refreshing_accounts",
        "disabled_accounts",
        "unavailable_accounts",
        "services",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    let fields = health.as_object().expect("health object");
    assert_eq!(
        fields.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        expected
    );
    assert_eq!(health["status"], "ok");
    for (field, value) in fields {
        if field != "status" && field != "services" {
            assert!(value.is_u64(), "account count must be an integer: {field}");
        }
    }
    for service in health["services"].as_array().expect("service states") {
        assert_eq!(
            service
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["name", "enabled", "running"]),
        );
        assert!(service["name"].is_string());
        assert!(service["enabled"].is_boolean());
        assert!(service["running"].is_boolean());
    }
}

#[tokio::test]
async fn independent_status_listener_survives_service_changes_and_checks_each_pool() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let upstream = MockServer::start().await;
    let home = tempfile::tempdir().expect("tempdir");
    let socket = home.path().join("admin.sock");
    let status_port = unused_tcp_port();
    let mut config = Config::default();
    config.admin.socket = socket.to_string_lossy().into_owned();
    config.status_http.enabled = true;
    config.status_http.port = status_port;
    tokio::fs::write(
        home.path().join("config.toml"),
        toml::to_string(&config).unwrap(),
    )
    .await
    .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_crosslaned"))
        .env("CROSSLANE_HOME", home.path())
        .env("CROSSLANE_CODEWHISPERER_URL", upstream.uri())
        .env("CROSSLANE_AMAZONQ_URL", upstream.uri())
        .env("CROSSLANE_RUNTIME_URL", upstream.uri())
        .env("CROSSLANE_MANAGEMENT_URL", upstream.uri())
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn daemon with independent status listener");
    wait_for_socket(&socket).await;
    wait_for_http(status_port).await;
    let daemon = Daemon {
        child,
        socket,
        home,
        api_key: None,
    };
    let client = reqwest::Client::new();
    let status_url = format!("http://127.0.0.1:{status_port}");

    let root = client
        .get(&status_url)
        .header("user-agent", "status-monitor/1.0")
        .send()
        .await
        .unwrap();
    assert_eq!(root.status(), reqwest::StatusCode::OK);
    assert_eq!(
        root.json::<serde_json::Value>().await.unwrap()["name"],
        "crosslane"
    );
    let health = client
        .get(format!("{status_url}/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), reqwest::StatusCode::OK);
    let health: serde_json::Value = health.json().await.unwrap();
    assert_health_contract(&health);
    assert_eq!(health["services"], serde_json::json!([]));
    assert_eq!(health["total_accounts"], 0);
    assert_eq!(
        client
            .get(format!("{status_url}/ready"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );
    for path in [
        "/v1/models",
        "/v1/messages",
        "/api/event_logging/batch",
        "/api/v1/accounts",
    ] {
        assert_eq!(
            client
                .get(format!("{status_url}{path}"))
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::NOT_FOUND
        );
    }

    let port = unused_tcp_port();
    let first = expect_ok(
        daemon
            .call(
                "service.create",
                serde_json::json!({
                    "name":"team-a", "host":"127.0.0.1", "port":port, "account_tag":"team-a"
                }),
            )
            .await,
    );
    for path in ["/", "/health", "/ready"] {
        assert_eq!(
            client
                .get(format!("http://127.0.0.1:{port}{path}"))
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::NOT_FOUND
        );
    }
    let key = first["api_key"]["key"].as_str().unwrap();
    assert_eq!(
        client
            .get(format!("http://127.0.0.1:{port}/v1/models"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .post(format!("http://127.0.0.1:{port}/api/event_logging/batch"))
            .bearer_auth(key)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );

    expect_ok(daemon.call("account.import", serde_json::json!({"accounts":[{
        "id":"acc_00000001", "email":"status-fixture@example.com", "enabled":true,
        "machine_id":"a".repeat(64), "tags":["team-a"],
        "credentials":{"access_token":"fixture-only", "region":"us-east-1", "expires_at":4_000_000_000i64, "auth_method":"idc"},
        "usage":{"current":0.0,"limit":100.0,"percent_used":0.0,"updated_at":1},
        "created_at":1
    }]})).await);
    let ready = client
        .get(format!("{status_url}/ready"))
        .send()
        .await
        .unwrap();
    assert_eq!(ready.status(), reqwest::StatusCode::OK);
    assert_eq!(
        ready.json::<serde_json::Value>().await.unwrap()["services"][0]["available_accounts"],
        1
    );
    let health: serde_json::Value = client
        .get(format!("{status_url}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_health_contract(&health);
    assert_eq!(
        health["total_accounts"], 0,
        "tagged accounts are excluded even when their service is ready"
    );
    assert_eq!(
        health["services"],
        serde_json::json!([{"name":"team-a","enabled":true,"running":true}])
    );
    assert!(!health
        .to_string()
        .contains(first["service"]["id"].as_str().unwrap()));

    expect_ok(daemon.call("account.import", serde_json::json!({"accounts":[
        {"id":"acc_00000002", "email":"untagged@example.com", "enabled":true,
         "machine_id":"b".repeat(64), "tags":[],
         "credentials":{"access_token":"fixture-only-untagged", "region":"us-east-1", "expires_at":4_000_000_000i64, "auth_method":"idc"},
         "usage":{"current":26.75,"limit":100.0,"percent_used":26.75,"updated_at":1}, "created_at":1},
        {"id":"acc_00000003", "email":"untagged-disabled@example.com", "enabled":false,
         "machine_id":"c".repeat(64),
         "credentials":{"access_token":"fixture-only-disabled", "region":"us-east-1", "expires_at":4_000_000_000i64, "auth_method":"idc"},
         "created_at":1}
    ]})).await);

    let second = expect_ok(
        daemon
            .call(
                "service.create",
                serde_json::json!({
                    "name":"shared", "host":"127.0.0.1", "port":unused_tcp_port()
                }),
            )
            .await,
    );
    assert_eq!(
        client
            .get(format!("{status_url}/ready"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    let ready: serde_json::Value = client
        .get(format!("{status_url}/ready"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        ready["services"]
            .as_array()
            .unwrap()
            .iter()
            .find(|service| service["service_name"] == "shared")
            .unwrap()["available_accounts"],
        2
    );
    let unavailable = expect_ok(daemon.call("service.create", serde_json::json!({
        "name":"team-b", "host":"127.0.0.1", "port":unused_tcp_port(), "account_tag":"team-b"
    })).await);
    let ready = client
        .get(format!("{status_url}/ready"))
        .send()
        .await
        .unwrap();
    assert_eq!(ready.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    let ready: serde_json::Value = ready.json().await.unwrap();
    assert!(ready["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .any(|reason| reason
            .as_str()
            .unwrap()
            .contains("team-b has no available account")));
    let health: serde_json::Value = client
        .get(format!("{status_url}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_health_contract(&health);
    // Untagged active and disabled accounts count once, independently of service membership.
    assert_eq!(health["total_accounts"], 2);
    assert_eq!(health["available_accounts"], 1);
    assert_eq!(health["disabled_accounts"], 1);
    assert_eq!(health["services"].as_array().unwrap().len(), 3);
    for service in [&first, &second, &unavailable] {
        assert!(!health
            .to_string()
            .contains(service["service"]["id"].as_str().unwrap()));
    }

    expect_ok(
        daemon
            .call(
                "service.delete",
                serde_json::json!({"service":unavailable["service"]["id"]}),
            )
            .await,
    );
    assert_eq!(
        client
            .get(format!("{status_url}/ready"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    expect_ok(
        daemon
            .call(
                "service.delete",
                serde_json::json!({"service":second["service"]["id"]}),
            )
            .await,
    );
    expect_ok(
        daemon
            .call(
                "service.delete",
                serde_json::json!({"service":first["service"]["id"]}),
            )
            .await,
    );
    assert_eq!(
        client
            .get(format!("{status_url}/health"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(
        client
            .get(format!("{status_url}/ready"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );
    daemon.stop().await;
}
