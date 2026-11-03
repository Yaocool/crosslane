use super::*;
use reqwest::Method;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

async fn reload(daemon: &Daemon, config: &Config) -> Value {
    let file = daemon.home().join("config.toml");
    {
        let _lock = crosslane_store::atomic::lock_file_exclusive(&file)
            .await
            .unwrap();
        crosslane_store::atomic::write_bytes_atomically(
            &file,
            toml::to_string(config).unwrap().as_bytes(),
            Some(0o600),
        )
        .await
        .unwrap();
    }
    expect_ok(daemon.call("config.reload", json!({})).await)
}

async fn wait_closed(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_ok()
    {
        assert!(
            Instant::now() < deadline,
            "old status listener did not close"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn status_http_reloads_enable_move_rollback_and_disable_without_restarting_business_services()
{
    let _guard = HTTP_TEST_LOCK.lock().await;
    let upstream = MockServer::start().await;
    let business_port = unused_tcp_port();
    let daemon = Daemon::start_http(business_port, &upstream.uri()).await;
    let pid = daemon.child.id();
    let file = daemon.home().join("config.toml");
    let mut config: Config =
        toml::from_str(&tokio::fs::read_to_string(&file).await.unwrap()).unwrap();
    let port = unused_tcp_port();
    config.status_http.enabled = true;
    config.status_http.port = port;
    config.admin.rest.enabled = true;
    config.admin.rest.token_sha256 = format!("{:x}", Sha256::digest(b"reload-admin-token"));
    let result = reload(&daemon, &config).await;
    assert_eq!(result["applied"], true);
    assert_eq!(result["needs_restart"], json!([]));
    wait_for_http(port).await;
    let client = reqwest::Client::new();
    let response = client
        .get(format!("http://127.0.0.1:{port}/admin/v1/providers"))
        .bearer_auth("reload-admin-token")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let occupied = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let mut invalid = config.clone();
    invalid.status_http.port = occupied.local_addr().unwrap().port();
    assert_eq!(reload(&daemon, &invalid).await["applied"], false);
    assert_eq!(
        client
            .get(format!("http://127.0.0.1:{port}/health"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let effective = expect_ok(daemon.call("config.show", Value::Null).await);
    assert_eq!(effective["effective_json"]["status_http"]["port"], port);

    let next_port = unused_tcp_port();
    let mut invalid = config.clone();
    invalid.status_http.port = next_port;
    let mut service = invalid.proxy_service[0].clone();
    service.id = "svc_reload_blocked".into();
    service.name = "reload-blocked".into();
    service.port = occupied.local_addr().unwrap().port();
    invalid.proxy_service.push(service);
    assert_eq!(reload(&daemon, &invalid).await["applied"], false);
    wait_closed(next_port).await;
    assert_eq!(
        client
            .get(format!("http://127.0.0.1:{port}/admin/v1/providers"))
            .bearer_auth("reload-admin-token")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    config.status_http.port = next_port;
    assert_eq!(reload(&daemon, &config).await["needs_restart"], json!([]));
    wait_for_http(next_port).await;
    wait_closed(port).await;
    config.status_http.host = "0.0.0.0".into();
    assert_eq!(reload(&daemon, &config).await["applied"], true);
    wait_for_http(next_port).await;
    config.status_http.host = "127.0.0.1".into();
    assert_eq!(reload(&daemon, &config).await["applied"], true);

    config.admin.rest.enabled = false;
    config.status_http.enabled = false;
    assert_eq!(reload(&daemon, &config).await["applied"], true);
    wait_closed(next_port).await;
    assert_eq!(daemon.child.id(), pid);
    assert_eq!(
        client
            .get(format!("http://127.0.0.1:{business_port}/v1/models"))
            .bearer_auth(daemon.api_key.as_ref().unwrap())
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    daemon.stop().await;
}

#[tokio::test]
async fn status_http_rebinding_preserves_admin_batch_reports() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let fixture = super::admin_rest::AdminFixture::start(false).await;
    let response = fixture.request(Method::POST, "/account-batches", Some(super::admin_rest::WRITE_TOKEN))
        .json(&json!({"provider":"kiro","auth":"api-key","accounts":[{"email":"reload@example.invalid","token":"ksk_reload_fixture"}]}))
        .send().await.unwrap();
    assert_eq!(response.status(), 202);
    let accepted: Value = response.json().await.unwrap();
    let id = accepted["data"]["id"].as_str().unwrap();
    let file = fixture.daemon.home().join("config.toml");
    let mut config: Config =
        toml::from_str(&tokio::fs::read_to_string(file).await.unwrap()).unwrap();
    let port = unused_tcp_port();
    config.status_http.port = port;
    assert_eq!(reload(&fixture.daemon, &config).await["applied"], true);
    wait_for_http(port).await;
    let response = reqwest::Client::new()
        .get(format!(
            "http://127.0.0.1:{port}/admin/v1/account-batches/{id}"
        ))
        .bearer_auth(super::admin_rest::WRITE_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.json::<Value>().await.unwrap()["data"]["id"], id);
    fixture.daemon.stop().await;
}
