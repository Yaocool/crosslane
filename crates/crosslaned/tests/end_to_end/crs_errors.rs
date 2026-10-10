use super::*;
use serde_json::{json, Value};
use std::sync::atomic::AtomicU16;

const FIELD_ERROR: &str = "messages.1.output_config: Extra inputs are not permitted";

async fn start_copilot_error() -> (MockServer, Daemon, u16) {
    start_copilot_error_with_status(Arc::new(AtomicU16::new(400))).await
}

async fn start_copilot_error_with_status(status: Arc<AtomicU16>) -> (MockServer, Daemon, u16) {
    let upstream = MockServer::start().await;
    for (endpoint, body) in [
        ("/user", json!({"id":7,"login":"crs-error-test"})),
        (
            "/copilot_internal/v2/token",
            json!({"token":"fake-api-token","expires_at":4_000_000_000i64,"endpoints":{"api":upstream.uri()}}),
        ),
        (
            "/models",
            json!({"data":[{"id":"claude-opus-5.5","capabilities":{"supported_endpoints":["/v1/messages"]}}]}),
        ),
    ] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&upstream)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(move |_: &wiremock::Request| {
            ResponseTemplate::new(status.load(Ordering::SeqCst)).set_body_json(json!({
                "type":"error",
                "error":{"type":"invalid_request_error","message":FIELD_ERROR}
            }))
        })
        .mount(&upstream)
        .await;
    let port = unused_tcp_port();
    let daemon = Daemon::start_copilot(port, &upstream.uri()).await;
    expect_ok(
        daemon
            .call(
                "v2.account.importToken",
                json!({
                    "provider":"copilot","token":"fake-github-token"
                }),
            )
            .await,
    );
    (upstream, daemon, port)
}

#[tokio::test]
async fn copilot_error_statuses_are_preserved_for_both_protocols_and_modes() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let status = Arc::new(AtomicU16::new(400));
    let (_upstream, daemon, port) = start_copilot_error_with_status(Arc::clone(&status)).await;
    let client = reqwest::Client::new();
    for code in [
        400, 401, 402, 403, 404, 408, 409, 413, 422, 429, 500, 502, 503, 504, 529, 599,
    ] {
        status.store(code, Ordering::SeqCst);
        for endpoint in ["messages", "chat/completions"] {
            for stream in [false, true] {
                let sse_error = cfg!(feature = "crs") && endpoint == "messages" && stream;
                let response = client
                    .post(format!("http://127.0.0.1:{port}/v1/{endpoint}"))
                    .bearer_auth(daemon.api_key.as_deref().unwrap())
                    .header("user-agent", "claude-cli/2.1.295 (external, e2e)")
                    .json(
                        &json!({"model":"copilot/claude-opus-5-5","max_tokens":16,"stream":stream,
                        "messages":[{"role":"user","content":"hello"}]}),
                    )
                    .send()
                    .await
                    .unwrap();
                assert_eq!(
                    response.status().as_u16(),
                    if sse_error { 200 } else { code },
                    "{endpoint} stream={stream}"
                );
                let text = response.text().await.unwrap();
                let body: Value = if sse_error {
                    sse_error_payload(&text)
                } else {
                    serde_json::from_str(&text).unwrap()
                };
                assert!(body["error"]["message"].is_string(), "{body}");
                if cfg!(feature = "crs") && endpoint == "messages" {
                    assert_eq!(body["error"]["status"], code);
                    assert_eq!(body["error"]["upstream_status"], code);
                    assert_eq!(body["error"]["detail"], FIELD_ERROR);
                }
            }
        }
    }
    daemon.stop().await;
}

