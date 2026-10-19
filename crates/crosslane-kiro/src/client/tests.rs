use crosslane_core::account::{AuthMethod, Credentials};
use crosslane_translate::{KiroConversationState, KiroCurrentMessage, KiroUserInputMessage};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

#[path = "request_contracts.rs"]
mod request_contracts;

#[cfg(feature = "crs")]
#[path = "runtime_isolated.rs"]
mod runtime_isolated;

#[tokio::test]
async fn disabled_models_block_direct_calls_and_clones_already_waiting_for_a_slot() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let policy = Arc::new(crosslane_provider::ModelPolicy::default());
    let client = test_client(&server).with_model_policy(Arc::clone(&policy));
    let old_client = client.clone();
    let slots = Arc::clone(&client.stream_slots)
        .acquire_many_owned(client.stream_slots.available_permits() as u32)
        .await
        .unwrap();
    let account = account(AuthMethod::Social);
    let payload = payload();
    let waiting = old_client.generate(&account, &payload, Some(EndpointKey::Codewhisperer));
    tokio::pin!(waiting);
    assert!(futures::poll!(&mut waiting).is_pending());
    policy
        .replace(&crosslane_core::config::ModelsConfig {
            disabled: vec!["sonnet4".into()],
            ..Default::default()
        })
        .await;
    drop(slots);
    let error = waiting.await.err().expect("queued request must be blocked");
    assert_eq!(error.status, Some(400));
    assert!(error.message.contains("model_disabled"));
    assert!(!error.is_auth() && !error.is_retriable());
    assert!(client.generate(&account, &payload, None).await.is_err());
    assert!(server.received_requests().await.unwrap().is_empty());
    policy.replace(&Default::default()).await;
    assert!(old_client
        .generate(&account, &payload, Some(EndpointKey::Codewhisperer))
        .await
        .is_ok());
}

fn account(method: AuthMethod) -> Account {
    Account {
        id: "acc_test".into(),
        email: "test@example.com".into(),
        label: None,
        enabled: true,
        machine_id: "a".repeat(64),
        profile_arn: None,
        upstream_user_id: None,
        credentials: Credentials {
            access_token: "access-token".into(),
            refresh_token: None,
            client_id: None,
            client_secret: None,
            region: "us-east-1".into(),
            expires_at: i64::MAX,
            auth_method: method,
        },
        usage: None,
        subscription: None,
        tags: Vec::new(),
        created_at: 0,
        credit_exhausted: false,
    }
}

fn assert_ide_identity(headers: &reqwest::header::HeaderMap, machine_id: &str) {
    let user_agent = headers["user-agent"].to_str().unwrap();
    assert!(user_agent.starts_with("aws-sdk-js/1.0.39 ua/2.1 "));
    assert!(user_agent.ends_with(&format!(" KiroIDE 1.2.56 {machine_id}")));
    assert!(
        user_agent.contains("api/codewhispererstreaming#1.0.39")
            || user_agent.contains("api/codewhisperer#1.0.39")
    );
    assert_eq!(
        headers["x-amz-user-agent"],
        format!("aws-sdk-js/1.0.39 KiroIDE 1.2.56 {machine_id}")
    );
}

fn assert_cli_identity(headers: &reqwest::header::HeaderMap, api: &str) {
    assert_eq!(
        headers["user-agent"],
        format!(
            "aws-sdk-rust/1.3.15 ua/2.1 api/{api}/0.1.17975 os/macos lang/rust/1.92.0 md/appVersion-2.21.4 app/AmazonQ-For-CLI"
        )
    );
    assert_eq!(
        headers["x-amz-user-agent"],
        format!(
            "aws-sdk-rust/1.3.15 ua/2.1 api/{api}/0.1.17975 os/macos lang/rust/1.92.0 m/F app/AmazonQ-For-CLI"
        )
    );
}

fn payload() -> KiroPayload {
    KiroPayload {
        conversation_state: KiroConversationState {
            agent_continuation_id: None,
            agent_task_type: None,
            chat_trigger_type: "MANUAL".into(),
            conversation_id: "conversation".into(),
            current_message: KiroCurrentMessage {
                user_input_message: KiroUserInputMessage {
                    content: "hello".into(),
                    model_id: "claude-sonnet-4".into(),
                    origin: "AI_EDITOR".into(),
                    images: Vec::new(),
                    documents: Vec::new(),
                    cache_point: None,
                    client_cache_config: None,
                    user_input_message_context: None,
                },
            },
            history: Vec::new(),
        },
        profile_arn: None,
        inference_config: None,
        additional_model_request_fields: None,
        model_request_intent: None,
        protected_history_messages: 0,
    }
}

fn test_client(server: &MockServer) -> KiroClient {
    KiroClient::new(
        UpstreamConfig::default(),
        EndpointOverrides {
            amazonq_url: Some(format!("{}/amazon/generateAssistantResponse", server.uri())),
            codewhisperer_url: Some(format!(
                "{}/codewhisperer/generateAssistantResponse",
                server.uri()
            )),
            mcp_url: Some(format!("{}/mcp", server.uri())),
            runtime_url: Some(format!("{}/runtime", server.uri())),
            management_url: None,
        },
    )
    .expect("client")
}

#[test]
fn formatted_upstream_errors_redact_api_keys_without_changing_classification() {
    let error = KiroError {
        status: Some(402),
        endpoint: "Runtime".into(),
        message: "Quota exhausted for ksk_synthetic-secret_123".into(),
    };
    assert!(error.is_quota());
    for formatted in [error.to_string(), format!("{error:?}")] {
        assert!(
            !formatted.contains("ksk_synthetic-secret_123"),
            "{formatted}"
        );
        assert!(formatted.contains("[REDACTED]"), "{formatted}");
        assert!(formatted.contains("Quota exhausted"), "{formatted}");
    }
    assert_eq!(
        error.message,
        "Quota exhausted for ksk_synthetic-secret_123"
    );
}

