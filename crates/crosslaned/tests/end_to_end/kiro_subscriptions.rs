use super::*;
use crosslane_ipc::protocol::method;
use serde_json::{json, Value};

async fn socket_only_daemon() -> Daemon {
    let home = tempfile::tempdir().unwrap();
    let socket = home.path().join("admin.sock");
    let mut config = Config::default();
    config.admin.socket = socket.to_string_lossy().into_owned();
    config.status_http.enabled = false;
    config.admin.rest.enabled = false;
    tokio::fs::write(
        home.path().join("config.toml"),
        toml::to_string(&config).unwrap(),
    )
    .await
    .unwrap();
    let log = std::fs::File::create(home.path().join("kiro-cli.log")).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_crosslaned"));
    for name in [
        "ACCESS_KEY_ID",
        "SECRET_ACCESS_KEY",
        "SESSION_TOKEN",
        "REGION",
        "PROFILE_ARN",
        "APPLICATION_ARN",
        "IDENTITY_CENTER_REGION",
    ] {
        command.env_remove(format!("CROSSLANE_KIRO_ADMIN_{name}"));
    }
    let child = command
        .current_dir(home.path())
        .env("CROSSLANE_HOME", home.path())
        .env("CROSSLANE_DISABLE_HTTP", "1")
        .env("RUST_LOG", "info")
        .stdout(log)
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    wait_for_socket(&socket).await;
    Daemon {
        child,
        socket,
        home,
        api_key: None,
    }
}

fn error_details(response: Response, code: i32) -> Value {
    let Response::Err { error, .. } = response else {
        panic!("expected RPC error")
    };
    assert_eq!(error.code, code);
    serde_json::from_str(&error.message).unwrap()
}

