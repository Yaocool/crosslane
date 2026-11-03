use super::admin_rest::{AdminFixture, WRITE_TOKEN};
use super::*;
use crosslane_ipc::protocol::method;
use reqwest::Method;
use serde_json::{json, Value};

async fn credential_gate(fixture: &AdminFixture) -> u16 {
    // Invalid region fails before AWS transport. 400 proves credentials were
    // loaded; 503 proves they were disabled/missing. Never contact real AWS.
    fixture
        .request(
            Method::GET,
            "/kiro/enterprise/profiles?region=invalid",
            Some(WRITE_TOKEN),
        )
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test]
async fn kiro_credentials_bootstrap_hot_applies_without_environment_or_restart() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let fixture = AdminFixture::start(false).await;
    let pid = fixture.daemon.child.id();
    let file = fixture.daemon.home().join("config.toml");
    let config = std::fs::read(&file).unwrap();
    assert_eq!(credential_gate(&fixture).await, 503);
    expect_ok(fixture.daemon.call(method::ADMIN_KIRO_CREDENTIALS_CONFIGURE,
        json!({"access_key_id":"AKIA000000000EXAMPLE","secret_access_key":"fixture-private-bootstrap"})).await);
    assert_eq!(credential_gate(&fixture).await, 400);
    let result = expect_ok(fixture.daemon.call(method::ADMIN_KIRO_CREDENTIALS_CONFIGURE,
        json!({"access_key_id":"ASIA000000000EXAMPLE","secret_access_key":"fixture-private-rotated","session_token":"fixture-private-rotated-session"})).await);
    assert_eq!(result["has_session_token"], true);
    assert_eq!(credential_gate(&fixture).await, 400);
    assert_eq!(fixture.daemon.child.id(), pid);
    assert_eq!(std::fs::read(file).unwrap(), config);
    fixture.daemon.stop().await;
}