#[tokio::test]
async fn api_key_generation_uses_runtime_without_oauth_profile_or_refresh() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/runtime"))
        .respond_with(|request: &wiremock::Request| {
            assert_cli_identity(&request.headers, "codewhispererstreaming");
            assert_eq!(request.headers["authorization"], "Bearer ksk_test-key");
            assert_eq!(request.headers["tokentype"], "API_KEY");
            assert_eq!(
                request.headers["x-amz-target"],
                "AmazonCodeWhispererStreamingService.GenerateAssistantResponse"
            );
            assert_eq!(
                request.headers["content-type"],
                "application/x-amz-json-1.0"
            );
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            assert!(body.get("profileArn").is_none());
            assert!(body["conversationState"]
                .get("agentContinuationId")
                .is_none());
            let user = &body["conversationState"]["currentMessage"]["userInputMessage"];
            assert_eq!(user["origin"], "KIRO_CLI");
            assert_eq!(user["modelId"], "claude-opus-5");
            assert!(body["conversationState"]["history"][0]["userInputMessage"]
                .get("modelId")
                .is_none());
            ResponseTemplate::new(200).set_body_bytes(Vec::<u8>::new())
        })
        .expect(1)
        .mount(&server)
        .await;
    let client = test_client(&server);
    let mut account = account(AuthMethod::ApiKey);
    account.credentials.access_token = "ksk_test-key".into();
    account.credentials.expires_at = 0;
    let request: crosslane_translate::ClaudeRequest = serde_json::from_value(serde_json::json!({
        "model":"claude-opus-5", "max_tokens":256,
        "messages":[{"role":"user","content":"before"},{"role":"assistant","content":"done"},{"role":"user","content":"hello"}]
    })).unwrap();
    let mut payload = crosslane_translate::claude_to_kiro(
        &request,
        &crosslane_translate::TranslationOptions::new("claude-opus-5", "AI_EDITOR"),
    );
    payload.profile_arn = Some("stale-oauth-profile".into());
    let response = client
        .generate(&account, &payload, Some(EndpointKey::Amazonq))
        .await
        .unwrap();
    assert_eq!(response.endpoint.key, EndpointKey::Runtime);
    let models = client.list_models(&account).await.unwrap();
    assert!(models.iter().any(|model| model.model_id == "claude-opus-5"));
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "API keys must not trigger profile/catalog discovery"
    );
    assert!(!account.is_token_expiring(1_000_000, 300));
    let headers = mcp_headers(&account, false).unwrap();
    assert_eq!(headers["tokentype"], "API_KEY");
    assert!(!headers.contains_key("x-amzn-kiro-profile-arn"));
}

#[tokio::test]
async fn runtime_catalog_uses_management_rpc_and_oauth_profile() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/management"))
        .respond_with(|request: &wiremock::Request| {
            assert!(request.headers["user-agent"].to_str().unwrap().contains("api/kiro-controlplane-bearer#"));
            assert!(request.headers["user-agent"].to_str().unwrap().ends_with("KAS/0.66.32"));
            assert_eq!(request.headers["authorization"], "Bearer access-token");
            assert_eq!(request.headers["content-type"], "application/x-amz-json-1.0");
            assert!(request.headers.contains_key("amz-sdk-invocation-id"));
            assert_eq!(request.headers["amz-sdk-request"], "attempt=1; max=1");
            assert_eq!(request.headers["x-amz-target"], "KiroControlPlaneBearerService.ListAvailableModels");
            assert_eq!(request.headers["tokentype"], "SSO_OIDC");
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["origin"], "AI_EDITOR");
            assert_eq!(body["profileArn"], "arn:aws:codewhisperer:us-east-1:123456789012:profile/enterprise");
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"models":[{
                "modelId":"claude-opus-5", "modelName":"Opus 5",
                "additionalModelRequestFieldsSchema":{"properties":{"output_config":{"properties":{"effort":{"enum":["low","high"]}}}}}
            }]}))
        }).expect(1).mount(&server).await;
    let mut client = test_client(&server);
    client.upstream.preferred_endpoint = Some(crosslane_core::config::Endpoint::Runtime);
    client.overrides.management_url = Some(format!("{}/management", server.uri()));
    let models = client
        .list_models(&power_account(AuthMethod::Idc))
        .await
        .unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].model_name, "Opus 5");
}

#[test]
fn regional_endpoints_use_profile_region_and_keep_govcloud_isolated() {
    let mut account = account(AuthMethod::Idc);
    account.credentials.region = "eu-west-1".into();
    account.profile_arn = Some("arn:aws:codewhisperer:eu-central-1:123:profile/test".into());
    let overrides = EndpointOverrides::default();
    assert_eq!(
        EndpointDefinition::for_account(EndpointKey::Runtime, &overrides, &account)
            .unwrap()
            .url,
        "https://runtime.eu-central-1.kiro.dev/"
    );
    assert_eq!(
        EndpointDefinition::for_account(EndpointKey::Amazonq, &overrides, &account)
            .unwrap()
            .url,
        "https://q.eu-central-1.amazonaws.com/generateAssistantResponse"
    );
    account.credentials.region = "us-gov-west-1".into();
    account.profile_arn = Some(KIRO_BUILDER_ID_PROFILE_ARN.into());
    assert_eq!(
        EndpointCache::default().order(
            &account,
            Some(EndpointKey::Amazonq),
            EndpointPurpose::Generation
        ),
        vec![EndpointKey::Runtime]
    );
    assert_eq!(
        EndpointDefinition::for_account(EndpointKey::Runtime, &overrides, &account)
            .unwrap()
            .url,
        "https://runtime.us-gov-west-1.kiro.dev/"
    );
    assert_eq!(usage_api_regions(&account), ["us-gov-west-1"]);
    assert!(usage_profile_arn(&account).is_none());
    account.credentials.region = "bad/region".into();
    account.profile_arn = None;
    assert!(EndpointDefinition::for_account(EndpointKey::Runtime, &overrides, &account).is_err());
}

