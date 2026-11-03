#![cfg(unix)]
use crosslane_ipc::protocol::{decode_line, encode_line, method, Request, Response};
use serde_json::{json, Value};
use std::process::{Output, Stdio};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::process::Command;

fn status() -> Value {
    json!({"configured":true,"source":"encrypted_file","credentials_file":"/private/.secrets/kiro-admin.enc","region":"us-east-1","has_session_token":true,"profile_arn":null,"application_arn":null,"identity_center_region":null})
}

async fn cli(args: &[&str], input: Option<&str>, expected: &'static str) -> Output {
    cli_with_warning(args, input, expected, None).await
}

async fn cli_with_warning(
    args: &[&str],
    input: Option<&str>,
    expected: &'static str,
    warning: Option<&str>,
) -> Output {
    let home = tempfile::tempdir().unwrap();
    let socket = home.path().join("admin.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let configuring = input.is_some();
    let mut response = status();
    if let Some(warning) = warning {
        response["persistence_warning"] = warning.into();
    }
    let server = tokio::spawn(async move {
        let (stream, _) =
            tokio::time::timeout(std::time::Duration::from_secs(10), listener.accept())
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
        assert_eq!(request.method, expected);
        if configuring {
            assert_eq!(request.params["access_key_id"], "ASIA000000000EXAMPLE");
            assert_eq!(
                request.params["secret_access_key"],
                "private-cli-fixture-secret"
            );
            assert_eq!(
                request.params["session_token"],
                "private-cli-fixture-session"
            );
            assert_eq!(request.params["region"], "us-east-1");
        } else {
            assert_eq!(request.params, Value::Null);
        }
        write
            .write_all(
                encode_line(&Response::ok(request.id, response))
                    .unwrap()
                    .as_bytes(),
            )
            .await
            .unwrap();
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_crosslane"))
        .current_dir(home.path())
        .env("CROSSLANE_PROVIDER", "copilot")
        .env_remove("CROSSLANE_WRAPPER_LOCAL_ONLY")
        .arg("--socket")
        .arg(socket)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(input) = input {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .await
            .unwrap();
    }
    let output = child.wait_with_output().await.unwrap();
    server.await.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for secret in [
        "ASIA000000000EXAMPLE",
        "private-cli-fixture-secret",
        "private-cli-fixture-session",
    ] {
        assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
    }
    output
}

#[tokio::test]
async fn kiro_admin_credentials_use_stdin_and_only_return_safe_metadata() {
    let input = r#"{"access_key_id":"ASIA000000000EXAMPLE","secret_access_key":"private-cli-fixture-secret","session_token":"private-cli-fixture-session"}"#;
    let output = cli(
        &["--json", "admin", "kiro", "configure", "--stdin"],
        Some(input),
        method::ADMIN_KIRO_CREDENTIALS_CONFIGURE,
    )
    .await;
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result, status());
    for (command, rpc) in [
        ("show", method::ADMIN_KIRO_CREDENTIALS_SHOW),
        ("clear", method::ADMIN_KIRO_CREDENTIALS_CLEAR),
        ("use-env", method::ADMIN_KIRO_CREDENTIALS_USE_ENV),
    ] {
        cli(&["admin", "kiro", command], None, rpc).await;
    }
}

#[tokio::test]
async fn invalid_kiro_credential_input_never_echoes_values_or_contacts_the_daemon() {
    let home = tempfile::tempdir().unwrap();
    for input in [
        r#"{"access_key_id":"private-input-fixture","secret_access_key":{"private-input-fixture":1}}"#,
        r#"{"access_key_id":"private-input-fixture","secret_access_key":"private-input-fixture","extra":1}"#,
        r#"{"access_key_id":"private-input-fixture""#,
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_crosslane"))
            .current_dir(home.path())
            .env_remove("CROSSLANE_WRAPPER_LOCAL_ONLY")
            .args([
                "--socket",
                "/missing-kiro-fixture.sock",
                "admin",
                "kiro",
                "configure",
                "--stdin",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .await
            .unwrap();
        let output = child.wait_with_output().await.unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("JSON 无效"), "{error}");
        assert!(!error.contains("private-input-fixture"));
        assert!(!error.contains("无法连接"));
    }
}

#[test]
fn kiro_credential_help_is_available_without_environment_or_daemon() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join(".env"), "INVALID=\"unterminated").unwrap();
    for args in [
        vec!["admin", "kiro"],
        vec!["admin", "kiro", "help", "configure"],
        vec!["help", "admin", "kiro", "configure"],
        vec!["admin", "kiro", "configure", "--help"],
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_crosslane"))
            .current_dir(home.path())
            .env_remove("CROSSLANE_WRAPPER_LOCAL_ONLY")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let help = String::from_utf8_lossy(&output.stdout);
        assert!(help.contains("configure") && help.contains("--stdin"));
        assert!(!help.contains("--secret-access-key"));
    }
}

#[tokio::test]
async fn active_but_unsynced_credential_changes_preserve_the_warning_in_json_and_terminal_output() {
    let input = r#"{"access_key_id":"ASIA000000000EXAMPLE","secret_access_key":"private-cli-fixture-secret","session_token":"private-cli-fixture-session"}"#;
    let warning = "change is active; crash durability was not confirmed";
    let output = cli_with_warning(
        &["--json", "admin", "kiro", "configure", "--stdin"],
        Some(input),
        method::ADMIN_KIRO_CREDENTIALS_CONFIGURE,
        Some(warning),
    )
    .await;
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["persistence_warning"], warning);
    assert_eq!(result["configured"], true);
    let output = cli_with_warning(
        &["admin", "kiro", "configure", "--stdin"],
        Some(input),
        method::ADMIN_KIRO_CREDENTIALS_CONFIGURE,
        Some(warning),
    )
    .await;
    assert!(String::from_utf8_lossy(&output.stderr).contains(warning));
}
