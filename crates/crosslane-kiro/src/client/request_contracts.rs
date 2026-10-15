use super::*;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use wiremock::matchers::body_json;

const PROFILE: &str = "arn:aws:codewhisperer:us-east-1:123456789012:profile/enterprise";

#[tokio::test]
async fn oauth_runtime_uses_agent_rpc_without_mutating_cli_or_source_payloads() {
    for auth in [AuthMethod::Idc, AuthMethod::Social] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/runtime"))
            .respond_with(move |request: &wiremock::Request| {
                assert_eq!(
                    request.headers["x-amz-target"],
                    "KiroRuntimeService.GenerateAssistantResponse"
                );
                assert_eq!(
                    request.headers["content-type"],
                    "application/x-amz-json-1.0"
                );
                assert!(request.headers["user-agent"]
                    .to_str()
                    .unwrap()
                    .contains("api/kiro-runtime#"));
                assert!(request.headers["user-agent"]
                    .to_str()
                    .unwrap()
                    .ends_with("KAS/0.66.32"));
                assert!(!request.headers.contains_key("x-amzn-kiro-agent-mode"));
                assert_eq!(request.headers["x-kiro-attempt"], "1;max=1");
                if auth == AuthMethod::Idc {
                    assert_eq!(request.headers["tokentype"], "SSO_OIDC");
                } else {
                    assert!(!request.headers.contains_key("tokentype"));
                }
                let wire: Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(wire["profileArn"], PROFILE);
                assert_eq!(wire["agentMode"], "spec");
                assert_eq!(wire["conversationState"]["agentTaskType"], "spectask");
                assert_eq!(
                    wire["conversationState"]["rootConversationId"],
                    "conversation"
                );
                assert_eq!(
                    wire["conversationState"]["agentContinuationId"],
                    "same-human-turn"
                );
                assert_eq!(
                    wire["conversationState"]["currentMessage"]["userInputMessage"]["origin"],
                    "AI_EDITOR"
                );
                ResponseTemplate::new(200)
            })
            .expect(1)
            .mount(&server)
            .await;
        let mut client = test_client(&server);
        client.upstream.agent_mode = AgentMode::Spec;
        let mut account = account(auth);
        account.profile_arn = Some(PROFILE.into());
        let mut payload = payload();
        payload.conversation_state.agent_continuation_id = Some("same-human-turn".into());
        let before = serde_json::to_value(&payload).unwrap();
        client
            .generate(&account, &payload, Some(EndpointKey::Runtime))
            .await
            .unwrap();
        assert_eq!(serde_json::to_value(&payload).unwrap(), before);
    }
}

#[tokio::test]
async fn default_mcp_uses_invoke_rpc_and_puts_oauth_profile_in_the_body() {
    for auth in [AuthMethod::Idc, AuthMethod::Social, AuthMethod::ApiKey] {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/"))
            .respond_with(move |request: &wiremock::Request| {
                assert_eq!(request.headers["x-amz-target"], "KiroRuntimeService.InvokeMCP");
                assert_eq!(request.headers["content-type"], "application/x-amz-json-1.0");
                assert_eq!(request.headers["x-amzn-codewhisperer-optout"], "true");
                assert!(!request.headers.contains_key("x-amzn-kiro-profile-arn"));
                let wire: Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(wire["jsonrpc"], "2.0");
                assert_eq!(wire["method"], "tools/call");
                assert_eq!(wire["params"], json!({"name":"web_search","arguments":{"query":"rust"}}));
                if auth == AuthMethod::ApiKey {
                    assert_eq!(request.headers["tokentype"], "API_KEY");
                    assert!(wire.get("profileArn").is_none());
                } else {
                    assert_eq!(wire["profileArn"], PROFILE);
                    assert_eq!(request.headers.contains_key("tokentype"), auth == AuthMethod::Idc);
                }
                ResponseTemplate::new(200).set_body_json(json!({
                    "jsonrpc":"2.0","result":{"content":[{"type":"text","text":"{\"results\":[],\"totalResults\":0}"}]}
                }))
            }).expect(1).mount(&server).await;
        let mut client = test_client(&server);
        client.overrides.mcp_url = None;
        client.overrides.runtime_url = Some(format!("{}/", server.uri()));
        let mut account = account(auth);
        account.profile_arn = Some(PROFILE.into());
        client.web_search(&account, "rust").await.unwrap();
    }
}

