use super::*;
use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;

struct Mixed {
    daemon: Daemon,
    port: u16,
    _kiro: MockServer,
    _copilot: MockServer,
    kiro_status: Arc<AtomicUsize>,
    kiro_stream_error: Arc<AtomicBool>,
    copilot_status: Arc<AtomicUsize>,
    protocol_error: Arc<AtomicBool>,
    kiro_calls: Arc<AtomicUsize>,
    copilot_calls: Arc<AtomicUsize>,
}

impl Mixed {
    async fn start() -> Self {
        let kiro = MockServer::start().await;
        let copilot = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/ListAvailableModels"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":[{
                "modelId":"claude-sonnet-4.6", "modelName":"Claude Sonnet 4.6",
                "tokenLimits":{"maxInputTokens":200_000,"maxOutputTokens":16_000}
            }, {"modelId":"claude-sonnet-4.5", "modelName":"Claude Sonnet 4.5",
                "tokenLimits":{"maxInputTokens":200_000,"maxOutputTokens":16_000}}]})))
            .mount(&kiro)
            .await;
        for (endpoint, body) in [
            ("/user", json!({"id":7,"login":"balanced-test"})),
            (
                "/copilot_internal/v2/token",
                json!({"token":"fake-api-token","expires_at":4_000_000_000i64,"endpoints":{"api":copilot.uri()}}),
            ),
            (
                "/models",
                json!({"data":[{"id":"claude-sonnet-4-6","capabilities":{"supported_endpoints":["/v1/messages","/chat/completions","/responses"]}}]}),
            ),
        ] {
            Mock::given(method("GET"))
                .and(path(endpoint))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&copilot)
                .await;
        }
        let kiro_status = Arc::new(AtomicUsize::new(200));
        let kiro_stream_error = Arc::new(AtomicBool::new(false));
        let copilot_status = Arc::new(AtomicUsize::new(200));
        let protocol_error = Arc::new(AtomicBool::new(false));
        let kiro_calls = Arc::new(AtomicUsize::new(0));
        let copilot_calls = Arc::new(AtomicUsize::new(0));
        let status = Arc::clone(&kiro_status);
        let stream_error = Arc::clone(&kiro_stream_error);
        let calls = Arc::clone(&kiro_calls);
        Mock::given(method("POST"))
            .and(path("/generateAssistantResponse"))
            .respond_with(move |request: &wiremock::Request| {
                calls.fetch_add(1, Ordering::SeqCst);
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                let model = body["conversationState"]["currentMessage"]["userInputMessage"]
                    ["modelId"]
                    .as_str()
                    .unwrap();
                assert!(matches!(model, "claude-sonnet-4.6" | "claude-sonnet-4.5"));
                let status = status.load(Ordering::SeqCst) as u16;
                if status != 200 && model == "claude-sonnet-4.6" {
                    return ResponseTemplate::new(status)
                        .set_body_json(json!({"message":"test Kiro unavailable"}));
                }
                if stream_error.load(Ordering::SeqCst) && model == "claude-sonnet-4.6" {
                    return ResponseTemplate::new(200)
                        .insert_header("content-type", "application/vnd.amazon.eventstream")
                        .set_body_bytes(event_stream_frame("error", json!({
                            "__type":"MODEL_TEMPORARILY_UNAVAILABLE",
                            "message":"Encountered unexpectedly high load when processing the request, please try again."
                        })));
                }
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/vnd.amazon.eventstream")
                    .set_body_bytes(generation_body("from kiro"))
            })
            .mount(&kiro)
            .await;
        for endpoint in [
            "/v1/messages",
            "/chat/completions",
            "/responses",
            "/v1/messages/count_tokens",
        ] {
            let status = Arc::clone(&copilot_status);
            let calls = Arc::clone(&copilot_calls);
            let protocol_error = Arc::clone(&protocol_error);
            Mock::given(method("POST")).and(path(endpoint))
                .respond_with(move |request: &wiremock::Request| {
                    let sequence = calls.fetch_add(1, Ordering::SeqCst);
                    let body: Value = serde_json::from_slice(&request.body).unwrap();
                    assert_eq!(body["model"], "claude-sonnet-4-6");
                    let status = status.load(Ordering::SeqCst) as u16;
                    if status != 200 {
                        return ResponseTemplate::new(status).set_body_json(json!({"error":{"type":"invalid_request_error","message":if status == 400 {"max_tokens must be greater than 0"} else {"test Copilot unavailable"}}}));
                    }
                    if body["stream"] == true {
                        let data = if protocol_error.load(Ordering::SeqCst) {
                            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial from copilot\"}}\n\nevent: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"unavailable\"}}\n\n".to_owned()
                        } else if endpoint == "/v1/messages" {
                            format!("event: message_start\ndata: {}\n\nevent: content_block_delta\ndata: {}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n", json!({"type":"message_start","message":{"id":"msg-balanced","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"usage":{"input_tokens":8,"output_tokens":0}}}), json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"from copilot"}}))
                        } else if endpoint == "/chat/completions" {
                            format!("data: {}\n\ndata: [DONE]\n\n", json!({"id":"chat-balanced","object":"chat.completion.chunk","model":"claude-sonnet-4-6","choices":[{"index":0,"delta":{"content":"from copilot"},"finish_reason":"stop"}]}))
                        } else {
                            format!("event: response.completed\ndata: {}\n\n", json!({"type":"response.completed","response":{"id":format!("resp-copilot-{sequence}"),"object":"response","status":"completed","model":"claude-sonnet-4-6","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"from copilot"}]}]}}))
                        };
                        return ResponseTemplate::new(200).insert_header("content-type", "text/event-stream").set_body_string(data);
                    }
                    if protocol_error.load(Ordering::SeqCst) {
                        return ResponseTemplate::new(200).set_body_json(json!({"error":{"message":"test upstream protocol failure"}}));
                    }
                    let response = match endpoint {
                        "/v1/messages/count_tokens" => json!({"input_tokens":8}),
                        "/v1/messages" => json!({"id":"msg-balanced","type":"message","model":"claude-sonnet-4-6","role":"assistant","content":[{"type":"text","text":"from copilot"}],"stop_reason":"end_turn","usage":{"input_tokens":8,"output_tokens":2}}),
                        "/chat/completions" => json!({"id":"chat-balanced","object":"chat.completion","model":"claude-sonnet-4-6","choices":[{"index":0,"message":{"role":"assistant","content":"from copilot"},"finish_reason":"stop"}],"usage":{"prompt_tokens":8,"completion_tokens":2}}),
                        "/responses" => json!({"id":format!("resp-copilot-{sequence}"),"object":"response","model":"claude-sonnet-4-6","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"from copilot"}]}],"usage":{"input_tokens":8,"output_tokens":2}}),
                        _ => unreachable!(),
                    };
                    ResponseTemplate::new(200).set_body_json(response)
                }).mount(&copilot).await;
        }
        let port = unused_tcp_port();
        let daemon =
            Daemon::start_http(port, &format!("{}/generateAssistantResponse", kiro.uri())).await;
        model_policy::configure(&daemon, |config| {
            let mut provider = ProviderConfig {
                id: "copilot".into(),
                kind: "copilot".into(),
                ..ProviderConfig::default()
            };
            provider
                .settings
                .insert("github_api_base".into(), json!(copilot.uri()));
            provider
                .settings
                .insert("oauth_base".into(), json!(copilot.uri()));
            provider
                .settings
                .insert("allow_insecure_http".into(), json!(true));
            config.provider = vec![ProviderConfig::default(), provider];
            config.proxy_service[0].allowed_providers = vec!["kiro".into(), "copilot".into()];
            config.proxy_service[0].default_provider = "kiro".into();
            config.proxy_service[0].skip_user_agent_check = true;
            config.api_key[0].allowed_providers = vec!["kiro".into(), "copilot".into()];
            config.api_key[0].skip_user_agent_check = true;
            config.upstream.max_retries = 0;
        })
        .await;
        expect_ok(daemon.call("account.import", json!({"accounts":[{
            "id":"acc_aaaaaaaa", "email":"balanced@example.com", "machine_id":"a".repeat(64),
            "credentials":{"access_token":"wiremock-token","region":"us-east-1","expires_at":4_000_000_000i64,"auth_method":"idc"},
            "usage":{"current":0.0,"limit":100_000.0,"percent_used":0.0,"updated_at":1},
            "subscription":{"kind":"pro"},"created_at":1
        }]})).await);
        expect_ok(
            daemon
                .call(
                    "v2.account.importToken",
                    json!({"provider":"copilot","token":"fake-github-token"}),
                )
                .await,
        );
        let models = expect_ok(daemon.call("models", json!({"refresh":true})).await);
        assert!(models.to_string().contains("claude-sonnet-4.6"), "{models}");
        Self {
            daemon,
            port,
            _kiro: kiro,
            _copilot: copilot,
            kiro_status,
            kiro_stream_error,
            copilot_status,
            protocol_error,
            kiro_calls,
            copilot_calls,
        }
    }

    async fn request(
        &self,
        endpoint: &str,
        model: &str,
        stream: bool,
        previous: Option<&str>,
    ) -> (u16, String) {
        let mut body = if endpoint == "/v1/responses" {
            json!({"model":model,"input":"hello","stream":stream,"store":true})
        } else {
            json!({"model":model,"messages":[{"role":"user","content":"hello"}],"max_tokens":16,"stream":stream})
        };
        if let Some(previous) = previous {
            body["previous_response_id"] = json!(previous);
        }
        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{}{endpoint}", self.port))
            .bearer_auth(self.daemon.api_key.as_deref().unwrap())
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.text().await.unwrap())
    }

    fn counts(&self) -> (usize, usize) {
        (
            self.kiro_calls.load(Ordering::SeqCst),
            self.copilot_calls.load(Ordering::SeqCst),
        )
    }
}

#[tokio::test]
async fn matching_model_defaults_balance_and_different_overrides_keep_their_semantics() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mixed = Mixed::start().await;
    model_policy::configure(&mixed.daemon, |config| {
        config.features.default_model_id = "claude-sonnet-4.6".into();
        config.provider[1].routing.default_model_id = "claude-sonnet-4-6".into();
    })
    .await;
    for _ in 0..2 {
        let (status, body) = mixed
            .request("/v1/chat/completions", "claude-sonnet-4-6", false, None)
            .await;
        assert_eq!(status, 200, "{body}");
    }
    assert_eq!(mixed.counts(), (1, 1));
    model_policy::configure(&mixed.daemon, |config| {
        config.features.default_model_id = "claude-sonnet-4.5".into();
        config.provider[1].routing.default_model_id = "claude-sonnet-4-5".into();
    })
    .await;
    let (status, body) = mixed
        .request("/v1/chat/completions", "claude-sonnet-4-6", false, None)
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("from kiro"), "{body}");
    let requests = mixed._kiro.received_requests().await.unwrap();
    let request = requests
        .iter()
        .rev()
        .find(|request| request.url.path() == "/generateAssistantResponse")
        .unwrap();
    let value: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(
        value["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
        "claude-sonnet-4.6"
    );
    model_policy::configure(&mixed.daemon, |config| {
        config.api_key[0].allowed_providers = vec!["copilot".into()];
        config.provider[1].routing.default_model_id = "claude-sonnet-4-6".into();
    })
    .await;
    let (status, body) = mixed
        .request("/v1/chat/completions", "claude-sonnet-4-6", false, None)
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("from copilot"), "{body}");
    assert_eq!(mixed.counts(), (2, 2));
    mixed.daemon.stop().await;
}

#[tokio::test]
async fn kiro_stream_failures_do_not_downgrade_the_model_and_cool_the_route() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mixed = Mixed::start().await;
    model_policy::configure(&mixed.daemon, |config| {
        config.features.enable_model_fallback = true;
        config.upstream.max_retries = 1;
    })
    .await;
    mixed.kiro_stream_error.store(true, Ordering::SeqCst);
    let (status, body) = mixed
        .request("/v1/messages", "claude-sonnet-4-6", true, None)
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("error"), "{body}");
    assert!(!body.contains("from kiro"), "{body}");
    let (status, body) = mixed
        .request("/v1/messages", "claude-sonnet-4-6", false, None)
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("from copilot"), "{body}");
    let requests = mixed._kiro.received_requests().await.unwrap();
    for request in requests
        .iter()
        .filter(|request| request.url.path() == "/generateAssistantResponse")
    {
        let value: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(
            value["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
            "claude-sonnet-4.6"
        );
    }
    mixed.daemon.stop().await;
}

#[tokio::test]
async fn kiro_batched_chat_balances_candidates_without_holding_a_parent_slot() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mixed = Mixed::start().await;
    model_policy::configure(&mixed.daemon, |config| {
        config.server.max_concurrent_requests = 1;
        config.server.max_connections = 1;
        config.pool.max_concurrent_per_account = 1;
    })
    .await;
    for stream in [false, true] {
        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{}/v1/chat/completions", mixed.port))
            .bearer_auth(mixed.daemon.api_key.as_deref().unwrap())
            .json(&json!({"model":"claude-sonnet-4-6","messages":[{"role":"user","content":"hello"}],"n":3,"stream":stream}))
            .send().await.unwrap();
        assert_eq!(response.status(), 200);
        let body = tokio::time::timeout(Duration::from_secs(10), response.text())
            .await
            .unwrap()
            .unwrap();
        assert!(!body.contains("\"error\""), "{body}");
        assert!(
            body.contains("from kiro") && body.contains("from copilot"),
            "{body}"
        );
    }
    assert_eq!(mixed.counts(), (3, 3));
    mixed.daemon.stop().await;
}

#[tokio::test]
async fn equivalent_models_balance_across_all_generation_protocols_and_streaming() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mixed = Mixed::start().await;
    for endpoint in ["/v1/messages", "/v1/chat/completions", "/v1/responses"] {
        for stream in [false, true] {
            for _ in 0..2 {
                let (status, body) = mixed
                    .request(endpoint, "claude-sonnet-4-6", stream, None)
                    .await;
                assert_eq!(status, 200, "{body}");
                assert!(
                    body.contains("from kiro") || body.contains("from copilot"),
                    "{body}"
                );
            }
        }
    }
    assert_eq!(mixed.counts(), (6, 6));
    for _ in 0..2 {
        let (status, body) = mixed
            .request(
                "/v1/messages/count_tokens",
                "claude-sonnet-4-6",
                false,
                None,
            )
            .await;
        assert_eq!(status, 200, "{body}");
        assert!(serde_json::from_str::<Value>(&body).unwrap()["input_tokens"].is_number());
    }
    mixed.daemon.stop().await;
}

#[tokio::test]
async fn kiro_failure_uses_copilot_and_recovers_after_cooldown() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mixed = Mixed::start().await;
    mixed.kiro_status.store(503, Ordering::SeqCst);
    for _ in 0..2 {
        let (status, body) = mixed
            .request("/v1/messages", "claude-sonnet-4-6", false, None)
            .await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("from copilot"));
    }
    assert_eq!(mixed.counts(), (1, 2));
    mixed.kiro_status.store(200, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(5_100)).await;
    for _ in 0..2 {
        assert_eq!(
            mixed
                .request("/v1/messages", "claude-sonnet-4-6", false, None)
                .await
                .0,
            200
        );
    }
    assert_eq!(mixed.counts().0, 2);
    mixed.daemon.stop().await;
}

