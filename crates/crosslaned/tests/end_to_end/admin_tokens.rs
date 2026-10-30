use super::admin_rest::{AdminFixture, READ_TOKEN, WRITE_TOKEN};
use super::*;
use reqwest::Method;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

async fn bearer(fixture: &AdminFixture, secret: &str, expected: u16) {
    let response = fixture
        .request(Method::GET, "/accounts", Some(secret))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), expected);
}

async fn rpc_error(fixture: &AdminFixture, name: &str, params: Value, code: i32) {
    let response = fixture.daemon.call(name, params).await;
    let Response::Err { error, .. } = response else {
        panic!("expected rejected admin mutation")
    };
    assert_eq!(error.code, code);
}

#[tokio::test]
async fn named_admin_tokens_are_independent_durable_revocable_and_compatible_with_legacy_credentials(
) {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mut fixture = AdminFixture::start(false).await;
    let file = fixture.daemon.home().join("config.toml");
    let port = unused_tcp_port();
    let service = expect_ok(
        fixture
            .daemon
            .call(
                "service.create",
                json!({"name":"token-fixture","host":"127.0.0.1","port":port}),
            )
            .await,
    );
    let client_key = service["api_key"]["key"].as_str().unwrap().to_owned();
    let original = tokio::fs::read_to_string(&file).await.unwrap();
    let listed = expect_ok(fixture.daemon.call("admin.token.list", Value::Null).await);
    assert_eq!(listed["tokens"].as_array().unwrap().len(), 2);
    assert!(listed.to_string().contains("legacy-admin"));
    assert!(listed.to_string().contains("legacy-read-only"));
    rpc_error(
        &fixture,
        "admin.token.create",
        json!({"name":"\n","role":"admin"}),
        400,
    )
    .await;
    assert_eq!(tokio::fs::read_to_string(&file).await.unwrap(), original);
    let ops = expect_ok(
        fixture
            .daemon
            .call("admin.token.create", json!({"name":"ops","role":"admin"}))
            .await,
    );
    let ops_id = ops["token"]["id"].as_str().unwrap().to_owned();
    let old_secret = ops["secret"].as_str().unwrap().to_owned();
    assert!(old_secret.starts_with("sk-admin-"));
    bearer(&fixture, &old_secret, 200).await;
    bearer(&fixture, WRITE_TOKEN, 200).await;
    bearer(&fixture, READ_TOKEN, 200).await;
    let raw = tokio::fs::read_to_string(&file).await.unwrap();
    assert!(!raw.contains(&old_secret));
    let disk: Config = toml::from_str(&raw).unwrap();
    assert_eq!(
        disk.admin.rest.tokens[0].token_sha256,
        format!("{:x}", Sha256::digest(old_secret.as_bytes()))
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    rpc_error(&fixture, "admin.token.create", json!({"name":"ops"}), 409).await;
    let reader = expect_ok(
        fixture
            .daemon
            .call(
                "admin.token.create",
                json!({"name":"monitor","role":"read_only"}),
            )
            .await,
    );
    let reader_secret = reader["secret"].as_str().unwrap().to_owned();
    bearer(&fixture, &reader_secret, 200).await;
    assert_eq!(
        fixture
            .request(Method::POST, "/services", Some(&reader_secret))
            .json(&json!({"name":"must-not-create"}))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let safe = expect_ok(
        fixture
            .daemon
            .call("admin.token.show", json!({"id":"ops"}))
            .await,
    );
    assert!(safe["token"].get("token_sha256").is_none());
    assert!(safe.get("secret").is_none());
    let rotated = expect_ok(
        fixture
            .daemon
            .call("admin.token.rotate", json!({"id":ops_id}))
            .await,
    );
    let secret = rotated["secret"].as_str().unwrap().to_owned();
    assert_eq!(rotated["token"]["id"], ops_id);
    bearer(&fixture, &old_secret, 401).await;
    bearer(&fixture, &secret, 200).await;
    bearer(&fixture, &reader_secret, 200).await;
    expect_ok(
        fixture
            .daemon
            .call(
                "admin.token.setEnabled",
                json!({"id":"ops","enabled":false}),
            )
            .await,
    );
    bearer(&fixture, &secret, 401).await;
    expect_ok(
        fixture
            .daemon
            .call("admin.token.setEnabled", json!({"id":"ops","enabled":true}))
            .await,
    );
    bearer(&fixture, &secret, 200).await;
    expect_ok(
        fixture
            .daemon
            .call("admin.token.revoke", json!({"id":"monitor"}))
            .await,
    );
    bearer(&fixture, &reader_secret, 401).await;
    rpc_error(&fixture, "admin.token.show", json!({"id":"monitor"}), 404).await;

    let legacy = expect_ok(
        fixture
            .daemon
            .call("admin.token.rotate", json!({"id":"legacy-admin"}))
            .await,
    );
    let legacy_secret = legacy["secret"].as_str().unwrap().to_owned();
    bearer(&fixture, WRITE_TOKEN, 401).await;
    bearer(&fixture, &legacy_secret, 200).await;
    bearer(&fixture, READ_TOKEN, 200).await;
    expect_ok(
        fixture
            .daemon
            .call(
                "admin.token.setEnabled",
                json!({"id":"legacy-read-only","enabled":false}),
            )
            .await,
    );
    bearer(&fixture, READ_TOKEN, 401).await;
    expect_ok(
        fixture
            .daemon
            .call(
                "admin.token.setEnabled",
                json!({"id":"legacy-read-only","enabled":true}),
            )
            .await,
    );
    bearer(&fixture, READ_TOKEN, 200).await;

    // Cooperating writes must not lose either change, nor resurrect old secrets.
    let (rotated, backup) = tokio::join!(
        fixture
            .daemon
            .call("admin.token.rotate", json!({"id":"ops"})),
        fixture
            .daemon
            .call("admin.token.create", json!({"name":"backup"}))
    );
    let rotated = expect_ok(rotated);
    let backup = expect_ok(backup);
    let final_secret = rotated["secret"].as_str().unwrap().to_owned();
    let backup_secret = backup["secret"].as_str().unwrap().to_owned();
    bearer(&fixture, &secret, 401).await;
    bearer(&fixture, &final_secret, 200).await;
    bearer(&fixture, &backup_secret, 200).await;
    // A later failed business configuration must roll back to the new auth
    // baseline, never to the token that was just rotated/revoked.
    let occupied = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    rpc_error(
        &fixture,
        "service.create",
        json!({"name":"occupied","host":"127.0.0.1","port":occupied.local_addr().unwrap().port()}),
        400,
    )
    .await;
    bearer(&fixture, &final_secret, 200).await;
    bearer(&fixture, &secret, 401).await;
    assert_eq!(
        expect_ok(fixture.daemon.call("config.reload", json!({})).await)["applied"],
        true
    );
    fixture.restart().await;
    bearer(&fixture, &final_secret, 200).await;
    bearer(&fixture, &backup_secret, 200).await;
    bearer(&fixture, &old_secret, 401).await;
    bearer(&fixture, WRITE_TOKEN, 401).await;
    let response = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/v1/models"))
        .bearer_auth(&client_key)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let listed = expect_ok(fixture.daemon.call("admin.token.list", Value::Null).await);
    assert_eq!(listed["tokens"].as_array().unwrap().len(), 4);
    for token in listed["tokens"].as_array().unwrap() {
        let revoked = expect_ok(
            fixture
                .daemon
                .call("admin.token.revoke", json!({"id":token["id"]}))
                .await,
        );
        assert_eq!(revoked["token"]["enabled"], false);
    }
    let none = expect_ok(fixture.daemon.call("admin.token.list", Value::Null).await);
    assert_eq!(none["tokens"], json!([]));
    assert_eq!(none["rest_enabled"], false);
    assert_eq!(none["listener_enabled"], true);
    bearer(&fixture, &final_secret, 404).await;
    let health: Value = reqwest::get(format!("http://127.0.0.1:{}/health", fixture.port))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    status_http::assert_health_contract(&health);
    // Bootstrap is always possible over UDS; no existing Bearer token is needed.
    let bootstrap = expect_ok(
        fixture
            .daemon
            .call(
                "admin.token.create",
                json!({"name":"bootstrap","enable_rest":true}),
            )
            .await,
    );
    bearer(&fixture, bootstrap["secret"].as_str().unwrap(), 200).await;
    let log = tokio::fs::read_to_string(fixture.daemon.home().join("rest-test.log"))
        .await
        .unwrap();
    assert!(log.contains("admin.token.changed"));
    assert!(log.contains(&ops_id));
    let events = log
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect::<Vec<_>>();
    assert!(events
        .iter()
        .any(|event| event["fields"]["event"] == "admin.http.request"
            && event["fields"]["token_id"] == ops_id
            && event["fields"]["role"] == "admin"));
    assert!(events
        .iter()
        .any(|event| event["fields"]["event"] == "admin.http.request"
            && event["fields"]["token_id"] == reader["token"]["id"]
            && event["fields"]["role"] == "read_only"));
    for key in [
        &old_secret,
        &secret,
        &final_secret,
        &legacy_secret,
        &reader_secret,
        &backup_secret,
    ] {
        assert!(!log.contains(key));
        assert!(!log.contains(&format!("{:x}", Sha256::digest(key.as_bytes()))));
    }
    fixture.daemon.stop().await;
}