#[test]
fn management_routes_cover_both_partitions_and_keep_unsupported_region_fallbacks() {
    let client = KiroClient::new(UpstreamConfig::default(), EndpointOverrides::default()).unwrap();
    for (region, host) in [
        ("us-east-1", "management.us-east-1.kiro.dev"),
        ("eu-central-1", "management.eu-central-1.kiro.dev"),
        ("us-gov-east-1", "management.us-gov-east-1.kiro.dev"),
        ("us-gov-west-1", "management.us-gov-west-1.kiro.dev"),
        ("ap-southeast-1", "q.ap-southeast-1.amazonaws.com"),
    ] {
        let mut account = account(AuthMethod::Idc);
        account.credentials.region = region.into();
        let partition = if region.starts_with("us-gov-") {
            "aws-us-gov"
        } else {
            "aws"
        };
        account.profile_arn = Some(format!(
            "arn:{partition}:codewhisperer:{region}:123456789012:profile/test"
        ));
        assert_eq!(
            client.profile_discovery_urls(&account).unwrap()[0].host_str(),
            Some(host)
        );
        assert_eq!(
            client.usage_limits_urls(&account).unwrap()[0].host_str(),
            Some(host)
        );
        assert_eq!(
            client.web_search_url(&account).unwrap().as_str(),
            format!("https://runtime.{region}.kiro.dev/")
        );
    }
}

#[tokio::test]
async fn completed_profile_discovery_is_cached_but_token_region_and_expiry_invalidate_it() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/amazon/ListAvailableProfiles"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"profiles":[{"arn":PROFILE}]})),
        )
        .expect(4)
        .mount(&server)
        .await;
    let client = test_client(&server);
    let mut account = account(AuthMethod::Idc);
    let results =
        futures::future::join_all((0..10).map(|_| client.resolve_profile_arn(&account))).await;
    assert!(results.into_iter().all(|result| result.unwrap() == PROFILE));
    assert_eq!(client.resolve_profile_arn(&account).await.unwrap(), PROFILE);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    account.credentials.access_token = "rotated-access-token".into();
    client.resolve_profile_arn(&account).await.unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    account.credentials.region = "eu-central-1".into();
    client.resolve_profile_arn(&account).await.unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
    {
        let mut cache = client.profile_cache.lock().unwrap();
        let key = (
            account.id.clone(),
            account.credentials.access_token.clone(),
            account_api_region(&account).unwrap(),
        );
        cache.get_mut(&key).unwrap().expires = Instant::now() - Duration::from_secs(1);
    }
    client.resolve_profile_arn(&account).await.unwrap();
    account.profile_arn =
        Some("arn:aws:codewhisperer:eu-central-1:123456789012:profile/selected".into());
    assert_eq!(
        client.resolve_profile_arn(&account).await.unwrap(),
        account.profile_arn.unwrap()
    );
}

#[tokio::test]
async fn profile_discovery_follows_next_token_without_substituting_a_builder_profile() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/amazon/ListAvailableProfiles"))
        .and(body_json(json!({})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"profiles":[],"nextToken":"page-2"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/amazon/ListAvailableProfiles"))
        .and(body_json(json!({"nextToken":"page-2"})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"profiles":[{"arn":PROFILE}]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let profile = test_client(&server)
        .resolve_profile_arn(&account(AuthMethod::Idc))
        .await
        .unwrap();
    assert_eq!(profile, PROFILE);
}

#[tokio::test]
async fn profile_quota_errors_never_cache_a_builder_profile_or_refresh_authentication() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/amazon/ListAvailableProfiles"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "__type":"ThrottlingException", "reason":"USAGE_LIMIT_REACHED"
        })))
        .expect(2)
        .mount(&server)
        .await;
    let client = test_client(&server);
    let account = account(AuthMethod::Idc);
    for _ in 0..2 {
        let error = client.resolve_profile_arn(&account).await.unwrap_err();
        assert!(error.is_quota());
        assert!(!error.is_auth());
    }
    assert!(client.profile_cache.lock().unwrap().is_empty());
}