#[tokio::test]
async fn govcloud_replaces_commercial_profile_before_runtime_and_management_requests() {
    let server = MockServer::start().await;
    let profile = "arn:aws-us-gov:codewhisperer:us-gov-west-1:123456789012:profile/enterprise";
    Mock::given(method("POST"))
        .and(path("/management/us-gov-west-1/ListAvailableProfiles"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "profiles":[{"arn":KIRO_BUILDER_ID_PROFILE_ARN},{"arn":profile}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/management/us-gov-west-1"))
        .respond_with(move |request: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["profileArn"], profile);
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"models":[{
                "modelId":"claude-opus-5","modelName":"Opus 5"
            }]}))
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/runtime/us-gov-west-1"))
        .respond_with(move |request: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["profileArn"], profile);
            ResponseTemplate::new(200).set_body_bytes(Vec::<u8>::new())
        })
        .expect(1)
        .mount(&server)
        .await;
    let mut client = test_client(&server);
    client.overrides.management_url = Some(format!("{}/management/{{region}}", server.uri()));
    client.overrides.runtime_url = Some(format!("{}/runtime/{{region}}", server.uri()));
    let mut account = power_account(AuthMethod::Idc);
    account.credentials.region = "us-gov-west-1".into();
    account.profile_arn = Some(KIRO_BUILDER_ID_PROFILE_ARN.into());
    assert_eq!(client.list_models(&account).await.unwrap().len(), 1);
    let mut payload = payload();
    payload.profile_arn.clone_from(&account.profile_arn);
    let response = client
        .generate(&account, &payload, Some(EndpointKey::Amazonq))
        .await
        .unwrap();
    assert_eq!(response.endpoint.key, EndpointKey::Runtime);
    assert!(!server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .any(|request| request.url.path().contains("us-east-1")
            || request.url.path() == "/amazon/generateAssistantResponse"));
}

#[tokio::test]
async fn govcloud_without_a_profile_does_not_use_the_builder_id_fallback() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/amazon/ListAvailableProfiles"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "profiles":[{"arn":KIRO_BUILDER_ID_PROFILE_ARN}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let client = test_client(&server);
    let mut account = account(AuthMethod::Idc);
    account.credentials.region = "us-gov-west-1".into();
    assert!(client.resolve_profile_arn(&account).await.is_err());
}

fn generation_endpoint(server: &MockServer) -> EndpointDefinition {
    EndpointDefinition {
        key: EndpointKey::Amazonq,
        url: format!("{}/amazon/generateAssistantResponse", server.uri()),
        origin: "AI_EDITOR",
        amz_target: "",
        name: "AmazonQ",
    }
}

fn power_account(method: AuthMethod) -> Account {
    let mut account = account(method);
    account.profile_arn =
        Some("arn:aws:codewhisperer:us-east-1:123456789012:profile/enterprise".into());
    account.subscription = Some(Subscription {
        kind: SubscriptionKind::Power,
        title: Some("Kiro Power".into()),
        raw_type: Some("POWER".into()),
        expires_at: None,
        days_remaining: None,
    });
    account
}

#[tokio::test]
async fn idc_generation_uses_kiro_ide_origin_and_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/amazon/generateAssistantResponse"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let client = test_client(&server);
    let account = power_account(AuthMethod::Idc);

    let response = client
        .generate(&account, &payload(), None)
        .await
        .expect("enterprise IdC generation succeeds");
    drop(response);

    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert!(request.headers.get("x-amz-target").is_none());
    assert_ide_identity(&request.headers, &account.machine_id);
    let body: serde_json::Value = serde_json::from_slice(&request.body).expect("JSON body");
    assert_eq!(
        body.pointer("/conversationState/currentMessage/userInputMessage/origin")
            .and_then(serde_json::Value::as_str),
        Some("AI_EDITOR")
    );
}

#[tokio::test]
async fn generation_rejects_unprepared_tool_history_before_network_io() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/amazon/generateAssistantResponse"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let client = test_client(&server);
    let invalid: KiroPayload = serde_json::from_value(serde_json::json!({
        "conversationState": {
            "chatTriggerType": "MANUAL",
            "conversationId": "conversation",
            "history": [
                {"userInputMessage": {
                    "content": "start", "modelId": "model", "origin": "AI_EDITOR"
                }},
                {"assistantResponseMessage": {
                    "content": "calling", "toolUses": [{
                        "toolUseId": "call_1", "name": "lookup", "input": {}
                    }]
                }}
            ],
            "currentMessage": {"userInputMessage": {
                "content": "latest", "modelId": "model", "origin": "AI_EDITOR",
                "userInputMessageContext": {
                    "tools": [{"toolSpecification": {
                        "name": "lookup", "description": "",
                        "inputSchema": {"json": {"type": "object"}}
                    }}],
                    "toolResults": [{
                        "toolUseId": "orphan", "status": "success",
                        "content": [{"text": "valuable orphan output"}]
                    }]
                }
            }}
        },
        "profileArn": null,
        "inferenceConfig": null
    }))
    .expect("payload");

    let error = match client
        .send_generation(
            &account(AuthMethod::Idc),
            &invalid,
            generation_endpoint(&server),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("unprepared payload must not bypass request accounting"),
    };

    let requests = server.received_requests().await.expect("requests");
    assert!(requests.is_empty());
    assert_eq!(error.status, Some(400));
    assert!(error.message.contains("without prepared tool history"));
}

