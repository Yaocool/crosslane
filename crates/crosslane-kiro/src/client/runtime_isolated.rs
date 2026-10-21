use super::*;
use serde_json::{json, Value};

fn prepared_tool_payload() -> KiroPayload {
    serde_json::from_value(json!({
        "profileArn":"arn:aws:codewhisperer:us-east-1:123456789012:profile/frozen",
        "inferenceConfig":{"maxTokens":128000,"temperature":0.4,"topP":0.9},
        "additionalModelRequestFields":{
            "thinking":{"type":"adaptive","display":"summarized"},
            "output_config":{"effort":"high"}
        },
        "conversationState":{
            "conversationId":"conversation-preserved",
            "agentContinuationId":"continuation-preserved",
            "chatTriggerType":"MANUAL",
            "history":[
                {"userInputMessage":{
                    "content":"read the checkpoint", "modelId":"claude-opus-5", "origin":"AI_EDITOR"
                }},
                {"assistantResponseMessage":{
                    "content":"", "toolUses":[{
                        "toolUseId":"call-preserved", "name":"checkpoint_reader", "input":{"path":"checkpoint.txt"}
                    }]
                }},
                {"userInputMessage":{
                    "content":"", "modelId":"claude-opus-5", "origin":"AI_EDITOR",
                    "userInputMessageContext":{"toolResults":[{
                        "toolUseId":"call-preserved", "status":"success", "content":[{"text":"checkpoint content"}]
                    }]}
                }},
                {"assistantResponseMessage":{"content":"checkpoint read"}}
            ],
            "currentMessage":{"userInputMessage":{
                "content":"continue", "modelId":"claude-opus-5", "origin":"AI_EDITOR",
                "userInputMessageContext":{"tools":[{"toolSpecification":{
                    "name":"checkpoint_reader", "description":"Read a local checkpoint",
                    "inputSchema":{"json":{"type":"object","properties":{"path":{"type":"string"}}}}
                }}]}
            }}
        }
    }))
    .unwrap()
}

#[tokio::test]
async fn claude_five_tools_use_native_termination_without_changing_other_protocols() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(2)
        .mount(&server)
        .await;
    let client = test_client(&server);
    let account = power_account(AuthMethod::Idc);
    let mut payload = prepared_tool_payload();
    payload.model_request_intent = Some(crosslane_translate::ModelRequestIntent {
        crs_native_messages: true,
        ..Default::default()
    });
    let before = format!("{:?}", client.cache);
    drop(client.generate(&account, &payload, None).await.unwrap());
    assert_eq!(format!("{:?}", client.cache), before);
    payload
        .model_request_intent
        .as_mut()
        .unwrap()
        .crs_native_messages = false;
    drop(client.generate(&account, &payload, None).await.unwrap());
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests[0].url.path(), "/runtime");
    assert_eq!(requests[1].url.path(), "/amazon/generateAssistantResponse");
    for request in requests {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["inferenceConfig"]["maxTokens"], 128000);
        assert!(!String::from_utf8_lossy(&request.body).contains("crs_native_messages"));
    }
}

#[test]
fn native_termination_is_scoped_to_claude_five_tool_requests() {
    let mut payload = prepared_tool_payload();
    payload.model_request_intent = Some(crosslane_translate::ModelRequestIntent {
        crs_native_messages: true,
        ..Default::default()
    });
    assert!(payload.requires_native_tool_termination());
    payload
        .conversation_state
        .current_message
        .user_input_message
        .model_id = "claude-opus-5.5".into();
    assert!(payload.requires_native_tool_termination());
    payload
        .conversation_state
        .current_message
        .user_input_message
        .model_id = "claude-sonnet-4.6".into();
    assert!(!payload.requires_native_tool_termination());
    payload
        .conversation_state
        .current_message
        .user_input_message
        .model_id = "claude-opus-5".into();
    payload
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context = None;
    assert!(!payload.requires_native_tool_termination());
}

#[tokio::test]
async fn tool_free_native_summary_keeps_output_controls_and_endpoint_policy() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let client = test_client(&server);
    let account = power_account(AuthMethod::Idc);
    let mut payload = prepared_tool_payload();
    payload
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context = None;
    crosslane_translate::prepare_compaction_source_history(&mut payload);
    payload.model_request_intent = Some(crosslane_translate::ModelRequestIntent {
        crs_native_summary: true,
        ..Default::default()
    });
    assert!(!payload.requires_native_tool_termination());
    assert!(payload.requires_native_generation());
    let before = format!("{:?}", client.cache);
    drop(client.generate(&account, &payload, None).await.unwrap());
    assert_eq!(format!("{:?}", client.cache), before);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests[0].url.path(), "/runtime");
    let wire: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(wire["inferenceConfig"]["maxTokens"], 128000);
    assert_eq!(
        wire["additionalModelRequestFields"]["output_config"]["effort"],
        "high"
    );
    assert!(!String::from_utf8_lossy(&requests[0].body).contains("crs_native_summary"));
}