#[tokio::test]
async fn copilot_throttle_uses_kiro_without_repeating_failed_client_requests() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mixed = Mixed::start().await;
    assert_eq!(
        mixed
            .request("/v1/chat/completions", "claude-sonnet-4-6", false, None)
            .await
            .0,
        200
    );
    mixed.copilot_status.store(429, Ordering::SeqCst);
    for _ in 0..2 {
        let (status, body) = mixed
            .request("/v1/chat/completions", "claude-sonnet-4-6", false, None)
            .await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("from kiro"));
    }
    assert_eq!(mixed.counts(), (3, 1));
    let stats = expect_ok(
        mixed
            .daemon
            .call("stats", json!({"detail":true,"recent":10}))
            .await,
    );
    assert_eq!(stats["stats"]["total"]["requests"], 3, "{stats}");
    assert_eq!(stats["stats"]["total"]["successes"], 3, "{stats}");
    mixed.daemon.stop().await;
}

#[tokio::test]
async fn both_provider_failures_are_bounded_and_cooling_requests_do_not_hit_upstreams() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mixed = Mixed::start().await;
    mixed.kiro_status.store(503, Ordering::SeqCst);
    mixed.copilot_status.store(503, Ordering::SeqCst);
    let (status, body) = mixed
        .request("/v1/chat/completions", "claude-sonnet-4-6", false, None)
        .await;
    assert!(status >= 500, "{body}");
    assert_eq!(mixed.counts(), (1, 1));
    let (status, body) = mixed
        .request("/v1/chat/completions", "claude-sonnet-4-6", false, None)
        .await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(mixed.counts(), (1, 1));
    mixed.daemon.stop().await;
}