async fn start_crs_console(daemon: &Daemon, port: u16) -> (Child, String) {
    let crs = std::env::var("CROSSLANE_CRS_SOURCE").expect("set CROSSLANE_CRS_SOURCE");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/crs_console.cjs");
    let mut relay = Command::new("node")
        .arg(script)
        .arg(crs)
        .arg(format!("http://127.0.0.1:{port}"))
        .arg(daemon.api_key.as_deref().unwrap())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("start CRS relay fixture");
    let mut output = BufReader::new(relay.stdout.take().unwrap()).lines();
    let address = tokio::time::timeout(Duration::from_secs(10), output.next_line())
        .await
        .expect("CRS fixture ready")
        .unwrap()
        .unwrap();
    (relay, address)
}

#[cfg(feature = "crs")]
#[tokio::test]
#[ignore = "requires CROSSLANE_CRS_SOURCE; executes local source and loopback services only"]
async fn supplier_status_and_stream_diagnostics_survive_unmodified_crs_console() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let status = Arc::new(AtomicU16::new(400));
    let (_upstream, daemon, port) = start_copilot_error_with_status(Arc::clone(&status)).await;
    let (mut relay, address) = start_crs_console(&daemon, port).await;
    let client = reqwest::Client::new();
    for code in [
        400, 401, 402, 403, 404, 408, 409, 413, 422, 429, 500, 502, 503, 504, 529, 599,
    ] {
        status.store(code, Ordering::SeqCst);
        for stream in [false, true] {
            let response = client
                .post(format!("{address}/v1/messages"))
                .header("user-agent", "claude-cli/2.1.295 (external, e2e)")
                .json(
                    &json!({"model":"copilot/claude-opus-5-5","max_tokens":16,"stream":stream,
                    "messages":[{"role":"user","content":"hello"}]}),
                )
                .send()
                .await
                .unwrap();
            let actual = response.status().as_u16();
            let text = response.text().await.unwrap();
            assert_eq!(
                actual,
                if stream { 200 } else { code },
                "stream={stream}: {text}"
            );
            if stream {
                let body = sse_error_payload(&text);
                assert_eq!(body["error"]["status"], code);
                assert_eq!(body["error"]["upstream_status"], code);
                assert_eq!(body["error"]["detail"], FIELD_ERROR);
                assert!(body["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains(body["request_id"].as_str().unwrap()));
            } else {
                // Unmodified Console sanitizes non-2xx bodies, independently
                // of Crosslane's rich diagnostic JSON and original status.
                let body: Value = serde_json::from_str(&text).unwrap();
                assert!(body["error"]["message"].is_string(), "{body}");
                assert!(body.get("request_id").is_none(), "{body}");
                assert!(body["error"].get("detail").is_none(), "{body}");
                assert!(!text.contains(FIELD_ERROR), "{text}");
            }
        }
    }
    relay.kill().await.unwrap();
    daemon.stop().await;
}

#[tokio::test]
async fn copilot_errors_preserve_crs_diagnostics_and_report_build_features() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let version = Command::new(env!("CARGO_BIN_EXE_crosslaned"))
        .arg("--version")
        .output()
        .await
        .unwrap();
    assert!(version.status.success());
    let version = String::from_utf8(version.stdout).unwrap();
    assert!(version.contains(&format!("crs={}", cfg!(feature = "crs"))));
    assert!(version.contains(&format!("sso={}", cfg!(feature = "sso"))));

    let (upstream, daemon, port) = start_copilot_error().await;
    let client = reqwest::Client::new();
    for stream in [false, true] {
        let response = client
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .bearer_auth(daemon.api_key.as_deref().unwrap())
            .header("user-agent", "claude-cli/2.1.295 (external, cli)")
            .json(
                &json!({"model":"claude-opus-5-5","max_tokens":16,"stream":stream,
                "messages":[{"role":"user","content":"hello"}]}),
            )
            .send()
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.text().await.unwrap();
        let sse_error = cfg!(feature = "crs") && stream;
        assert_eq!(status, if sse_error { 200 } else { 400 }, "{body}");
        assert_eq!(
            headers["content-type"],
            if sse_error {
                "text/event-stream"
            } else {
                "application/json"
            }
        );
        let error: Value = if sse_error {
            sse_error_payload(&body)
        } else {
            serde_json::from_str(&body).unwrap()
        };
        assert_eq!(error["error"]["type"], "invalid_request_error");
        if cfg!(feature = "crs") {
            assert_eq!(error["error"]["detail"], FIELD_ERROR);
            assert_eq!(error["error"]["stage"], "provider_upstream");
            assert_eq!(error["error"]["upstream_status"], 400);
            assert_eq!(error["error"]["retryable"], false);
            let message = error["error"]["message"].as_str().unwrap();
            assert!(message.starts_with(FIELD_ERROR));
            assert!(message.contains(error["request_id"].as_str().unwrap()));
        } else {
            assert_eq!(error["error"]["message"], FIELD_ERROR);
        }
    }
    // This probe is entirely local, including when no model was supplied.
    let probe = client
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .bearer_auth(daemon.api_key.as_deref().unwrap())
        .json(&json!({"stream":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        probe.status().as_u16(),
        if cfg!(feature = "crs") { 200 } else { 400 }
    );
    let unauthorized = client
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .json(&json!({"stream":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);
    assert_eq!(
        upstream
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == "/v1/messages")
            .count(),
        2
    );
    daemon.stop().await;
}

#[tokio::test]
#[ignore = "requires CROSSLANE_CRS_SOURCE and CROSSLANE_CLAUDE_CODE_BIN; executes local clients only"]
async fn claude_code_displays_copilot_error_through_unmodified_crs_console() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let claude = std::env::var("CROSSLANE_CLAUDE_CODE_BIN").expect("set CROSSLANE_CLAUDE_CODE_BIN");
    let (_upstream, daemon, port) = start_copilot_error().await;
    let (mut relay, address) = start_crs_console(&daemon, port).await;
    let isolated = tempfile::tempdir().unwrap();
    let mut client = Command::new(claude);
    client.env_clear();
    for name in ["PATH", "HOME", "TMPDIR"] {
        if let Some(value) = std::env::var_os(name) {
            client.env(name, value);
        }
    }
    client
        .current_dir(isolated.path())
        .env("ANTHROPIC_BASE_URL", &address)
        .env("ANTHROPIC_API_KEY", "local-fake-key")
        .env("CLAUDE_CONFIG_DIR", isolated.path().join("claude"))
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("DISABLE_TELEMETRY", "1")
        .env("DISABLE_ERROR_REPORTING", "1")
        .args([
            "--bare",
            "--print",
            "--output-format",
            "json",
            "--tools",
            "",
            "--setting-sources",
            "",
            "--no-session-persistence",
            "--max-turns",
            "1",
            "--model",
            "claude-opus-5-5",
            "hello",
        ])
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(30), client.output())
        .await
        .expect("Claude Code completed")
        .unwrap();
    assert!(!output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "Claude Code result: {error}; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert_eq!(result["is_error"], true, "{result}");
    assert_eq!(result["terminal_reason"], "api_error", "{result}");
    assert_eq!(result["total_cost_usd"], 0, "{result}");
    assert_eq!(result["usage"]["input_tokens"], 0, "{result}");
    assert_eq!(result["usage"]["output_tokens"], 0, "{result}");
    let message = result["result"].as_str().unwrap();
    if cfg!(feature = "crs") {
        assert!(message.contains(FIELD_ERROR), "{message}");
        assert!(message.contains("upstream HTTP 400"), "{message}");
        assert!(message.contains("stage: provider_upstream"), "{message}");
        assert!(message.contains("request ID: trace_"), "{message}");
        let requests: Value = reqwest::get(format!("{address}/__requests"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            requests,
            json!([{"stream":true}]),
            "no non-streaming fallback"
        );
    } else {
        assert_eq!(message, "API Error: 400 Internal server error");
    }
    relay.kill().await.unwrap();
    relay.wait().await.unwrap();
    daemon.stop().await;
}