#[tokio::test]
async fn runtime_isolated_preserves_frozen_controls_history_and_native_wire_contract() {
    for (auth, previous) in [
        (AuthMethod::Idc, EndpointKey::Amazonq),
        (AuthMethod::Social, EndpointKey::Codewhisperer),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/runtime"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let mut client = test_client(&server);
        client.upstream.agent_mode = AgentMode::Spec;
        let account = power_account(auth);
        client
            .cache
            .mark_success(&account.id, EndpointPurpose::Generation, previous);
        let cache_before = format!("{:?}", client.cache);
        let payload = prepared_tool_payload();
        let before = serde_json::to_value(&payload).unwrap();

        let response = client
            .generate_runtime_isolated(&account, &payload, previous)
            .await
            .expect("automatic OAuth legacy attempt is eligible")
            .expect("one native Runtime attempt");
        assert!(response.thinking_enabled());
        let (endpoint, response, permit, saved_payload) = response.into_parts_with_payload();
        assert_eq!(endpoint.key, EndpointKey::Runtime);
        drop((response, permit));
        assert_eq!(serde_json::to_value(saved_payload).unwrap(), before);
        assert_eq!(serde_json::to_value(&payload).unwrap(), before);
        assert_eq!(format!("{:?}", client.cache), cache_before);

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(
            request.headers["x-amz-target"],
            "KiroRuntimeService.GenerateAssistantResponse"
        );
        assert_eq!(request.headers["authorization"], "Bearer access-token");
        assert_eq!(request.headers["x-kiro-attempt"], "1;max=1");
        assert_eq!(
            request.headers.contains_key("tokentype"),
            auth == AuthMethod::Idc
        );
        assert!(request.headers["user-agent"]
            .to_str()
            .unwrap()
            .contains("api/kiro-runtime#"));
        let mut expected = before;
        expected["agentMode"] = json!("spec");
        expected["conversationState"]["agentTaskType"] = json!("spectask");
        expected["conversationState"]["rootConversationId"] = json!("conversation-preserved");
        for item in expected["conversationState"]["history"]
            .as_array_mut()
            .unwrap()
        {
            if let Some(user) = item
                .get_mut("userInputMessage")
                .and_then(Value::as_object_mut)
            {
                user.remove("modelId");
            }
        }
        assert_eq!(
            serde_json::from_slice::<Value>(&request.body).unwrap(),
            expected
        );
    }
}