#[tokio::test]
async fn a_nonstream_protocol_error_with_http_200_fails_over_before_delivery() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mixed = Mixed::start().await;
    assert_eq!(
        mixed
            .request("/v1/chat/completions", "claude-sonnet-4-6", false, None)
            .await
            .0,
        200
    );
    mixed.protocol_error.store(true, Ordering::SeqCst);
    let (status, body) = mixed
        .request("/v1/chat/completions", "claude-sonnet-4-6", false, None)
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("from kiro"));
    assert!(!body.contains("test upstream protocol failure"));
    assert_eq!(mixed.counts(), (2, 1));
    mixed.daemon.stop().await;
}

#[tokio::test]
async fn a_model_available_only_on_kiro_is_never_sent_to_copilot() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mixed = Mixed::start().await;
    let (status, body) = mixed
        .request("/v1/chat/completions", "claude-sonnet-4-5", false, None)
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("from kiro"));
    let (status, body) = mixed
        .request(
            "/v1/chat/completions",
            "copilot/claude-sonnet-4-5",
            false,
            None,
        )
        .await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("model_not_available"));
    assert_eq!(mixed.counts(), (1, 0));
    mixed.daemon.stop().await;
}

#[tokio::test]
async fn explicit_routes_mappings_and_client_errors_do_not_fail_over() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mixed = Mixed::start().await;
    mixed.copilot_status.store(503, Ordering::SeqCst);
    assert_eq!(
        mixed
            .request(
                "/v1/chat/completions",
                "copilot/claude-sonnet-4-6",
                false,
                None
            )
            .await
            .0,
        503
    );
    assert_eq!(mixed.counts(), (0, 1));
    mixed.copilot_status.store(400, Ordering::SeqCst);
    assert_eq!(
        mixed
            .request("/v1/chat/completions", "claude-sonnet-4-6", false, None)
            .await
            .0,
        200
    );
    assert_eq!(
        mixed
            .request("/v1/chat/completions", "claude-sonnet-4-6", false, None)
            .await
            .0,
        400
    );
    assert_eq!(mixed.counts(), (1, 2));
    mixed.copilot_status.store(200, Ordering::SeqCst);
    model_policy::configure(&mixed.daemon, |config| {
        config.provider[0].routing.allow_cross_provider_fallback = true;
        config.model_mapping.push(ModelMappingRule {
            name: "pinned-mapping".into(),
            source_models: vec!["custom".into()],
            target_models: vec!["copilot/claude-sonnet-4-6".into()],
            providers: vec!["kiro".into()],
            ..ModelMappingRule::default()
        });
    })
    .await;
    for _ in 0..2 {
        let (status, body) = mixed
            .request("/v1/chat/completions", "custom", false, None)
            .await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("from copilot"));
    }
    assert_eq!(mixed.counts(), (1, 4));
    mixed.daemon.stop().await;
}

