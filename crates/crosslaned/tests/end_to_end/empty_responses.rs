use super::*;
use serde_json::json;

#[tokio::test]
async fn ordinary_reasoning_only_replies_are_not_retried_in_each_protocol() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    for protocol in ["messages", "chat/completions", "responses"] {
        for stream in [false, true] {
            let mock = MockServer::start().await;
            mount_context_alignment_models(&mock).await;
            Mock::given(method("POST"))
                .and(path("/generateAssistantResponse"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "application/vnd.amazon.eventstream")
                        .set_body_bytes(event_stream_frame(
                            "reasoningContentEvent",
                            json!({"reasoningContentEvent":{"text":"VALID_REASONING_ONLY", "signature":"reasoning-signature"}}),
                        )),
                )
                .expect(1)
                .mount(&mock)
                .await;
            let port = unused_tcp_port();
            let daemon =
                Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri()))
                    .await;
            import_context_alignment_account(&daemon, 0.0).await;
            let request = match protocol {
                "messages" => json!({"model":"source-large", "max_tokens":4096, "stream":stream,
                    "thinking":{"type":"adaptive"}, "messages":[{"role":"user", "content":"Think carefully"}]}),
                "responses" => {
                    json!({"model":"source-large", "max_output_tokens":4096, "stream":stream,
                    "reasoning":{"effort":"high"}, "input":"Think carefully"})
                }
                _ => json!({"model":"source-large", "max_tokens":4096, "stream":stream,
                    "reasoning_effort":"high", "messages":[{"role":"user", "content":"Think carefully"}]}),
            };
            let response = reqwest::Client::new()
                .post(format!("http://127.0.0.1:{port}/v1/{protocol}"))
                .header("x-api-key", daemon.api_key.as_deref().unwrap())
                .header(
                    "user-agent",
                    if protocol == "messages" {
                        "claude-cli/2.1.296 (external, sdk-cli)"
                    } else {
                        "codex_cli_rs/0.147.0 (Mac OS 26.0; arm64) Terminal/1.0"
                    },
                )
                .json(&request)
                .send()
                .await
                .unwrap();
            let status = response.status();
            let body = response.text().await.unwrap();
            assert_eq!(status, 200, "{protocol}, stream={stream}: {body}");
            assert!(
                body.contains("VALID_REASONING_ONLY"),
                "{protocol}, stream={stream}: {body}"
            );
            assert!(
                !body.contains("event: error\n") && !body.contains("response.failed"),
                "{body}"
            );
            daemon.stop().await;
        }
    }
}

#[tokio::test]
async fn ordinary_empty_replies_retry_once_then_complete_or_fail_in_each_protocol() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    for protocol in ["messages", "chat/completions", "responses"] {
        for stream in [false, true] {
            for recover in [true, false] {
                let mock = MockServer::start().await;
                mount_context_alignment_models(&mock).await;
                let calls = Arc::new(AtomicUsize::new(0));
                let observed = Arc::clone(&calls);
                Mock::given(method("POST")).and(path("/generateAssistantResponse"))
                    .respond_with(move |_: &wiremock::Request| {
                        let attempt = observed.fetch_add(1, Ordering::SeqCst);
                        let body = if recover && attempt == 1 {
                            generation_body("EMPTY_REPLY_RECOVERED")
                        } else {
                            event_stream_frame("messageMetadataEvent", json!({"usage":{"inputTokens":100,"outputTokens":0,"creditsConsumed":0.01}}))
                        };
                        ResponseTemplate::new(200).insert_header("content-type", "application/vnd.amazon.eventstream").set_body_bytes(body)
                    }).mount(&mock).await;
                let port = unused_tcp_port();
                let daemon =
                    Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri()))
                        .await;
                import_context_alignment_account(&daemon, 0.0).await;
                model_policy::configure(&daemon, |config| {
                    config.upstream.pool.stream_max_connections = 1;
                    config.upstream.stream_slot_wait_timeout_ms = 100;
                })
                .await;
                let mut request = json!({"model":"source-large","max_tokens":64,"stream":stream,
                    "messages":[{"role":"user","content":"Answer this ordinary request"}]});
                if protocol == "responses" {
                    request = json!({"model":"source-large","max_output_tokens":64,"stream":stream,"input":"Answer this ordinary request"});
                }
                let response = reqwest::Client::new()
                    .post(format!("http://127.0.0.1:{port}/v1/{protocol}"))
                    .header("x-api-key", daemon.api_key.as_deref().unwrap())
                    .header(
                        "user-agent",
                        if protocol == "messages" {
                            "claude-cli/2.1.296 (external, sdk-cli)"
                        } else {
                            "codex_cli_rs/0.147.0 (Mac OS 26.0; arm64) Terminal/1.0"
                        },
                    )
                    .json(&request)
                    .send()
                    .await
                    .unwrap();
                let status = response.status();
                let body = response.text().await.unwrap();
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    2,
                    "{protocol}, stream={stream}: {body}"
                );
                if recover {
                    assert_eq!(status, reqwest::StatusCode::OK, "{body}");
                    assert!(body.contains("EMPTY_REPLY_RECOVERED"), "{body}");
                } else {
                    assert_eq!(
                        status,
                        if stream {
                            reqwest::StatusCode::OK
                        } else {
                            reqwest::StatusCode::BAD_GATEWAY
                        },
                        "{body}"
                    );
                    assert!(
                        body.contains("no assistant output")
                            || body.contains("upstream_unavailable"),
                        "{body}"
                    );
                    assert!(!body.contains("response.completed"), "{body}");
                    assert!(!body.contains("\"type\":\"message_stop\""), "{body}");
                }
                daemon.stop().await;
            }
        }
    }
}