#[tokio::test]
async fn runtime_isolated_skips_explicit_preferences_runtime_api_keys_govcloud_and_disabled() {
    let server = MockServer::start().await;
    for case in 0..12 {
        let mut client = test_client(&server);
        let mut account = power_account(AuthMethod::Idc);
        let mut payload = prepared_tool_payload();
        let mut previous = EndpointKey::Amazonq;
        match case {
            0 => {
                client.upstream.preferred_endpoint = Some(crosslane_core::config::Endpoint::Amazonq)
            }
            1 => {
                client.upstream.preferred_endpoint =
                    Some(crosslane_core::config::Endpoint::Codewhisperer)
            }
            2 => {
                client.upstream.preferred_endpoint = Some(crosslane_core::config::Endpoint::Runtime)
            }
            3 => previous = EndpointKey::Runtime,
            4 => account.credentials.auth_method = AuthMethod::ApiKey,
            5 => account.credentials.region = "us-gov-west-1".into(),
            6 => {
                account.profile_arn =
                    Some("arn:aws-us-gov:codewhisperer:us-gov-east-1:123:profile/test".into())
            }
            7 => client.cache.mark_disabled(
                &account.id,
                EndpointPurpose::Generation,
                EndpointKey::Runtime,
            ),
            8 => account.credentials.region = "invalid/region".into(),
            9 => payload.profile_arn = None,
            10 => payload.profile_arn = Some(" ".into()),
            11 => {
                payload.profile_arn =
                    Some("arn:aws-us-gov:codewhisperer:us-gov-east-1:123:profile/test".into())
            }
            _ => unreachable!(),
        }
        let before = format!("{:?}", client.cache);
        assert!(
            !client.runtime_isolated_retry_eligible(&account, &payload, previous),
            "case {case}"
        );
        assert!(
            client
                .generate_runtime_isolated(&account, &payload, previous)
                .await
                .is_none(),
            "case {case}"
        );
        assert_eq!(format!("{:?}", client.cache), before);
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn runtime_isolated_terminal_errors_never_switch_endpoints_or_change_cache() {
    for (status, message, expected) in [
        (401, "UnauthorizedException", 401),
        (403, "AccessDeniedException", 403),
        (403, "Quota exhausted", 429),
        (403, "ThrottlingException: rate exceeded", 429),
        (400, "ValidationException", 400),
        (500, "Internal Server Error", 500),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/runtime"))
            .respond_with(ResponseTemplate::new(status).set_body_string(message))
            .expect(1)
            .mount(&server)
            .await;
        let client = test_client(&server);
        let account = power_account(AuthMethod::Idc);
        client.cache.mark_success(
            &account.id,
            EndpointPurpose::Generation,
            EndpointKey::Amazonq,
        );
        client
            .cache
            .mark_disabled(&account.id, EndpointPurpose::Models, EndpointKey::Runtime);
        let before = format!("{:?}", client.cache);
        let error = client
            .generate_runtime_isolated(&account, &prepared_tool_payload(), EndpointKey::Amazonq)
            .await
            .unwrap()
            .err()
            .expect("upstream error");
        assert_eq!(error.status, Some(expected));
        assert_eq!(error.endpoint, "KiroRuntime");
        assert_eq!(format!("{:?}", client.cache), before);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn endpoint_isolated_preserves_exact_legacy_endpoint_on_auth_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codewhisperer/generateAssistantResponse"))
        .respond_with(ResponseTemplate::new(401).set_body_string("UnauthorizedException"))
        .expect(1)
        .mount(&server)
        .await;
    let client = test_client(&server);
    let account = power_account(AuthMethod::Idc);
    let before = format!("{:?}", client.cache);
    let error = client
        .generate_at_endpoint_isolated(
            &account,
            &prepared_tool_payload(),
            EndpointKey::Codewhisperer,
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.status, Some(401));
    assert_eq!(error.endpoint, "CodeWhisperer");
    assert_eq!(format!("{:?}", client.cache), before);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn endpoint_isolated_never_discovers_profiles_or_uses_incompatible_or_disabled_endpoints() {
    let server = MockServer::start().await;
    for case in 0..6 {
        let client = test_client(&server);
        let mut account = power_account(AuthMethod::Idc);
        let mut payload = prepared_tool_payload();
        let mut endpoint = EndpointKey::Runtime;
        match case {
            0 => payload.profile_arn = None,
            1 => {
                payload.profile_arn = Some(" ".into());
                account.credentials.auth_method = AuthMethod::Social;
            }
            2 => {
                account.credentials.auth_method = AuthMethod::ApiKey;
                endpoint = EndpointKey::Amazonq;
            }
            3 => {
                account.credentials.region = "us-gov-west-1".into();
                endpoint = EndpointKey::Codewhisperer;
            }
            4 => account.credentials.region = "us-gov-west-1".into(),
            5 => client.cache.mark_disabled(
                &account.id,
                EndpointPurpose::Generation,
                EndpointKey::Runtime,
            ),
            _ => unreachable!(),
        }
        let before = format!("{:?}", client.cache);
        let error = client
            .generate_at_endpoint_isolated(&account, &payload, endpoint)
            .await
            .err()
            .unwrap();
        assert_eq!(error.status, Some(400), "case {case}");
        assert_eq!(error.endpoint, "isolated-endpoint-policy", "case {case}");
        assert!(!error.is_auth(), "case {case}");
        assert_eq!(format!("{:?}", client.cache), before);
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn endpoint_isolated_admits_models_before_profile_handling_and_again_after_slot_wait() {
    let server = MockServer::start().await;
    let policy = Arc::new(crosslane_provider::ModelPolicy::default());
    let client = test_client(&server).with_model_policy(Arc::clone(&policy));
    let account = power_account(AuthMethod::Idc);
    let payload = payload();
    let disabled = crosslane_core::config::ModelsConfig {
        disabled: vec!["sonnet4".into()],
        ..Default::default()
    };
    policy.replace(&disabled).await;
    let error = client
        .generate_at_endpoint_isolated(&account, &payload, EndpointKey::Runtime)
        .await
        .err()
        .unwrap();
    assert!(error.message.contains("model_disabled"));
    policy.replace(&Default::default()).await;

    let slots = Arc::clone(&client.stream_slots)
        .acquire_many_owned(client.stream_slots.available_permits() as u32)
        .await
        .unwrap();
    let mut payload = payload;
    payload.profile_arn.clone_from(&account.profile_arn);
    let pending = client.generate_at_endpoint_isolated(&account, &payload, EndpointKey::Runtime);
    tokio::pin!(pending);
    assert!(futures::poll!(&mut pending).is_pending());
    policy.replace(&disabled).await;
    drop(slots);
    let error = pending.await.err().unwrap();
    assert!(error.message.contains("model_disabled"));
    assert!(server.received_requests().await.unwrap().is_empty());
}
