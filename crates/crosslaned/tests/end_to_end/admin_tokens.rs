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
async fn admin_token_bootstrap_respects_configured_and_running_listener_state() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mut daemon = Daemon::start().await;
    let file = daemon.home().join("config.toml");
    let paths = expect_ok(daemon.call("config.path", Value::Null).await);
    assert_eq!(
        paths["default_admin_socket"],
        json!(daemon.home().join("admin.sock"))
    );
    let reject_enable = async |daemon: &Daemon, expected_file: &str| {
        let response = daemon
            .call(
                "admin.token.create",
                json!({"name":"must-not-create","enable_rest":true}),
            )
            .await;
        let Response::Err { error, .. } = response else {
            panic!("listener must be configured and running before --enable-rest")
        };
        assert_eq!(error.code, 400);
        assert!(error.message.contains("crosslane config edit admin"));
        assert_eq!(
            tokio::fs::read_to_string(&file).await.unwrap(),
            expected_file
        );
    };
    let raw = tokio::fs::read_to_string(&file).await.unwrap();
    reject_enable(&daemon, &raw).await;
    let token = expect_ok(
        daemon
            .call("admin.token.create", json!({"name":"bootstrap"}))
            .await,
    );
    assert_eq!(token["rest_enabled"], false);
    assert_eq!(token["listener_enabled"], false);
    let secret = token["secret"].as_str().unwrap();
    let raw = tokio::fs::read_to_string(&file).await.unwrap();
    assert!(!raw.contains(secret));
    let mut config: Config = toml::from_str(&raw).unwrap();
    let port = unused_tcp_port();
    config.status_http.enabled = true;
    config.status_http.port = port;
    config.admin.rest.enabled = true;
    config.validate().unwrap();
    let updated = toml::to_string(&config).unwrap();
    {
        let _lock = crosslane_store::atomic::lock_file_exclusive(&file)
            .await
            .unwrap();
        crosslane_store::atomic::write_bytes_atomically(&file, updated.as_bytes(), Some(0o600))
            .await
            .unwrap();
    }
    let reload = expect_ok(daemon.call("config.reload", json!({})).await);
    assert_eq!(reload["applied"], true);
    assert!(reload["needs_restart"]
        .as_array()
        .unwrap()
        .contains(&json!("status_http.enabled")));
    reject_enable(&daemon, &updated).await;
    let listed = expect_ok(daemon.call("admin.token.list", Value::Null).await);
    assert_eq!(listed["tokens"].as_array().unwrap().len(), 1);
    assert_eq!(listed["rest_enabled"], false);
    assert_eq!(listed["listener_enabled"], false);

    daemon.child.kill().await.unwrap();
    daemon.child.wait().await.unwrap();
    daemon.child = Command::new(env!("CARGO_BIN_EXE_crosslaned"))
        .env("CROSSLANE_HOME", daemon.home())
        .env("CROSSLANE_DISABLE_HTTP", "1")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    wait_for_socket(&daemon.socket).await;
    wait_for_http(port).await;
    let response = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/admin/v1/providers"))
        .bearer_auth(secret)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    // Enabling REST must wait for the configured address, too: otherwise a
    // pending move away from a public listener could expose it at the old host.
    for (host, next_port) in [("127.0.0.1", unused_tcp_port()), ("0.0.0.0", port)] {
        let mut pending = config.clone();
        pending.status_http.host = host.into();
        pending.status_http.port = next_port;
        pending.admin.rest.enabled = false;
        let pending_raw = toml::to_string(&pending).unwrap();
        {
            let _lock = crosslane_store::atomic::lock_file_exclusive(&file)
                .await
                .unwrap();
            crosslane_store::atomic::write_bytes_atomically(
                &file,
                pending_raw.as_bytes(),
                Some(0o600),
            )
            .await
            .unwrap();
        }
        assert_eq!(
            expect_ok(daemon.call("config.reload", json!({})).await)["applied"],
            true
        );
        reject_enable(&daemon, &pending_raw).await;
        pending.admin.rest.enabled = true;
        {
            let _lock = crosslane_store::atomic::lock_file_exclusive(&file)
                .await
                .unwrap();
            crosslane_store::atomic::write_bytes_atomically(
                &file,
                toml::to_string(&pending).unwrap().as_bytes(),
                Some(0o600),
            )
            .await
            .unwrap();
        }
        assert_eq!(
            expect_ok(daemon.call("config.reload", json!({})).await)["applied"],
            true
        );
        let listed = expect_ok(daemon.call("admin.token.list", Value::Null).await);
        assert_eq!(listed["rest_enabled"], false);
        let rotated = expect_ok(
            daemon
                .call("admin.token.rotate", json!({"id":"bootstrap"}))
                .await,
        );
        assert_eq!(rotated["rest_enabled"], false);
        config.admin.rest =
            toml::from_str::<Config>(&tokio::fs::read_to_string(&file).await.unwrap())
                .unwrap()
                .admin
                .rest;
        assert!(config.admin.rest.enabled);
    }

    // The listener remains alive until restart, but a pending disable must not
    // be silently undone by creating another token with --enable-rest.
    config.status_http.enabled = false;
    config.admin.rest.enabled = false;
    let disabled = toml::to_string(&config).unwrap();
    {
        let _lock = crosslane_store::atomic::lock_file_exclusive(&file)
            .await
            .unwrap();
        crosslane_store::atomic::write_bytes_atomically(&file, disabled.as_bytes(), Some(0o600))
            .await
            .unwrap();
    }
    let reload = expect_ok(daemon.call("config.reload", json!({})).await);
    assert_eq!(reload["applied"], true);
    let listed = expect_ok(daemon.call("admin.token.list", Value::Null).await);
    assert_eq!(listed["listener_enabled"], true);
    assert_eq!(listed["rest_enabled"], false);
    reject_enable(&daemon, &disabled).await;
    daemon.stop().await;
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