#[tokio::test]
async fn native_model_discovery_pages_are_complete_and_repeated_tokens_are_bounded() {
    for repeating in [false, true] {
        let server = MockServer::start().await;
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        Mock::given(method("POST"))
            .and(path("/management"))
            .respond_with(move |request: &wiremock::Request| {
                assert_eq!(
                    request.headers["x-amz-target"],
                    "KiroControlPlaneBearerService.ListAvailableModels"
                );
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(body["origin"], "AI_EDITOR");
                let phase = calls.fetch_add(1, Ordering::SeqCst);
                if phase == 0 {
                    assert!(body.get("nextToken").is_none());
                    ResponseTemplate::new(200).set_body_json(
                        json!({"models":[{"modelId":"model-1"}],"nextToken":"page-2"}),
                    )
                } else {
                    assert_eq!(body["nextToken"], "page-2");
                    let mut body = json!({"models":[{"modelId":"model-2"}]});
                    if repeating {
                        body["nextToken"] = json!("page-2");
                    }
                    ResponseTemplate::new(200).set_body_json(body)
                }
            })
            .expect(2)
            .mount(&server)
            .await;
        let mut client = test_client(&server);
        client.overrides.management_url = Some(format!("{}/management", server.uri()));
        let result = client
            .fetch_models_once(&power_account(AuthMethod::Idc))
            .await;
        if repeating {
            assert!(result.unwrap_err().message.contains("pagination limit"));
        } else {
            assert_eq!(
                result
                    .unwrap()
                    .iter()
                    .map(|model| model.model_id.as_str())
                    .collect::<Vec<_>>(),
                ["model-1", "model-2"]
            );
        }
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn management_override_is_shared_by_rest_profiles_usage_and_subscriptions() {
    let server = MockServer::start().await;
    for (operation, response) in [
        (
            "ListAvailableProfiles",
            json!({"profiles":[{"arn":PROFILE}]}),
        ),
        (
            "getUsageLimits",
            json!({"userInfo":{"userId":"synthetic-user"}}),
        ),
        ("listAvailableSubscriptions", json!({})),
    ] {
        Mock::given(path(format!("/management/us-east-1/{operation}")))
            .respond_with(move |request: &wiremock::Request| {
                assert!(request.headers["user-agent"]
                    .to_str()
                    .unwrap()
                    .contains("api/codewhisperer#"));
                assert!(!request.headers["user-agent"]
                    .to_str()
                    .unwrap()
                    .contains("KAS/"));
                assert!(!request.headers.contains_key("tokentype"));
                assert!(!request.headers.contains_key("x-amz-target"));
                ResponseTemplate::new(200).set_body_json(response.clone())
            })
            .expect(1)
            .mount(&server)
            .await;
    }
    let mut client = test_client(&server);
    client.overrides.management_url = Some(format!("{}/management/{{region}}", server.uri()));
    let account = account(AuthMethod::Idc);
    client.resolve_profile_arn(&account).await.unwrap();
    client.get_usage_limits(&account).await.unwrap();
    client.list_subscriptions(&account).await.unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn kiro_http_retry_hints_survive_error_decoding_and_are_bounded() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429)
            .insert_header("x-amzn-kiro-ratelimit-retry-after", "400000")
            .set_body_json(json!({"reason":"USER_REQUEST_RATE_EXCEEDED","message":"slow down","retryAfterMilliseconds":99})))
        .expect(1).mount(&server).await;
    let error = test_client(&server)
        .generate(&account(AuthMethod::Social), &payload(), None)
        .await
        .err()
        .unwrap();
    assert_eq!(error.retry_after_ms(), Some(300_000));
    assert!(error.is_throttle());
    assert!(!error.is_quota());
    assert_eq!(error.status, Some(429));
}