#[tokio::test]
async fn generation_times_out_when_waiting_for_a_stream_slot() {
    let server = MockServer::start().await;
    let mut upstream = UpstreamConfig::default();
    upstream.pool.stream_max_connections = 1;
    upstream.stream_slot_wait_timeout_ms = 20;
    let client = KiroClient::new(upstream, EndpointOverrides::default()).expect("client");
    let _held = client.stream_slots.acquire().await.expect("held slot");

    let error = match client
        .send_generation(
            &account(AuthMethod::Idc),
            &payload(),
            generation_endpoint(&server),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("slot wait must time out"),
    };

    assert!(error
        .message
        .contains("waiting for an upstream stream slot"));
}

#[tokio::test]
async fn generation_times_out_when_upstream_sends_no_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/amazon/generateAssistantResponse"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(500)))
        .mount(&server)
        .await;
    let upstream = UpstreamConfig {
        stream_read_timeout_ms: 20,
        ..UpstreamConfig::default()
    };
    let client = KiroClient::new(upstream, EndpointOverrides::default()).expect("client");

    let started = Instant::now();
    let error = match client
        .send_generation(
            &account(AuthMethod::Idc),
            &payload(),
            generation_endpoint(&server),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("silent upstream must time out"),
    };

    assert!(error.status.is_none());
    assert!(started.elapsed() < Duration::from_millis(300));
}

#[tokio::test]
async fn generation_tries_401_fallback_serially_and_caches_success() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/amazon/generateAssistantResponse"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Auth error"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/codewhisperer/generateAssistantResponse"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let client = test_client(&server);
    let account = account(AuthMethod::Idc);

    let response = client
        .generate(&account, &payload(), None)
        .await
        .expect("fallback succeeds");

    assert_eq!(response.endpoint.key, EndpointKey::Codewhisperer);
    assert_eq!(
        client
            .endpoint_cache()
            .preferred(&account.id, EndpointPurpose::Generation),
        Some(EndpointKey::Codewhisperer)
    );
    let paths = server
        .received_requests()
        .await
        .expect("requests")
        .into_iter()
        .map(|request| {
            assert_ide_identity(&request.headers, &account.machine_id);
            request.url.path().to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec![
            "/amazon/generateAssistantResponse",
            "/codewhisperer/generateAssistantResponse"
        ]
    );
}

#[tokio::test]
async fn generation_maps_throttled_403_to_429_without_switching_or_disabling() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/amazon/generateAssistantResponse"))
        .respond_with(
            ResponseTemplate::new(403).set_body_string("ThrottlingException: rate exceeded"),
        )
        .mount(&server)
        .await;
    let client = test_client(&server);
    let account = account(AuthMethod::Idc);

    let error = match client.generate(&account, &payload(), None).await {
        Ok(_) => panic!("throttled request unexpectedly succeeded"),
        Err(error) => error,
    };

    assert_eq!(error.status, Some(429));
    assert!(error.is_throttle());
    assert_eq!(
        client
            .endpoint_cache()
            .order(&account, None, EndpointPurpose::Generation)[0],
        EndpointKey::Amazonq
    );
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), "/amazon/generateAssistantResponse");
}

#[tokio::test]
async fn generation_disables_plain_403_and_uses_cached_fallback_next_time() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/amazon/generateAssistantResponse"))
        .respond_with(ResponseTemplate::new(403).set_body_string("AccessDeniedException"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/codewhisperer/generateAssistantResponse"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let client = test_client(&server);
    let account = account(AuthMethod::Idc);

    client
        .generate(&account, &payload(), None)
        .await
        .expect("first fallback succeeds");
    client
        .generate(&account, &payload(), None)
        .await
        .expect("cached fallback succeeds");

    let paths = server
        .received_requests()
        .await
        .expect("requests")
        .into_iter()
        .map(|request| request.url.path().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec![
            "/amazon/generateAssistantResponse",
            "/codewhisperer/generateAssistantResponse",
            "/codewhisperer/generateAssistantResponse"
        ]
    );
}

#[tokio::test]
async fn concurrent_model_discovery_is_collapsed_and_401_uses_fallback() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/amazon/ListAvailableModels"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_delay(Duration::from_millis(25))
                .set_body_string("Auth error"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/codewhisperer/ListAvailableModels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"modelId": "claude-sonnet-4"}]
        })))
        .mount(&server)
        .await;
    let client = test_client(&server);
    let account = account(AuthMethod::Idc);

    let (first, second) = tokio::join!(client.list_models(&account), client.list_models(&account));

    assert_eq!(first.expect("first fetch")[0].model_id, "claude-sonnet-4");
    assert_eq!(second.expect("shared fetch")[0].model_id, "claude-sonnet-4");
    let paths = server
        .received_requests()
        .await
        .expect("requests")
        .into_iter()
        .map(|request| {
            assert_ide_identity(&request.headers, &account.machine_id);
            request.url.path().to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec![
            "/amazon/ListAvailableModels",
            "/codewhisperer/ListAvailableModels"
        ]
    );
}