#[tokio::test]
async fn kiro_subscriptions_work_without_rest_and_use_hot_loaded_private_credentials() {
    let daemon = socket_only_daemon().await;
    let pid = daemon.child.id();
    let config = std::fs::read(daemon.home().join("config.toml")).unwrap();
    let plans = expect_ok(
        daemon
            .call(method::ADMIN_KIRO_SUBSCRIPTION_PLANS, Value::Null)
            .await,
    );
    assert_eq!(plans.as_array().unwrap().len(), 4);
    assert_eq!(plans[3]["plan"], "power");

    for (method, params) in [
        (method::ADMIN_KIRO_SUBSCRIPTION_PROFILES, json!({})),
        (method::ADMIN_KIRO_SUBSCRIPTION_SCOPE, json!({})),
        (method::ADMIN_KIRO_SUBSCRIPTION_USERS, json!({})),
        (method::ADMIN_KIRO_SUBSCRIPTION_LIST, json!({})),
        (
            method::ADMIN_KIRO_SUBSCRIPTION_SHOW,
            json!({"user":"fixture@example.com"}),
        ),
        (
            method::ADMIN_KIRO_SUBSCRIPTION_ADD,
            json!({"user":"fixture@example.com","plan":"power"}),
        ),
        (
            method::ADMIN_KIRO_SUBSCRIPTION_UPGRADE,
            json!({"user":"fixture@example.com","plan":"power"}),
        ),
        (
            method::ADMIN_KIRO_SUBSCRIPTION_DOWNGRADE,
            json!({"user":"fixture@example.com","plan":"pro","dry_run":true}),
        ),
        (
            method::ADMIN_KIRO_SUBSCRIPTION_CANCEL,
            json!({"user":"fixture@example.com"}),
        ),
    ] {
        let error = error_details(daemon.call(method, params).await, 503);
        assert_eq!(error["code"], "kiro_admin_not_configured");
        assert_eq!(error["write_outcome"], "not_submitted");
    }

    expect_ok(daemon.call(method::ADMIN_KIRO_CREDENTIALS_CONFIGURE,
        json!({"access_key_id":"AKIA000000000EXAMPLE","secret_access_key":"fixture-private-kiro-cli"})).await);
    // Invalid region fails in the shared context constructor before transport.
    // The transition from 503 to 400 proves encrypted credentials hot-applied.
    for (method, params) in [
        (
            method::ADMIN_KIRO_SUBSCRIPTION_PROFILES,
            json!({"scope":{"region":"invalid"}}),
        ),
        (
            method::ADMIN_KIRO_SUBSCRIPTION_SCOPE,
            json!({"scope":{"region":"invalid"}}),
        ),
        (
            method::ADMIN_KIRO_SUBSCRIPTION_USERS,
            json!({"scope":{"region":"invalid"}}),
        ),
        (
            method::ADMIN_KIRO_SUBSCRIPTION_LIST,
            json!({"scope":{"region":"invalid"}}),
        ),
        (
            method::ADMIN_KIRO_SUBSCRIPTION_SHOW,
            json!({"user":"fixture@example.com","scope":{"region":"invalid"}}),
        ),
        (
            method::ADMIN_KIRO_SUBSCRIPTION_ADD,
            json!({"user":"fixture@example.com","plan":"power","dry_run":true,"scope":{"region":"invalid"}}),
        ),
        (
            method::ADMIN_KIRO_SUBSCRIPTION_UPGRADE,
            json!({"user":"fixture@example.com","plan":"power","scope":{"region":"invalid"}}),
        ),
        (
            method::ADMIN_KIRO_SUBSCRIPTION_DOWNGRADE,
            json!({"user":"fixture@example.com","plan":"pro","scope":{"region":"invalid"}}),
        ),
        (
            method::ADMIN_KIRO_SUBSCRIPTION_CANCEL,
            json!({"user":"fixture@example.com","mode":"immediate","dry_run":true,"scope":{"region":"invalid"}}),
        ),
    ] {
        let error = error_details(daemon.call(method, params).await, 400);
        assert_eq!(error["code"], "invalid_request");
        assert_eq!(error["write_outcome"], "not_submitted");
    }
    assert_eq!(daemon.child.id(), pid);
    assert_eq!(
        std::fs::read(daemon.home().join("config.toml")).unwrap(),
        config
    );
    expect_ok(
        daemon
            .call(method::ADMIN_KIRO_CREDENTIALS_CLEAR, Value::Null)
            .await,
    );
    error_details(
        daemon
            .call(method::ADMIN_KIRO_SUBSCRIPTION_LIST, json!({}))
            .await,
        503,
    );

    let log_path = daemon.home().join("kiro-cli.log");
    let deadline = Instant::now() + Duration::from_secs(5);
    let logs = loop {
        let logs = tokio::fs::read_to_string(&log_path).await.unwrap();
        if logs.contains("admin.kiro.rpc.request") {
            break logs;
        }
        assert!(Instant::now() < deadline, "CLI audit event missing");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let events = logs
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect::<Vec<_>>();
    assert!(events
        .iter()
        .any(|event| event["fields"]["event"] == "admin.kiro.rpc.request"
            && event["fields"]["source"] == "unix_socket"
            && event["fields"]["role"] == "admin"));
    assert!(!logs.contains("fixture-private-kiro-cli"));
    daemon.stop().await;
}

#[tokio::test]
async fn kiro_subscription_rpc_rejects_unknown_fields_and_plans_before_credentials() {
    let daemon = socket_only_daemon().await;
    for params in [
        json!({"user":"fixture","plan":"invalid"}),
        json!({"user":"fixture","plan":"pro","assign_application":true}),
        json!({"user":"fixture","plan":"pro","secret_access_key":"fixture-input-secret"}),
        json!({"user":"fixture","plan":"pro","scope":{"unexpected":"fixture-input-secret"}}),
    ] {
        for method in [
            method::ADMIN_KIRO_SUBSCRIPTION_ADD,
            method::ADMIN_KIRO_SUBSCRIPTION_UPGRADE,
            method::ADMIN_KIRO_SUBSCRIPTION_DOWNGRADE,
        ] {
            let response = daemon.call(method, params.clone()).await;
            let Response::Err { error, .. } = response else {
                panic!("expected error")
            };
            assert_eq!(error.code, 400);
            assert!(!error.message.contains("fixture-input-secret"));
            assert_eq!(error.message, "invalid Kiro subscription parameters");
        }
    }
    for params in [
        json!({"user":"fixture","mode":"invalid"}),
        json!({"user":"fixture","plan":"pro"}),
        json!({"user":"fixture","dry_run":"true"}),
    ] {
        let Response::Err { error, .. } = daemon
            .call(method::ADMIN_KIRO_SUBSCRIPTION_CANCEL, params)
            .await
        else {
            panic!("expected error")
        };
        assert_eq!(error.code, 400);
        assert_eq!(error.message, "invalid Kiro subscription parameters");
    }
    for key in ["profile_arn", "application_arn", "identity_center_region"] {
        let Response::Err { error, .. } = daemon
            .call(
                method::ADMIN_KIRO_SUBSCRIPTION_PROFILES,
                json!({"scope":{key:"fixture-input-secret"}}),
            )
            .await
        else {
            panic!("expected error")
        };
        assert_eq!(error.code, 400);
        assert!(error
            .message
            .contains("profile discovery accepts only region and legacy"));
        assert!(!error.message.contains("fixture-input-secret"));
    }
    daemon.stop().await;
}