#[tokio::test]
async fn long_native_error_bodies_keep_the_reason_and_retry_hint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "message":"\"\\\n".repeat(3_000),
            "reason":"USAGE_LIMIT_REACHED", "retryAfterMilliseconds":1200
        })))
        .expect(1)
        .mount(&server)
        .await;
    let error = test_client(&server)
        .generate(&account(AuthMethod::Social), &payload(), None)
        .await
        .err()
        .unwrap();
    assert!(error.is_quota());
    assert_eq!(error.retry_after_ms(), Some(1200));
    assert!(error.message.chars().count() <= 2000);
    assert!(serde_json::from_str::<Value>(&error.message).is_ok());
}

#[tokio::test]
async fn unavailable_native_catalog_falls_back_without_reprobing_every_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/management"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/amazon/ListAvailableModels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":[{
            "modelId":"claude-sonnet-4", "modelName":"Claude Sonnet 4"
        }]})))
        .expect(2)
        .mount(&server)
        .await;
    let mut client = test_client(&server);
    client.overrides.management_url = Some(format!("{}/management", server.uri()));
    let mut account = account(AuthMethod::Idc);
    account.profile_arn = Some(PROFILE.into());
    for _ in 0..2 {
        let models = client.fetch_models_once(&account).await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model_id, "claude-sonnet-4");
    }
}

#[tokio::test]
async fn regions_outside_the_native_endpoint_table_use_the_legacy_model_contract() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/amazon/ListAvailableModels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":[{
            "modelId":"claude-sonnet-4", "modelName":"Claude Sonnet 4"
        }]})))
        .expect(1)
        .mount(&server)
        .await;
    let mut client = test_client(&server);
    client.upstream.preferred_endpoint = Some(crosslane_core::config::Endpoint::Runtime);
    let mut account = account(AuthMethod::Idc);
    account.profile_arn =
        Some("arn:aws:codewhisperer:ap-southeast-1:123456789012:profile/test".into());
    assert_eq!(client.fetch_models_once(&account).await.unwrap().len(), 1);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[test]
fn native_reason_codes_distinguish_quota_rate_capacity_and_context_failures() {
    for reason in [
        "USAGE_LIMIT_REACHED",
        "DAILY_REQUEST_COUNT",
        "OVERAGE_REQUEST_LIMIT_EXCEEDED",
    ] {
        let message = format!("ThrottlingException: {}", json!({"reason":reason}));
        for status in [None, Some(403), Some(429)] {
            let error = KiroError {
                status,
                endpoint: "runtime".into(),
                message: message.clone(),
            };
            assert!(error.is_quota(), "{reason}");
            assert!(!error.is_throttle(), "{reason}");
            assert!(!error.is_auth(), "{reason}");
        }
    }
    for reason in [
        "USER_REQUEST_RATE_EXCEEDED",
        "CREDIT_CONSUMPTION_RATE_EXCEEDED",
    ] {
        let message = json!({"reason":reason}).to_string();
        assert!(text_is_throttle_error(&message));
        assert!(!text_is_quota_error(&message));
        let error = KiroError {
            status: Some(403),
            endpoint: "runtime".into(),
            message,
        };
        assert!(error.is_throttle());
        assert!(!error.is_auth());
    }
    assert!(text_is_model_temporarily_unavailable(
        "INSUFFICIENT_MODEL_CAPACITY"
    ));
    assert!(text_is_request_rejection(
        "validationError: invalid tool input"
    ));
    for reason in [
        "PROMPT_TOO_LONG",
        "CONTENT_LENGTH_EXCEEDS_THRESHOLD",
        "MODEL_CONTEXT_WINDOW_EXCEEDED",
    ] {
        assert!(
            text_is_context_too_long(&json!({"reason":reason}).to_string()),
            "{reason}"
        );
    }
    let unauthorized = KiroError {
        status: Some(401),
        endpoint: "getUsageLimits".into(),
        message: "quota query requires authentication".into(),
    };
    assert!(unauthorized.is_auth());
}
