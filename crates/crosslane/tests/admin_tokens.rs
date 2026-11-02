//! Native CLI contracts: trusted socket bootstrap and one-time secret output.
#![cfg(unix)]

use std::process::Output;
use std::time::Duration;

use crosslane_ipc::protocol::{decode_line, encode_line, method, Request, Response};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::process::Command;

fn metadata() -> Value {
    json!({"id":"adm_0123456789abcdef","name":"ops","role":"admin","enabled":true,"created_at":1})
}

async fn run(
    args: &[&str],
    expected_method: &'static str,
    expected_params: Value,
    result: Value,
) -> Output {
    let home = tempfile::tempdir().unwrap();
    let socket = home.path().join("admin.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let (read, mut write) = stream.into_split();
        let line = BufReader::new(read)
            .lines()
            .next_line()
            .await
            .unwrap()
            .unwrap();
        let request: Request = decode_line(&line).unwrap();
        assert_eq!(request.method, expected_method);
        assert_eq!(request.params, expected_params);
        write
            .write_all(
                encode_line(&Response::ok(request.id, result))
                    .unwrap()
                    .as_bytes(),
            )
            .await
            .unwrap();
    });
    let output = Command::new(env!("CARGO_BIN_EXE_crosslane"))
        .current_dir(home.path())
        .env_remove("CROSSLANE_WRAPPER_LOCAL_ONLY")
        .env("CROSSLANE_PROVIDER", "copilot")
        .arg("--socket")
        .arg(&socket)
        .args(args)
        .output()
        .await
        .unwrap();
    server.await.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[tokio::test]
async fn cli_creates_and_rotates_without_bearer_or_credentials_in_argv() {
    for (args, name, params) in [
        (
            vec!["admin", "token", "create", "--name", "ops", "--enable-rest"],
            method::ADMIN_TOKEN_CREATE,
            json!({"name":"ops","role":"admin","enable_rest":true}),
        ),
        (
            vec![
                "admin",
                "token",
                "add",
                "--name",
                "ops",
                "--role",
                "read-only",
            ],
            method::ADMIN_TOKEN_CREATE,
            json!({"name":"ops","role":"read_only","enable_rest":false}),
        ),
        (
            vec!["admin", "token", "rotate", "ops"],
            method::ADMIN_TOKEN_ROTATE,
            json!({"id":"ops"}),
        ),
    ] {
        let output=run(&args,name,params,json!({"token":metadata(),"secret":"sk-admin-cli-fixture","rest_enabled":true,"listener_enabled":true})).await;
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(stdout.matches("sk-admin-cli-fixture").count(), 1);
        assert!(!String::from_utf8_lossy(&output.stderr).contains("sk-admin-cli-fixture"));
    }
}

#[tokio::test]
async fn cli_lists_shows_and_changes_state_without_printing_secrets_or_hashes() {
    for (args, name, params, list) in [
        (
            vec!["--json", "admin", "token", "list"],
            method::ADMIN_TOKEN_LIST,
            Value::Null,
            true,
        ),
        (
            vec!["--json", "admin", "token", "show", "ops"],
            method::ADMIN_TOKEN_SHOW,
            json!({"id":"ops"}),
            false,
        ),
        (
            vec!["admin", "token", "enable", "ops"],
            method::ADMIN_TOKEN_SET_ENABLED,
            json!({"id":"ops","enabled":true}),
            false,
        ),
        (
            vec!["admin", "token", "disable", "ops"],
            method::ADMIN_TOKEN_SET_ENABLED,
            json!({"id":"ops","enabled":false}),
            false,
        ),
        (
            vec!["admin", "token", "revoke", "ops"],
            method::ADMIN_TOKEN_REVOKE,
            json!({"id":"ops"}),
            false,
        ),
    ] {
        let mut token = metadata();
        token["token_sha256"] = json!("fixture-private-hash");
        let response = if list {
            json!({"tokens":[token],"rest_enabled":true,"listener_enabled":true,"secret":"fixture-private-secret"})
        } else {
            json!({"token":token,"rest_enabled":true,"listener_enabled":true,"secret":"fixture-private-secret"})
        };
        let output = run(&args, name, params, response).await;
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("ops"));
        assert!(!stdout.contains("fixture-private-secret"));
        assert!(!stdout.contains("fixture-private-hash"));
    }
}

#[test]
fn nested_token_help_is_local_and_rejects_provider_scope() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join(".env"), "BROKEN='unterminated\n").unwrap();
    for args in [
        vec!["admin"],
        vec!["admin", "token"],
        vec!["help", "admin", "token"],
        vec!["admin", "token", "create", "--help"],
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_crosslane"))
            .current_dir(home.path())
            .env_remove("CROSSLANE_PROVIDER")
            .args(&args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("token"));
    }
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_crosslane"))
        .current_dir(home.path())
        .args(["--provider", "copilot", "admin", "token", "list"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("不接受 --provider"));
}

#[tokio::test]
async fn disabled_admin_access_points_to_the_combined_config_editor() {
    for listener_enabled in [false, true] {
        let output = run(
            &["admin", "token", "list"],
            method::ADMIN_TOKEN_LIST,
            Value::Null,
            json!({"tokens":[metadata()],"rest_enabled":false,"listener_enabled":listener_enabled}),
        )
        .await;
        let hint = String::from_utf8(output.stderr).unwrap();
        assert!(hint.contains("crosslane config edit admin"));
        if !listener_enabled {
            assert!(hint.contains("[status_http] enabled=true"));
        }
        assert!(hint.contains("[admin.rest] enabled=true"));
        assert!(hint.contains("已有令牌"));
        assert!(hint.contains("待重启"));
    }
}

#[test]
fn admin_help_and_guides_explain_listener_dependencies_and_configuration_defaults() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join(".env"), "BROKEN='unterminated\n").unwrap();
    for args in [
        vec!["admin", "--help"],
        vec!["admin", "token", "--help"],
        vec!["admin", "token", "create", "--help"],
        vec!["config", "edit", "--help"],
        vec!["guide", "admin"],
        vec!["guide", "config"],
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_crosslane"))
            .current_dir(home.path())
            .args(&args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let help = String::from_utf8(output.stdout).unwrap();
        assert!(help.contains("config edit admin"), "{args:?}: {help}");
        assert!(help.contains("status_http"), "{args:?}: {help}");
        assert!(help.contains("重启"), "{args:?}: {help}");
    }
}