#[tokio::test]
async fn enterprise_model_discovery_sends_profile_and_ide_origin() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/amazon/ListAvailableModels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"modelId": "claude-opus-4.6"}]
        })))
        .mount(&server)
        .await;
    let client = test_client(&server);
    let account = power_account(AuthMethod::Idc);

    let models = client
        .list_models(&account)
        .await
        .expect("enterprise model discovery succeeds");

    assert_eq!(models[0].model_id, "claude-opus-4.6");
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 1);
    assert_ide_identity(&requests[0].headers, &account.machine_id);
    let query = requests[0]
        .url
        .query_pairs()
        .into_owned()
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(query.get("origin").map(String::as_str), Some("AI_EDITOR"));
    assert_eq!(query.get("maxResults").map(String::as_str), Some("50"));
    assert_eq!(
        query.get("profileArn").map(String::as_str),
        account.profile_arn.as_deref()
    );
}

#[tokio::test]
async fn model_discovery_does_not_switch_endpoints_for_server_errors() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/amazon/ListAvailableModels"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let client = test_client(&server);
    let account = account(AuthMethod::Idc);

    let error = client
        .fetch_models_once(&account)
        .await
        .expect_err("server error fails the request");

    assert_eq!(error.status, Some(500));
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), "/amazon/ListAvailableModels");
}

#[tokio::test]
async fn enterprise_model_discovery_failure_uses_static_catalog() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/amazon/ListAvailableModels"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let client = test_client(&server);
    let account = power_account(AuthMethod::Idc);

    let models = client
        .list_models(&account)
        .await
        .expect("static catalog keeps model listing available");

    assert!(models
        .iter()
        .any(|model| model.model_id == "claude-opus-4.6"));
}

#[tokio::test]
async fn subscriptions_use_only_the_auth_inferred_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/amazon/listAvailableSubscriptions"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/codewhisperer/listAvailableSubscriptions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;
    let client = test_client(&server);
    let account = account(AuthMethod::Idc);

    let subscriptions = client
        .list_subscriptions(&account)
        .await
        .expect("subscription failures degrade to an empty response");

    assert!(subscriptions.subscription_plans.is_empty());
    assert!(subscriptions.disclaimer.is_empty());
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), "/amazon/listAvailableSubscriptions");
    assert_ide_identity(&requests[0].headers, &account.machine_id);
}

#[tokio::test]
async fn web_search_calls_mcp_and_decodes_nested_json_text() {
    let server = MockServer::start().await;
    let nested = serde_json::json!({
        "query":"rust async",
        "totalResults":3,
        "results":[
            {"title":"Tokio","url":"https://tokio.rs/","snippet":"runtime","publishedDate":123},
            {"title":"","url":"https://example.org/page","snippet":"title fallback"},
            {"title":"unsafe","url":"javascript:alert(1)","snippet":"drop me"}
        ]
    })
    .to_string();
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc":"2.0",
            "id":"response",
            "result":{"content":[{"type":"text","text":nested}],"isError":false}
        })))
        .mount(&server)
        .await;
    let client = test_client(&server);
    let mut account = account(AuthMethod::Idc);
    account.profile_arn =
        Some("arn:aws:codewhisperer:us-east-1:123456789012:profile/test-profile".into());
    let results = client
        .web_search(&account, "rust async")
        .await
        .expect("search");

    assert_eq!(results.total_results, 3);
    assert_eq!(results.results.len(), 2);
    assert_eq!(results.results[0].title, "Tokio");
    assert_eq!(results.results[1].title, "example.org");
    let requests = server.received_requests().await.expect("requests");
    let request = requests.last().expect("MCP request");
    assert_eq!(request.url.path(), "/mcp");
    assert_ide_identity(&request.headers, &account.machine_id);
    assert_eq!(
        request
            .headers
            .get("x-amzn-codewhisperer-optout")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        request
            .headers
            .get("x-amzn-kiro-profile-arn")
            .and_then(|value| value.to_str().ok()),
        account.profile_arn.as_deref()
    );
    let body: serde_json::Value = serde_json::from_slice(&request.body).expect("JSON body");
    assert_eq!(body["method"], "tools/call");
    assert_eq!(body["params"]["name"], "web_search");
    assert_eq!(body["params"]["arguments"]["query"], "rust async");
}

#[tokio::test]
async fn web_search_surfaces_json_rpc_errors_without_generation_fallback() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc":"2.0",
            "id":"response",
            "error":{"code":-32000,"message":"unavailable"}
        })))
        .mount(&server)
        .await;
    let client = test_client(&server);
    let mut account = account(AuthMethod::Idc);
    account.profile_arn =
        Some("arn:aws:codewhisperer:us-east-1:123456789012:profile/test-profile".into());
    let error = client
        .web_search(&account, "news")
        .await
        .expect_err("JSON-RPC error");

    assert_eq!(error.status, Some(502));
    assert!(error.message.contains("JSON-RPC error"));
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), "/mcp");
}

#[tokio::test]
async fn missing_idc_profile_is_discovered_once_per_access_token() {
    let server = MockServer::start().await;
    let discovered = "arn:aws:codewhisperer:us-east-1:123456789012:profile/discovered-profile";
    Mock::given(method("POST"))
        .and(path("/amazon/ListAvailableProfiles"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(50))
                .set_body_json(serde_json::json!({
                    "profiles":[{"arn":discovered}]
                })),
        )
        .mount(&server)
        .await;
    let client = test_client(&server);
    let account = account(AuthMethod::Idc);

    let (left, right) = tokio::join!(
        client.resolve_profile_arn(&account),
        client.resolve_profile_arn(&account)
    );

    assert_eq!(left.expect("left profile"), discovered);
    assert_eq!(right.expect("right profile"), discovered);
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), "/amazon/ListAvailableProfiles");
    assert_ide_identity(&requests[0].headers, &account.machine_id);
    assert_eq!(
        requests[0]
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer access-token")
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&requests[0].body).expect("JSON body"),
        serde_json::json!({})
    );
}