#[tokio::test]
async fn routing_respects_key_scope_exclusions_disabled_providers_and_model_denies() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mixed = Mixed::start().await;
    model_policy::configure(&mixed.daemon, |config| {
        config.api_key[0].allowed_providers = vec!["copilot".into()];
        config.api_key[0].allowed_models = vec!["copilot/claude-sonnet-4-6".into()];
    })
    .await;
    for _ in 0..2 {
        let (status, body) = mixed
            .request("/v1/messages", "claude-sonnet-4-6", false, None)
            .await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("from copilot"));
    }
    assert_eq!(mixed.counts(), (0, 2));
    model_policy::configure(&mixed.daemon, |config| {
        config.api_key[0].allowed_providers = vec!["kiro".into(), "copilot".into()];
        config.api_key[0].allowed_models.clear();
        config.proxy_service[0].excluded_account_ids = vec!["kiro/acc_aaaaaaaa".into()];
    })
    .await;
    assert_eq!(
        mixed
            .request("/v1/messages", "claude-sonnet-4-6", false, None)
            .await
            .0,
        200
    );
    assert_eq!(mixed.counts(), (0, 3));
    model_policy::configure(&mixed.daemon, |config| {
        config.proxy_service[0].excluded_account_ids.clear();
        config.provider[0].enabled = false;
    })
    .await;
    assert_eq!(
        mixed
            .request("/v1/messages", "claude-sonnet-4-6", false, None)
            .await
            .0,
        200
    );
    assert_eq!(mixed.counts(), (0, 4));
    model_policy::configure(&mixed.daemon, |config| {
        config.provider[0].enabled = true;
        config.models.disabled = vec!["claude-sonnet-4.6".into()];
    })
    .await;
    let (status, body) = mixed
        .request("/v1/messages", "claude-sonnet-4-6", false, None)
        .await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("model_disabled"));
    assert_eq!(mixed.counts(), (0, 4));
    mixed.daemon.stop().await;
}

