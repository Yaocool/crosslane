use super::*;
use serde_json::{json, Value};

async fn configure(daemon: &Daemon, edit: impl FnOnce(&mut crosslane_core::config::Config)) {
    let path = daemon.home().join("config.toml");
    let mut config: crosslane_core::config::Config =
        toml::from_str(&tokio::fs::read_to_string(&path).await.unwrap()).unwrap();
    edit(&mut config);
    tokio::fs::write(path, toml::to_string(&config).unwrap())
        .await
        .unwrap();
    assert_eq!(
        expect_ok(daemon.call("config.reload", json!({})).await)["applied"],
        true
    );
}

async fn log_records(daemon: &Daemon) -> Vec<Value> {
    let mut records = Vec::new();
    let mut files = tokio::fs::read_dir(daemon.home().join("logs"))
        .await
        .unwrap();
    while let Some(file) = files.next_entry().await.unwrap() {
        if file.path().extension().is_some_and(|ext| ext == "log") {
            for line in tokio::fs::read_to_string(file.path())
                .await
                .unwrap()
                .lines()
            {
                if let Ok(value) = serde_json::from_str::<Value>(line) {
                    records.push(value);
                }
            }
        }
    }
    records
}

#[tokio::test]
async fn invalid_models_are_rejected_once_without_leaking_pasted_text() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    configure(&daemon, |config| config.log.level = "debug".into()).await;
    let client = reqwest::Client::new();
    let mut traces = Vec::new();
    for model in [
        "private_terminal_sentinel\nSELECT secret".into(),
        format!("private_terminal_sentinel{}", "x".repeat(300)),
    ] {
        for route in [
            "messages",
            "messages/count_tokens",
            "chat/completions",
            "responses",
        ] {
            let body = if route == "responses" {
                json!({"model":model,"input":"hello","store":false})
            } else {
                json!({"model":model,"max_tokens":1,"messages":[{"role":"user","content":"hello"}]})
            };
            let response = client
                .post(format!("http://127.0.0.1:{port}/v1/{route}"))
                .header("x-api-key", daemon.api_key.as_deref().unwrap())
                .header(
                    "user-agent",
                    if route.starts_with("messages") {
                        "claude-cli/2.1.260 (external, test)"
                    } else {
                        "codex_cli_rs/0.147.0"
                    },
                )
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 400);
            assert_eq!(
                response.headers()["x-crosslane-error-stage"],
                "request_validation"
            );
            traces.push(
                response.headers()["request-id"]
                    .to_str()
                    .unwrap()
                    .to_owned(),
            );
            let error_body = response.text().await.unwrap();
            assert!(
                error_body.contains("invalid value for model"),
                "{error_body}"
            );
            assert!(!error_body.contains("private_terminal_sentinel"));
        }
    }
    let stats = expect_ok(
        daemon
            .call("stats", json!({"detail":true,"recent":20}))
            .await,
    );
    assert!(!stats.to_string().contains("private_terminal_sentinel"));
    for request in stats["stats"]["recent_requests"].as_array().unwrap() {
        assert!(request["attempts"].as_array().is_none_or(Vec::is_empty));
        assert_eq!(request["credits"], 0.0);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let records = log_records(&daemon).await;
        assert!(!records
            .iter()
            .any(|record| record.to_string().contains("private_terminal_sentinel")));
        let counts = traces
            .iter()
            .map(|trace| {
                records
                    .iter()
                    .filter(|record| {
                        record["level"] == "WARN" && record["fields"]["trace_id"] == *trace
                    })
                    .count()
            })
            .collect::<Vec<_>>();
        assert!(
            counts.iter().all(|count| *count <= 1),
            "duplicate rejection warnings: {counts:?}"
        );
        if counts.iter().all(|count| *count == 1) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "missing rejection warnings: {counts:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    daemon.stop().await;
}