#[tokio::test]
async fn builder_id_profile_is_used_when_profile_listing_is_forbidden() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/amazon/ListAvailableProfiles"))
        .respond_with(
            ResponseTemplate::new(403).set_body_string("User is not authorized to make this call"),
        )
        .mount(&server)
        .await;
    let client = test_client(&server);

    let profile = client
        .resolve_profile_arn(&account(AuthMethod::Idc))
        .await
        .expect("Builder ID fallback");

    assert_eq!(profile, KIRO_BUILDER_ID_PROFILE_ARN);
    assert_eq!(server.received_requests().await.expect("requests").len(), 1);
}

#[tokio::test]
async fn usage_limits_send_complete_enterprise_query_and_runtime_headers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/amazon/getUsageLimits"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "userInfo":{"userId":"stable-user","email":"test@example.com"},
            "usageBreakdownList":[]
        })))
        .mount(&server)
        .await;
    let client = test_client(&server);
    let mut account = account(AuthMethod::Idc);
    let profile_arn = "arn:aws:codewhisperer:us-east-1:123456789012:profile/enterprise-profile";
    account.profile_arn = Some(profile_arn.into());

    let limits = client
        .get_usage_limits(&account)
        .await
        .expect("usage limits");

    assert_eq!(
        limits.user_info.as_ref().map(|user| user.user_id.as_str()),
        Some("stable-user")
    );
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_ide_identity(&request.headers, &account.machine_id);
    let query = request
        .url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(query.get("origin").map(String::as_str), Some("AI_EDITOR"));
    assert_eq!(
        query.get("resourceType").map(String::as_str),
        Some("AGENTIC_REQUEST")
    );
    assert_eq!(
        query.get("isEmailRequired").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        query.get("profileArn").map(String::as_str),
        Some(profile_arn)
    );
    assert!(request.headers.contains_key("amz-sdk-invocation-id"));
    assert_eq!(
        request
            .headers
            .get("amz-sdk-request")
            .and_then(|value| value.to_str().ok()),
        Some("attempt=1; max=1")
    );
}

#[tokio::test]
async fn builder_id_usage_omits_the_synthetic_profile_arn() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/amazon/getUsageLimits"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;
    let client = test_client(&server);
    let mut account = account(AuthMethod::Idc);
    account.profile_arn = Some(KIRO_BUILDER_ID_PROFILE_ARN.into());

    client
        .get_usage_limits(&account)
        .await
        .expect("Builder ID usage limits");

    let requests = server.received_requests().await.expect("requests");
    let query = requests[0]
        .url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<std::collections::HashMap<_, _>>();
    assert!(!query.contains_key("profileArn"));
    assert_eq!(
        query.get("isEmailRequired").map(String::as_str),
        Some("true")
    );
}

#[tokio::test]
async fn usage_limits_retry_the_regional_endpoint_after_a_403() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/primary/getUsageLimits"))
        .respond_with(ResponseTemplate::new(403).set_body_string("AccessDeniedException"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fallback/getUsageLimits"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "userInfo":{"userId":"stable-user"}
        })))
        .mount(&server)
        .await;
    let client = test_client(&server);
    let account = account(AuthMethod::Idc);
    let urls = ["primary", "fallback"]
        .into_iter()
        .map(|path| {
            usage_limits_url(&format!("{}/{path}", server.uri()), &account).expect("usage URL")
        })
        .collect();

    let limits = client
        .get_usage_limits_from_urls(&account, urls)
        .await
        .expect("regional fallback");

    assert_eq!(
        limits.user_info.as_ref().map(|user| user.user_id.as_str()),
        Some("stable-user")
    );
    let paths = server
        .received_requests()
        .await
        .expect("requests")
        .into_iter()
        .map(|request| {
            assert_ide_identity(&request.headers, &account.machine_id);
            request.url.path().to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec!["/primary/getUsageLimits", "/fallback/getUsageLimits"]
    );
}

#[test]
fn usage_endpoint_regions_follow_profile_then_sso_region() {
    let mut account = account(AuthMethod::Idc);
    account.credentials.region = "eu-west-1".into();
    assert_eq!(
        usage_api_regions(&account),
        vec!["eu-central-1", "us-east-1"]
    );

    account.profile_arn =
        Some("arn:aws:codewhisperer:ap-southeast-1:123456789012:profile/enterprise".into());
    assert_eq!(
        usage_api_regions(&account),
        vec!["ap-southeast-1", "us-east-1"]
    );
}

#[tokio::test]
async fn social_profile_does_not_require_discovery() {
    let server = MockServer::start().await;
    let client = test_client(&server);

    let profile = client
        .resolve_profile_arn(&account(AuthMethod::Social))
        .await
        .expect("Social profile");

    assert_eq!(profile, KIRO_SOCIAL_PROFILE_ARN);
    assert!(server
        .received_requests()
        .await
        .expect("requests")
        .is_empty());
}

#[tokio::test]
async fn web_search_rejects_an_unresolved_profile_before_network_io() {
    let server = MockServer::start().await;
    let client = test_client(&server);

    let error = client
        .web_search(&account(AuthMethod::Idc), "news")
        .await
        .expect_err("unresolved profile");

    assert!(error.message.contains("profile ARN was not resolved"));
    assert!(server
        .received_requests()
        .await
        .expect("requests")
        .is_empty());
}