#[tokio::test]
async fn kiro_credentials_hot_reload_is_private_persistent_and_fails_closed() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mut fixture = AdminFixture::start(false).await;
    assert_eq!(credential_gate(&fixture).await, 503);
    let environment = [
        ("CROSSLANE_KIRO_ADMIN_ACCESS_KEY_ID", "AKIA000000000EXAMPLE"),
        (
            "CROSSLANE_KIRO_ADMIN_SECRET_ACCESS_KEY",
            "fixture-private-env-secret",
        ),
    ];
    fixture.restart_with_environment(&environment).await;
    assert_eq!(credential_gate(&fixture).await, 400);
    let pid = fixture.daemon.child.id();
    let config_file = fixture.daemon.home().join("config.toml");
    let before = std::fs::read(&config_file).unwrap();
    let first = json!({"access_key_id":"AKIA111111111EXAMPLE","secret_access_key":"fixture-private-file-secret","region":"eu-west-1"});
    let result = expect_ok(
        fixture
            .daemon
            .call(method::ADMIN_KIRO_CREDENTIALS_CONFIGURE, first.clone())
            .await,
    );
    assert_eq!(result["source"], "encrypted_file");
    assert_eq!(result["region"], "eu-west-1");
    assert_eq!(result["configured"], true);
    assert_eq!(credential_gate(&fixture).await, 400);
    let file = fixture.daemon.home().join(".secrets/kiro-admin.enc");
    let encrypted = std::fs::read(&file).unwrap();
    for params in [
        json!({"access_key_id":"ASIA111111111EXAMPLE","secret_access_key":"fixture-invalid-private-sts"}),
        json!({"access_key_id":"AKIA111111111EXAMPLE","secret_access_key":["fixture-invalid-private-type"]}),
        json!({"access_key_id":"AKIA111111111EXAMPLE","secret_access_key":"fixture-private-file-secret","unexpected":"fixture-private-unknown"}),
        json!({"access_key_id":"AKIA111111111EXAMPLE","secret_access_key":"fixture-private-file-secret","profile_arn":"arn:aws:"}),
        json!({"access_key_id":"AKIA111111111EXAMPLE","secret_access_key":"fixture-private-file-secret","application_arn":"arn:aws:codewhisperer:us-east-1:123456789012:profile/fixture"}),
    ] {
        let response = fixture
            .daemon
            .call(method::ADMIN_KIRO_CREDENTIALS_CONFIGURE, params)
            .await;
        assert!(matches!(response, Response::Err { .. }));
        let response = serde_json::to_string(&response).unwrap();
        for secret in [
            "fixture-invalid-private-sts",
            "fixture-invalid-private-type",
            "fixture-private-unknown",
        ] {
            assert!(!response.contains(secret));
        }
        assert_eq!(std::fs::read(&file).unwrap(), encrypted);
    }
    let rotated = json!({"access_key_id":"ASIA222222222EXAMPLE","secret_access_key":"fixture-rotated-file-secret","session_token":"fixture-private-session"});
    let result = expect_ok(
        fixture
            .daemon
            .call(method::ADMIN_KIRO_CREDENTIALS_CONFIGURE, rotated.clone())
            .await,
    );
    assert_eq!(result["has_session_token"], true);
    assert_eq!(result["region"], "us-east-1");
    assert_eq!(credential_gate(&fixture).await, 400);
    let mut corrupt = std::fs::read(&file).unwrap();
    *corrupt.last_mut().unwrap() ^= 1;
    std::fs::write(&file, corrupt).unwrap();
    assert_eq!(
        credential_gate(&fixture).await,
        503,
        "corrupt files cannot reactivate valid environment keys"
    );
    assert!(matches!(
        fixture
            .daemon
            .call(method::ADMIN_KIRO_CREDENTIALS_SHOW, Value::Null)
            .await,
        Response::Err { .. }
    ));
    expect_ok(
        fixture
            .daemon
            .call(method::ADMIN_KIRO_CREDENTIALS_CONFIGURE, rotated)
            .await,
    );
    assert_eq!(credential_gate(&fixture).await, 400);
    assert_eq!(fixture.daemon.child.id(), pid);
    fixture.restart().await;
    let saved = std::fs::read(&file).unwrap();
    assert!(matches!(
        fixture
            .daemon
            .call(method::ADMIN_KIRO_CREDENTIALS_USE_ENV, Value::Null)
            .await,
        Response::Err { .. }
    ));
    assert_eq!(std::fs::read(&file).unwrap(), saved);
    let result = expect_ok(
        fixture
            .daemon
            .call(method::ADMIN_KIRO_CREDENTIALS_SHOW, Value::Null)
            .await,
    );
    assert_eq!(result["source"], "encrypted_file");
    assert_eq!(result["has_session_token"], true);
    assert_eq!(credential_gate(&fixture).await, 400);
    let pid = fixture.daemon.child.id();
    let result = expect_ok(
        fixture
            .daemon
            .call(method::ADMIN_KIRO_CREDENTIALS_CLEAR, Value::Null)
            .await,
    );
    assert_eq!(result["source"], "disabled");
    assert_eq!(credential_gate(&fixture).await, 503);
    assert_eq!(fixture.daemon.child.id(), pid);
    fixture.restart_with_environment(&environment).await;
    assert_eq!(credential_gate(&fixture).await, 503);
    fixture
        .restart_with_environment(&[
            ("CROSSLANE_KIRO_ADMIN_ACCESS_KEY_ID", "ASIA000000000EXAMPLE"),
            (
                "CROSSLANE_KIRO_ADMIN_SECRET_ACCESS_KEY",
                "fixture-invalid-sts-env",
            ),
        ])
        .await;
    assert!(matches!(
        fixture
            .daemon
            .call(method::ADMIN_KIRO_CREDENTIALS_USE_ENV, Value::Null)
            .await,
        Response::Err { .. }
    ));
    let disabled = expect_ok(
        fixture
            .daemon
            .call(method::ADMIN_KIRO_CREDENTIALS_SHOW, Value::Null)
            .await,
    );
    assert_eq!(disabled["source"], "disabled");
    fixture.restart_with_environment(&environment).await;
    let result = expect_ok(
        fixture
            .daemon
            .call(method::ADMIN_KIRO_CREDENTIALS_USE_ENV, Value::Null)
            .await,
    );
    assert_eq!(result["source"], "environment");
    assert_eq!(credential_gate(&fixture).await, 400);
    for path in ["/kiro/credentials", "/kiro/enterprise/credentials"] {
        assert_eq!(
            fixture
                .request(Method::POST, path, Some(WRITE_TOKEN))
                .json(&first)
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
    }
    let config = expect_ok(fixture.daemon.call(method::CONFIG_SHOW, Value::Null).await);
    let logs = std::fs::read_to_string(fixture.daemon.home().join("rest-test.log")).unwrap();
    for secret in [
        "AKIA000000000EXAMPLE",
        "AKIA111111111EXAMPLE",
        "ASIA222222222EXAMPLE",
        "fixture-private-env-secret",
        "fixture-private-file-secret",
        "fixture-rotated-file-secret",
        "fixture-private-session",
    ] {
        assert!(!serde_json::to_string(&result).unwrap().contains(secret));
        assert!(!serde_json::to_string(&config).unwrap().contains(secret));
        assert!(!logs.contains(secret));
    }
    assert_eq!(std::fs::read(config_file).unwrap(), before);
    fixture.daemon.stop().await;
}