#[tokio::test]
async fn compaction_failure_accounting_does_not_duplicate_warnings_or_invent_credits() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    let scenario = Arc::new(AtomicUsize::new(0));
    let current = Arc::clone(&scenario);
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(move |request: &wiremock::Request| {
            let payload: Value = serde_json::from_slice(&request.body).unwrap();
            let summary = payload["conversationState"]["currentMessage"]["userInputMessage"]
                ["content"]
                .as_str()
                .unwrap()
                .contains("durable conversation checkpoint");
            if !summary {
                return ResponseTemplate::new(200)
                    .insert_header("content-type", "application/vnd.amazon.eventstream")
                    .set_body_bytes(generation_body("main request completed"));
            }
            match current.load(Ordering::SeqCst) {
                0 => ResponseTemplate::new(200)
                    .insert_header("content-type", "application/vnd.amazon.eventstream")
                    .set_body_bytes(generation_body("<summary>Late checkpoint.</summary>"))
                    .set_delay(Duration::from_millis(1_200)),
                1 => ResponseTemplate::new(200)
                    .insert_header("content-type", "application/vnd.amazon.eventstream")
                    .set_body_bytes(vec![0, 0, 0, 32, 0, 0, 0, 0]),
                2 => ResponseTemplate::new(200)
                    .insert_header("content-type", "application/vnd.amazon.eventstream")
                    .set_body_bytes(Vec::<u8>::new()),
                _ => unreachable!(),
            }
        })
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    let client = reqwest::Client::new();
    for case in 0..3 {
        scenario.store(case, Ordering::SeqCst);
        configure(&daemon, |config| {
            config.context.compaction_summary_model = "summary-large".into();
            // Leave accounting enough scheduling slack on busy CI machines;
            // the upstream delay still guarantees that the caller times out.
            config.context.compaction_summary_timeout_ms = if case == 0 { 1_000 } else { 3_000 };
            config.log.level = "debug".into();
        })
        .await;
        let response = client
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("x-api-key", daemon.api_key.as_deref().unwrap())
            .header("user-agent", "claude-cli/2.1.260 (external, test)")
            .json(
                &json!({"model":"resolved-tiny","max_tokens":64,"stream":false,
                "messages":[{"role":"user","content":"earlier context ".repeat(500)},
                    {"role":"assistant","content":"previous answer"},
                    {"role":"user","content":"continue"}]}),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 422);
        let trace = response.headers()["request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let response: Value = response.json().await.unwrap();
        assert_eq!(response["error"]["code"], "compaction_failed");
        assert!(response.get("content").is_none());
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let stats = expect_ok(
                daemon
                    .call("stats", json!({"detail":true,"recent":20}))
                    .await,
            );
            let summary = stats["stats"]["recent_requests"]
                .as_array()
                .unwrap()
                .iter()
                .find(|record| {
                    record["path"] == "/internal/compact" && record["trace_id"] == trace
                });
            let records = log_records(&daemon)
                .await
                .into_iter()
                .filter(|record| record["fields"]["trace_id"] == trace)
                .map(|record| json!({"level":record["level"], "fields":record["fields"]}))
                .collect::<Vec<_>>();
            let warnings = records
                .iter()
                .filter(|record| record["level"] == "WARN" && record["fields"]["trace_id"] == trace)
                .count();
            assert!(warnings <= 1, "duplicate summary warnings: {records:?}");
            let accounting_finished = records.iter().any(|record| {
                record["fields"]["trace_id"] == trace && record["fields"]["message"] == if case == 0 {
                    "timed-out compaction summary completed and was accounted"
                } else {
                    "Kiro semantic compaction summary failed after partial usage was accounted"
                }
            });
            if let Some(summary) = summary.filter(|_| warnings == 1 && accounting_finished) {
                assert_eq!(summary["diagnostics"]["upstream_status"], 200);
                if case == 0 {
                    assert_eq!(summary["status"], 200);
                    assert_eq!(summary["credits"], 0.25);
                    assert_eq!(summary["diagnostics"]["credits_source"], "server");
                } else {
                    assert_eq!(summary["status"], 502);
                    assert_eq!(summary["output_tokens"], 0);
                    assert_eq!(summary["credits"], 0.0);
                    assert_eq!(summary["diagnostics"]["credits_source"], "estimated");
                    assert_eq!(
                        summary["diagnostics"]["error_code"],
                        if case == 1 {
                            "compaction_stream_error"
                        } else {
                            "compaction_invalid_summary"
                        }
                    );
                }
                break;
            }
            assert!(
                Instant::now() < deadline,
                "summary accounting missing: {stats}; {records:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    daemon.stop().await;
}

/// Replays the three actual accumulation contracts: Claude Code retains starts
/// and ignores compaction deltas, TypeScript appends, Python replaces content.
fn accumulate(body: &str, client: &str) -> Vec<Value> {
    let mut blocks = Vec::new();
    for line in body.lines().filter_map(|line| line.strip_prefix("data: ")) {
        let event: Value = serde_json::from_str(line).unwrap();
        match event["type"].as_str() {
            Some("content_block_start") => {
                assert_eq!(event["index"].as_u64().unwrap() as usize, blocks.len());
                blocks.push(event["content_block"].clone());
            }
            Some("content_block_delta") => {
                let block = &mut blocks[event["index"].as_u64().unwrap() as usize];
                let delta = &event["delta"];
                match delta["type"].as_str() {
                    Some("compaction_delta") if client == "python" => {
                        block["content"] = delta["content"].clone()
                    }
                    Some("compaction_delta") if client == "typescript" => {
                        block["content"] = json!(format!(
                            "{}{}",
                            block["content"].as_str().unwrap_or(""),
                            delta["content"].as_str().unwrap()
                        ))
                    }
                    Some("text_delta") => {
                        block["text"] = json!(format!(
                            "{}{}",
                            block["text"].as_str().unwrap_or(""),
                            delta["text"].as_str().unwrap()
                        ))
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    blocks
}

#[tokio::test]
async fn compaction_checkpoints_round_trip_without_another_summary() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    let summaries = Arc::new(AtomicUsize::new(0));
    let invalid_summary = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let count = Arc::clone(&summaries);
    let invalid = Arc::clone(&invalid_summary);
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(move |request: &wiremock::Request| {
            let wire: Value = serde_json::from_slice(&request.body).unwrap();
            let summary = wire["conversationState"]["currentMessage"]["userInputMessage"]
                ["content"]
                .as_str()
                .unwrap()
                .contains("durable conversation checkpoint");
            let text = if summary {
                count.fetch_add(1, Ordering::SeqCst);
                if invalid.load(Ordering::SeqCst) {
                    "<summary>unterminated"
                } else {
                    "<summary>Remember checkpoint number 4729.</summary>"
                }
            } else {
                "answer"
            };
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(generation_body(text))
        })
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    configure(&daemon, |config| {
        config.server.enforce_user_agent_check = false;
        config.context.compaction_summary_model = "summary-large".into();
    })
    .await;
    let client = reqwest::Client::new();
    for fallback in [false, true] {
        invalid_summary.store(fallback, Ordering::SeqCst);
        for (kind, stream) in [
            ("claude", true),
            ("typescript", true),
            ("python", true),
            ("claude", false),
        ] {
            let mut before = summaries.load(Ordering::SeqCst);
            invalid_summary.store(fallback, Ordering::SeqCst);
            let mut request = json!({"model":"mapped-small","max_tokens":64,"stream":stream,
                "messages":[{"role":"user","content":"earlier context ".repeat(70000)},
                    {"role":"assistant","content":"remember 4729"},{"role":"user","content":"continue"}]});
            let agent = if kind == "claude" {
                "claude-cli/2.1.260 (external, test)"
            } else {
                "anthropic-sdk-test"
            };
            let response = client
                .post(format!("http://127.0.0.1:{port}/v1/messages"))
                .header("x-api-key", daemon.api_key.as_deref().unwrap())
                .header("user-agent", agent)
                .json(&request)
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                if fallback && !stream { 422 } else { 200 }
            );
            let body = response.text().await.unwrap();
            let body = if fallback {
                assert!(body.contains("compaction_failed"), "{body}");
                assert!(!body.contains("\"type\":\"compaction\""));
                assert!(summaries.load(Ordering::SeqCst) > before);
                before = summaries.load(Ordering::SeqCst);
                invalid_summary.store(false, Ordering::SeqCst);
                let recovered = client
                    .post(format!("http://127.0.0.1:{port}/v1/messages"))
                    .header("x-api-key", daemon.api_key.as_deref().unwrap())
                    .header("user-agent", agent)
                    .json(&request)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(recovered.status(), 200);
                recovered.text().await.unwrap()
            } else {
                body
            };
            let blocks = if stream {
                accumulate(&body, kind)
            } else {
                serde_json::from_str::<Value>(&body).unwrap()["content"]
                    .as_array()
                    .unwrap()
                    .clone()
            };
            assert_eq!(blocks[0]["type"], "compaction");
            let checkpoint = blocks[0]["content"]
                .as_str()
                .expect("a complete checkpoint, never null");
            assert!(!checkpoint.is_empty());
            if !fallback {
                assert_eq!(
                    checkpoint
                        .matches("Remember checkpoint number 4729.")
                        .count(),
                    1
                );
            }
            assert_eq!(summaries.load(Ordering::SeqCst), before + 1);
            request["messages"].as_array_mut().unwrap().extend([
                json!({"role":"assistant","content":blocks}),
                json!({"role":"user","content":"what next?"}),
            ]);
            let counted = client
                .post(format!("http://127.0.0.1:{port}/v1/messages/count_tokens"))
                .header("x-api-key", daemon.api_key.as_deref().unwrap())
                .header("user-agent", agent)
                .json(&request)
                .send()
                .await
                .unwrap();
            assert_eq!(counted.status(), 200);
            let counted: Value = counted.json().await.unwrap();
            assert!(counted["input_tokens"].as_u64().unwrap() < 121_600);
            let next = client
                .post(format!("http://127.0.0.1:{port}/v1/messages"))
                .header("x-api-key", daemon.api_key.as_deref().unwrap())
                .header("user-agent", agent)
                .json(&request)
                .send()
                .await
                .unwrap();
            assert_eq!(next.status(), 200);
            assert!(next.text().await.unwrap().contains("answer"));
            assert_eq!(
                summaries.load(Ordering::SeqCst),
                before + 1,
                "checkpoint lost by {kind}, fallback={fallback}"
            );
        }
    }
    let stats = expect_ok(
        daemon
            .call("stats", json!({"detail":true,"recent":100}))
            .await,
    );
    let summaries = stats["stats"]["recent_requests"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|log| log["path"] == "/internal/compact")
        .collect::<Vec<_>>();
    assert_eq!(summaries.len(), 12);
    assert_eq!(
        summaries
            .iter()
            .filter(|log| log["status"] == 502
                && log["diagnostics"]["error_code"] == "compaction_invalid_summary")
            .count(),
        4
    );
    assert!(summaries
        .iter()
        .all(|log| log["diagnostics"]["credits_source"] == "server"));
    daemon.stop().await;
}

#[tokio::test]
async fn omitted_client_checkpoint_is_replayed_without_another_summary() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    let summaries = Arc::new(AtomicUsize::new(0));
    let main_payloads = Arc::new(std::sync::Mutex::new(Vec::new()));
    let summary_count = Arc::clone(&summaries);
    let captured_payloads = Arc::clone(&main_payloads);
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(move |request: &wiremock::Request| {
            let wire: Value = serde_json::from_slice(&request.body).unwrap();
            let is_summary = wire["conversationState"]["currentMessage"]["userInputMessage"]
                ["content"]
                .as_str()
                .unwrap()
                .contains("durable conversation checkpoint");
            let text = if is_summary {
                summary_count.fetch_add(1, Ordering::SeqCst);
                "<summary>Remember server replay checkpoint 8142.</summary>"
            } else {
                captured_payloads
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(wire);
                "answer"
            };
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(generation_body(text))
        })
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    configure(&daemon, |config| {
        config.server.enforce_user_agent_check = false;
        config.context.compaction_summary_model = "summary-large".into();
    })
    .await;
    let client = reqwest::Client::new();
    let original_messages = json!([
        {"role":"user","content":"earlier context ".repeat(70000)},
        {"role":"assistant","content":"remember the old conclusion"},
        {"role":"user","content":"continue"}
    ]);
    let first = client
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("x-api-key", daemon.api_key.as_deref().unwrap())
        .header("user-agent", "custom-client/1.0")
        .header("x-client-request-id", "request-one")
        .json(&json!({
            "model":"mapped-small",
            "max_tokens":64,
            "stream":false,
            "messages":original_messages.clone()
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    let first: Value = first.json().await.unwrap();
    assert_eq!(first["content"][0]["type"], "compaction");
    assert_eq!(summaries.load(Ordering::SeqCst), 1);

    // Simulate a client that kept the assistant text but silently discarded
    // the returned compaction block and therefore resent the entire old prefix.
    let mut extended_messages = original_messages.as_array().unwrap().clone();
    extended_messages.extend([
        json!({"role":"assistant","content":[{"type":"text","text":"answer"}]}),
        json!({"role":"user","content":"what next?"}),
    ]);
    let counted = client
        .post(format!("http://127.0.0.1:{port}/v1/messages/count_tokens"))
        .header("x-api-key", daemon.api_key.as_deref().unwrap())
        .header("user-agent", "custom-client/1.0")
        .header("x-client-request-id", "request-count")
        .json(&json!({
            "model":"mapped-small",
            "messages":extended_messages.clone()
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(counted.status(), 200);
    assert!(
        counted.json::<Value>().await.unwrap()["input_tokens"]
            .as_u64()
            .unwrap()
            < 121_600
    );
    let second = client
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("x-api-key", daemon.api_key.as_deref().unwrap())
        .header("user-agent", "custom-client/1.0")
        .header("x-client-request-id", "request-two")
        .json(&json!({
            "model":"mapped-small",
            "max_tokens":64,
            "stream":false,
            "messages":extended_messages
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 200);
    assert_eq!(
        second.json::<Value>().await.unwrap()["content"][0]["text"],
        "answer"
    );
    assert_eq!(
        summaries.load(Ordering::SeqCst),
        1,
        "the same oversized source prefix must not be summarized again"
    );
    let replayed_wire = {
        let payloads = main_payloads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(payloads.len(), 2);
        payloads[1].to_string()
    };
    assert!(replayed_wire.contains("server replay checkpoint 8142"));
    assert!(replayed_wire.contains("answer"));
    assert!(replayed_wire.contains("what next?"));
    assert!(
        replayed_wire.len() < 100_000,
        "verified replay must keep the old multi-megabyte prefix off the upstream wire"
    );
    daemon.stop().await;
}

#[tokio::test]
#[ignore = "requires a locally installed Claude Code supporting --bare; all HTTP calls use the local mock"]
async fn real_claude_code_retains_the_compaction_checkpoint() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("GET")).and(path("/ListAvailableModels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":[
            {"modelId":"claude-haiku-4.5","tokenLimits":{"maxInputTokens":1500,"maxOutputTokens":32000}},
            {"modelId":"summary-large","tokenLimits":{"maxInputTokens":10000,"maxOutputTokens":8192}}
        ]}))).mount(&mock).await;
    let summaries = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&summaries);
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(move |request: &wiremock::Request| {
            let wire: Value = serde_json::from_slice(&request.body).unwrap();
            let is_summary = wire["conversationState"]["currentMessage"]["userInputMessage"]
                ["content"]
                .as_str()
                .unwrap()
                .contains("durable conversation checkpoint");
            let text = if is_summary {
                count.fetch_add(1, Ordering::SeqCst);
                "<summary>Remember checkpoint number 4729.</summary>"
            } else {
                "Ready."
            };
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(generation_body(text))
        })
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    expect_ok(daemon.call("account.import", json!({"accounts":[{
        "id":"acc_12345678","email":"local-client-test@example.com","machine_id":"1".repeat(64),
        "credentials":{"access_token":"mock-token","region":"us-east-1","expires_at":4_000_000_000i64,"auth_method":"idc"},
        "usage":{"current":0.0,"limit":100.0,"percent_used":0.0,"updated_at":1},"created_at":1
    }]})).await);
    expect_ok(daemon.call("models", json!({})).await);
    configure(&daemon, |config| {
        config.context.compaction_summary_model = "summary-large".into()
    })
    .await;

    let directory = tempfile::tempdir().unwrap();
    let mut child = Command::new("claude")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("CLAUDE_CONFIG_DIR", directory.path())
        .env("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{port}"))
        .env("ANTHROPIC_API_KEY", daemon.api_key.as_deref().unwrap())
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("DISABLE_TELEMETRY", "1")
        .current_dir(directory.path())
        .args([
            "--bare",
            "-p",
            "--model",
            "claude-haiku-4-5-20251001",
            "--tools",
            "",
            "--system-prompt",
            "Reply briefly.",
            "--setting-sources",
            "",
            "--strict-mcp-config",
            "--mcp-config",
            "{\"mcpServers\":{}}",
            "--no-session-persistence",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut checkpoint_seen = false;
    for prompt in [
        "earlier facts ".repeat(300),
        "later facts ".repeat(500),
        "What next?".into(),
    ] {
        input
            .write_all(
                format!(
                    "{}\n",
                    json!({"type":"user","message":{"role":"user","content":prompt}})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        input.flush().await.unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let line = lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("Claude Code output");
                let event: Value = serde_json::from_str(&line).unwrap();
                if event["type"] == "assistant" {
                    for block in event["message"]["content"].as_array().unwrap() {
                        if block["type"] == "compaction" {
                            assert!(
                                block["content"]
                                    .as_str()
                                    .is_some_and(|value| value.contains("4729")),
                                "checkpoint lost: {block}"
                            );
                            checkpoint_seen = true;
                        }
                    }
                }
                if event["type"] == "result" {
                    assert_eq!(event["is_error"], false, "{event}");
                    break;
                }
            }
        })
        .await
        .expect("local Claude Code round completed");
    }
    drop(input);
    tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(
        checkpoint_seen,
        "test conversation must trigger actual proxy compaction"
    );
    assert_eq!(
        summaries.load(Ordering::SeqCst),
        1,
        "third turn must reuse the checkpoint"
    );
    daemon.stop().await;
}
