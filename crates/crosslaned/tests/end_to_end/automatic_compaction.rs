use super::*;
use serde_json::{json, Value};

#[tokio::test]
async fn upstream_context_error_variants_compact_without_a_client_trigger() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    for rejection in [
        "CONTEXT_LENGTH_EXCEEDED",
        "INPUT_TOO_LONG",
        "The context window has been exceeded",
    ] {
        for stream in [false, true] {
            let mock = MockServer::start().await;
            mount_context_alignment_models(&mock).await;
            let responder = UpstreamOverflowCompactionResponder {
                rejection_message: Some(rejection.into()),
                ..Default::default()
            };
            Mock::given(method("POST"))
                .and(path("/generateAssistantResponse"))
                .respond_with(responder.clone())
                .mount(&mock)
                .await;
            let port = unused_tcp_port();
            let daemon =
                Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri()))
                    .await;
            import_context_alignment_account(&daemon, 0.0).await;

            // No context_management edit or /compact prompt: the local estimate
            // fits the advertised window, but the upstream rejects the input.
            let response = reqwest::Client::new()
                .post(format!("http://127.0.0.1:{port}/v1/messages"))
                .header("x-api-key", daemon.api_key.as_deref().unwrap())
                .header("user-agent", "claude-cli/2.1.260 (external, e2e)")
                .json(&json!({
                    "model":"source-large", "max_tokens":64, "stream":stream,
                    "system":"Keep this governing instruction.",
                    "messages":[
                        {"role":"user","content":"earlier context ".repeat(2_000)},
                        {"role":"assistant","content":"Keep the earlier decision"},
                        {"role":"user","content":"Continue after the upstream rejection"}
                    ]
                }))
                .send()
                .await
                .unwrap();
            let status = response.status();
            let body = response.text().await.unwrap();
            assert_eq!(
                status,
                reqwest::StatusCode::OK,
                "{rejection}, stream={stream}: {body}"
            );
            if stream {
                assert!(body.contains("\"type\":\"compaction\""), "{body}");
                assert!(
                    body.contains("main request completed after overflow retry"),
                    "{body}"
                );
                assert!(body.contains("\"type\":\"message_stop\""), "{body}");
            } else {
                let body: Value = serde_json::from_str(&body).unwrap();
                assert_eq!(body["content"][0]["type"], "compaction");
                assert_eq!(
                    body["content"][1]["text"],
                    "main request completed after overflow retry"
                );
            }
            assert_eq!(
                responder.main_calls.load(Ordering::SeqCst),
                2,
                "one recovery retry"
            );
            let payloads: Vec<Value> = mock
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|request| request.url.path() == "/generateAssistantResponse")
                .map(|request| serde_json::from_slice(&request.body).unwrap())
                .collect();
            assert_eq!(payloads.len(), 3, "initial generation, summary, retry");
            let retried = payloads.last().unwrap();
            assert_eq!(
                retried["conversationState"]["currentMessage"]["userInputMessage"]["content"],
                "Continue after the upstream rejection"
            );
            assert!(retried
                .to_string()
                .contains("Keep this governing instruction."));
            assert!(retried.to_string().len() < payloads[0].to_string().len());
            daemon.stop().await;
        }
    }
}

#[tokio::test]
async fn context_recovery_respects_disable_and_stops_after_one_retry() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    for automatic in [false, true] {
        let mock = MockServer::start().await;
        mount_context_alignment_models(&mock).await;
        let main_calls = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&main_calls);
        Mock::given(method("POST"))
            .and(path("/generateAssistantResponse"))
            .respond_with(move |request: &wiremock::Request| {
                let payload: Value = serde_json::from_slice(&request.body).unwrap();
                let current = payload["conversationState"]["currentMessage"]["userInputMessage"]
                    ["content"]
                    .as_str()
                    .unwrap();
                if current.contains("durable conversation checkpoint") {
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "application/vnd.amazon.eventstream")
                        .set_body_bytes(generation_body(
                            "<summary>Preserve the earlier decision.</summary>",
                        ))
                } else {
                    calls.fetch_add(1, Ordering::SeqCst);
                    ResponseTemplate::new(400)
                        .set_body_json(json!({"reason":"CONTEXT_LENGTH_EXCEEDED"}))
                }
            })
            .mount(&mock)
            .await;
        let port = unused_tcp_port();
        let daemon =
            Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
        import_context_alignment_account(&daemon, 0.0).await;
        if !automatic {
            let config_path = daemon.home().join("config.toml");
            let mut config: Config =
                toml::from_str(&tokio::fs::read_to_string(&config_path).await.unwrap()).unwrap();
            config.context.auto_compact_on_overflow = false;
            tokio::fs::write(&config_path, toml::to_string(&config).unwrap())
                .await
                .unwrap();
            expect_ok(daemon.call("config.reload", json!({})).await);
        }
        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("x-api-key", daemon.api_key.as_deref().unwrap())
            .header("user-agent", "claude-cli/2.1.260 (external, e2e)")
            .json(&json!({
                "model":"source-large", "max_tokens":64,
                "messages":[
                    {"role":"user","content":"earlier context ".repeat(2_000)},
                    {"role":"assistant","content":"Keep the earlier decision"},
                    {"role":"user","content":"Continue"}
                ]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
        assert_eq!(
            response.headers()["x-crosslane-error-code"],
            "context_length_exceeded"
        );
        assert_eq!(response.headers()["x-crosslane-account-error"], "false");
        let body: Value = response.json().await.unwrap();
        assert_eq!(
            body["error"]["message"],
            "prompt is too long: context length exceeded"
        );
        assert_eq!(
            main_calls.load(Ordering::SeqCst),
            if automatic { 2 } else { 1 }
        );
        daemon.stop().await;
    }
}