#[tokio::test]
async fn responses_continuations_stay_with_their_original_provider() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mixed = Mixed::start().await;
    for source in ["kiro", "copilot"] {
        let (status, body) = mixed
            .request("/v1/responses", "claude-sonnet-4-6", false, None)
            .await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains(&format!("from {source}")), "{body}");
        let response: Value = serde_json::from_str(&body).unwrap();
        let (status, next) = mixed
            .request(
                "/v1/responses",
                "claude-sonnet-4-6",
                false,
                response["id"].as_str(),
            )
            .await;
        assert_eq!(status, 200, "{next}");
        assert!(next.contains(&format!("from {source}")), "{next}");
    }
    assert_eq!(mixed.counts(), (2, 2));
    mixed.daemon.stop().await;
}

#[tokio::test]
async fn a_started_stream_is_not_replayed_and_its_failure_cools_the_provider() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mixed = Mixed::start().await;
    assert_eq!(
        mixed
            .request("/v1/messages", "claude-sonnet-4-6", false, None)
            .await
            .0,
        200
    );
    mixed.protocol_error.store(true, Ordering::SeqCst);
    let (status, body) = mixed
        .request("/v1/messages", "claude-sonnet-4-6", true, None)
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("partial from copilot"));
    assert!(!body.contains("from kiro"));
    assert_eq!(mixed.counts(), (1, 1));
    let (_, body) = mixed
        .request("/v1/messages", "claude-sonnet-4-6", false, None)
        .await;
    assert!(body.contains("from kiro"), "{body}");
    assert_eq!(mixed.counts(), (2, 1));
    mixed.daemon.stop().await;
}