#[test]
fn usage_limits_normalize_base_trial_bonus_and_subscription() {
    let limits: UsageLimits = serde_json::from_value(serde_json::json!({
        "usageBreakdownList":[{
            "resourceType":"CREDIT",
            "currentUsageWithPrecision":12.5,
            "usageLimitWithPrecision":100.0,
            "freeTrialInfo":{
                "freeTrialStatus":"ACTIVE",
                "currentUsage":2.0,
                "usageLimit":10.0
            },
            "bonuses":[{"currentUsage":1.0,"usageLimit":5.0,"status":"ACTIVE"}]
        }],
        "nextDateReset":2_000_000_000i64,
        "daysUntilReset":7,
        "subscriptionInfo":{
            "subscriptionTitle":"Kiro Pro+",
            "type":"Q_DEVELOPER_STANDALONE_PRO_PLUS"
        },
        "userInfo":{
            "email":"alice@example.com",
            "userId":"user-123"
        }
    }))
    .expect("usage response");
    let usage = limits.normalized_usage(123).expect("credit usage");
    assert_eq!(usage.current, 15.5);
    assert_eq!(usage.limit, 115.0);
    assert_eq!(usage.updated_at, 123);
    let subscription = limits.normalized_subscription().expect("subscription");
    assert_eq!(subscription.kind, SubscriptionKind::ProPlus);
    assert_eq!(subscription.days_remaining, Some(7));
    assert_eq!(subscription.expires_at, Some(2_000_000_000));
    let identity = limits.user_info.expect("authenticated identity");
    assert_eq!(identity.email, "alice@example.com");
    assert_eq!(identity.user_id, "user-123");
}

#[test]
fn usage_limits_include_enabled_overage_capacity() {
    let mut response = serde_json::json!({
        "overageConfiguration":{"overageStatus":"ENABLED"},
        "usageBreakdownList":[{
            "resourceType":"CREDIT",
            "currentUsageWithPrecision":10000.0,
            "usageLimitWithPrecision":10000.0,
            "currentOveragesWithPrecision":0.0,
            "overageCapWithPrecision":10000.0
        }]
    });
    let normalize = |response: &serde_json::Value| {
        serde_json::from_value::<UsageLimits>(response.clone())
            .expect("usage response")
            .normalized_usage(123)
            .expect("credit usage")
    };

    let available = normalize(&response);
    assert_eq!(available.current, 10000.0);
    assert_eq!(available.limit, 20000.0);
    assert_eq!(available.overage_cap, Some(10000.0));
    assert_eq!(available.limit_without_overage(), 10000.0);
    assert_eq!(available.percent_used, 50.0);

    response["usageBreakdownList"][0]["currentUsageWithPrecision"] = serde_json::json!(13000.0);
    response["usageBreakdownList"][0]["currentOveragesWithPrecision"] = serde_json::json!(3000.0);
    let spending_overage = normalize(&response);
    assert_eq!(spending_overage.current, 13000.0);
    assert_eq!(spending_overage.limit, 20000.0);

    response["usageBreakdownList"][0]["currentUsageWithPrecision"] = serde_json::json!(20000.0);
    assert_eq!(normalize(&response).current, normalize(&response).limit);

    response["usageBreakdownList"][0]["currentUsageWithPrecision"] = serde_json::json!(10000.0);
    response["overageConfiguration"]["overageStatus"] = serde_json::json!("DISABLED");
    let disabled = normalize(&response);
    assert_eq!(disabled.current, 10000.0);
    assert_eq!(disabled.limit, 10000.0);
    assert_eq!(disabled.overage_cap, None);
}

#[test]
fn context_overflow_codes_and_window_errors_are_recoverable() {
    for message in [
        r#"{"reason":"CONTEXT_LENGTH_EXCEEDED","message":"ValidationException"}"#,
        r#"{"reason":"INPUT_TOO_LONG"}"#,
        r#"{"message":"Input content length exceeds threshold.","reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD"}"#,
        "Input content length exceeds threshold.",
        "The context window has been exceeded",
    ] {
        let error = KiroError {
            status: Some(400),
            endpoint: "AmazonQ".into(),
            message: message.into(),
        };
        assert!(error.is_context_too_long(), "{message}");
        assert!(error.is_request_rejection(), "{message}");
    }
    for message in [
        "ValidationException: tool schema request too large",
        "request body size exceeds the 8 MiB limit",
        "Internal Server Error",
    ] {
        assert!(!text_is_context_too_long(message), "{message}");
    }
}

#[test]
fn framed_error_text_is_classified_like_http_statuses() {
    let auth = KiroError {
        status: None,
        endpoint: "event-stream".into(),
        message: "AccessDeniedException: expired token".into(),
    };
    assert!(auth.is_auth());
    let throttle = KiroError {
        status: None,
        endpoint: "event-stream".into(),
        message: "ThrottlingException: rate exceeded".into(),
    };
    assert!(throttle.is_throttle());
    assert!(throttle.is_retriable());
    let quota = KiroError {
        status: None,
        endpoint: "event-stream".into(),
        message: "credits exhausted".into(),
    };
    assert!(quota.is_quota());

    let payload = KiroError {
        status: Some(500),
        endpoint: "AmazonQ".into(),
        message: "ValidationException: tool schema request too large".into(),
    };
    assert!(payload.is_request_rejection());
    assert!(!payload.is_context_too_long());
    assert!(payload.is_retriable());

    let context = KiroError {
        status: Some(400),
        endpoint: "AmazonQ".into(),
        message: "prompt is too long: context length exceeded".into(),
    };
    assert!(context.is_request_rejection());
    assert!(context.is_context_too_long());

    let model_unavailable = KiroError {
        status: Some(500),
        endpoint: "AmazonQ".into(),
        message: r#"{"message":"Encountered unexpectedly high load when processing the request, please try again.","reason":"MODEL_TEMPORARILY_UNAVAILABLE"}"#.into(),
    };
    assert!(model_unavailable.is_model_temporarily_unavailable());
    assert!(model_unavailable.is_model_capacity_error());
    assert!(model_unavailable.is_retriable());
    assert!(!model_unavailable.is_throttle());
    assert!(!model_unavailable.is_request_rejection());

    let ordinary_server_error = KiroError {
        status: Some(500),
        endpoint: "AmazonQ".into(),
        message: "Internal Server Error".into(),
    };
    assert!(ordinary_server_error.is_retriable());
    assert!(!ordinary_server_error.is_model_capacity_error());
    assert!(!ordinary_server_error.is_context_too_long());
}

