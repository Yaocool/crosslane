//! Headless keys are read privately and use the existing account import RPC.

use std::process::Stdio;
use std::time::Duration;

use crosslane_ipc::protocol::{decode_line, encode_line, Request, Response};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::process::Command;

#[tokio::test]
async fn api_key_cli_reads_environment_or_stdin_without_echoing_the_secret() {
    for (stdin, unified) in [(false, false), (true, false), (false, true), (true, true)] {
        let workspace = tempfile::tempdir().unwrap();
        if unified {
            std::fs::write(workspace.path().join(".env"), "CROSSLANE_PROVIDER=kiro\n").unwrap();
        }
        let socket = workspace.path().join("admin.sock");
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
            assert_eq!(request.method, "account.import");
            let account = &request.params["accounts"][0];
            assert_eq!(account["email"], "ci@example.com");
            assert_eq!(account["credentials"]["access_token"], "ksk_cli-synthetic");
            assert_eq!(account["credentials"]["auth_method"], "api_key");
            assert_eq!(account["credentials"]["region"], "eu-central-1");
            assert_eq!(account["credentials"]["expires_at"], 0);
            assert!(account["credentials"].get("refresh_token").is_none());
            write
                .write_all(
                    encode_line(&Response::ok(
                        request.id,
                        json!({"imported":1,"skipped":[]}),
                    ))
                    .unwrap()
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let mut command = Command::new(env!("CARGO_BIN_EXE_crosslane"));
        command
            .current_dir(workspace.path())
            .env_remove("KIRO_API_KEY")
            .env_remove("CROSSLANE_PROVIDER")
            .env_remove("CROSSLANE_WRAPPER_LOCAL_ONLY")
            .arg("--socket")
            .arg(&socket)
            .args(if unified {
                &["account", "add", "--auth", "api-key"][..]
            } else {
                &["account", "add-api-key"][..]
            })
            .args(["--email", "ci@example.com", "--region", "eu-central-1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if stdin {
            command.arg(if unified {
                "--token-stdin"
            } else {
                "--key-stdin"
            });
        } else {
            command.env("KIRO_API_KEY", "ksk_cli-synthetic");
        }
        let mut child = command.spawn().unwrap();
        if stdin {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"ksk_cli-synthetic\n")
                .await
                .unwrap();
        }
        let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert!(!stdout.contains("ksk_cli-synthetic"));
        assert!(!stderr.contains("ksk_cli-synthetic"));
        server.await.unwrap();
    }
}