#[tokio::test]
async fn first_generation_preserves_prepared_controls_for_every_account_and_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(8)
        .mount(&server)
        .await;
    let client = test_client(&server);
    let request: crosslane_translate::ClaudeRequest = serde_json::from_value(serde_json::json!({
        "model":"claude-sonnet-4.6", "max_tokens":4096, "top_k":42,
        "thinking":{"type":"adaptive"}, "cache_control":{"type":"ephemeral","ttl":"1h"},
        "tools":[{"name":"lookup","input_schema":{"type":"object"},"cache_control":{"type":"ephemeral","ttl":"1h"}}],
        "messages":[
            {"role":"user","content":"start"},
            {"role":"assistant","content":[
                {"type":"thinking","thinking":"private prior reasoning","signature":"stale-signature"},
                {"type":"text","text":"visible answer","cache_control":{"type":"ephemeral","ttl":"1h"}}
            ]},
            {"role":"user","content":[{"type":"document","source":{"type":"text","media_type":"text/plain","data":"Report"},
                "title":"Report","context":"Use the totals on page two","citations":{"enabled":true}}]}
        ]
    })).expect("Claude request");
    crosslane_translate::validate_claude(&request).expect("valid request");
    let mut options =
        crosslane_translate::TranslationOptions::new("claude-sonnet-4.6", "AI_EDITOR");
    options.enable_prompt_cache = true;
    for schema in [
        None,
        Some(serde_json::json!({"properties":{
            "thinking":{"properties":{"type":{"enum":["adaptive","disabled"]}}},
            "output_config":{"properties":{"effort":{"enum":["low","high"]}}}
        }})),
    ] {
        options.additional_model_request_fields_schema = schema;
        let payload = crosslane_translate::claude_to_kiro(&request, &options);
        for index in 0..2 {
            let mut account = power_account(AuthMethod::Idc);
            account.id = format!("account-{index}");
            for endpoint in [EndpointKey::Amazonq, EndpointKey::Codewhisperer] {
                drop(
                    client
                        .generate(&account, &payload, Some(endpoint))
                        .await
                        .expect("one successful request"),
                );
            }
        }
    }
    let requests = server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 8);
    for (index, request) in requests.into_iter().enumerate() {
        let body: serde_json::Value = serde_json::from_slice(&request.body).expect("request JSON");
        assert!(body
            .pointer("/additionalModelRequestFields/top_k")
            .is_none());
        if index < 4 {
            assert!(body.get("additionalModelRequestFields").is_none());
        } else {
            assert_eq!(
                body["additionalModelRequestFields"]["thinking"]["type"],
                "adaptive"
            );
        }
        let current = body
            .pointer("/conversationState/currentMessage/userInputMessage")
            .expect("current user");
        assert_eq!(current["cachePoint"], serde_json::json!({"type":"default"}));
        assert_eq!(
            current["userInputMessageContext"]["tools"][1],
            serde_json::json!({"cachePoint":{"type":"default"}})
        );
        assert!(current["documents"][0].get("context").is_none());
        assert_eq!(current["documents"][0]["citations"]["enabled"], true);
        assert!(current["content"]
            .as_str()
            .expect("text")
            .contains("Use the totals on page two"));
        let wire = body.to_string();
        assert!(!wire.contains("\"ttl\""));
        assert!(!wire.contains("reasoningContent"));
        assert!(!wire.contains("private prior reasoning"));
        assert!(!wire.contains("stale-signature"));
    }
}

#[tokio::test]
async fn request_validation_errors_never_trigger_field_probes_or_capability_learning() {
    for message in [
        "ValidationException: top_k is not allowed",
        "ValidationException: top_k must be greater than zero",
        "ValidationException: cachePoint is invalid",
        "ValidationException: document context is not allowed",
        "ValidationException: document citations are not allowed",
        "ValidationException: improperly formed request",
        "THINKING_SIGNATURE_INVALID",
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(400).set_body_json(serde_json::json!({"message":message})),
            )
            .expect(2)
            .mount(&server)
            .await;
        let client = test_client(&server);
        let mut request = payload();
        // An already-prepared payload can still be rejected for a value or
        // service-side error. A field name in that error is not a capability.
        request.additional_model_request_fields = Some(serde_json::json!({"top_k":42}));
        for index in 0..2 {
            let mut account = power_account(AuthMethod::Idc);
            account.id = format!("account-{index}");
            let error = match client.generate(&account, &request, None).await {
                Err(error) => error,
                Ok(_) => panic!("the upstream rejection must be returned"),
            };
            assert_eq!(error.status, Some(400), "{message}");
            assert!(error.message.contains(message), "{error}");
        }
        let requests = server.received_requests().await.expect("received requests");
        assert_eq!(requests.len(), 2, "no hidden probe retries for {message}");
        for request in requests {
            let body: serde_json::Value =
                serde_json::from_slice(&request.body).expect("request JSON");
            assert_eq!(
                body.pointer("/additionalModelRequestFields/top_k"),
                Some(&serde_json::json!(42))
            );
        }
    }
}
