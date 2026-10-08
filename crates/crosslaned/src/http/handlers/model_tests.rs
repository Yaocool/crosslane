use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::*;

#[test]
fn token_count_authorization_uses_the_resolved_kiro_model() {
    let available = vec![
        "claude-sonnet-4-6".to_string(),
        "claude-haiku-4-5".to_string(),
    ];
    assert_eq!(
        resolve_kiro_authorization_model("sonnet4.6", "", &available),
        "claude-sonnet-4-6"
    );
    assert_eq!(
        resolve_kiro_authorization_model("unknown", "claude-haiku-4-5", &available),
        "claude-haiku-4-5"
    );
}

#[test]
fn conversation_fallback_is_stable_uuid_and_isolated_by_client() {
    let first = vec![serde_json::json!({"role":"user","content":"hello"})];
    let extended = vec![
        first[0].clone(),
        serde_json::json!({"role":"assistant","content":"hi"}),
        serde_json::json!({"role":"user","content":"continue"}),
    ];
    let first_hint = conversation_fingerprint(&first).expect("first fingerprint");
    // Values produced before the project rename must remain stable on upgrade.
    assert_eq!(first_hint, "3f05d621eba2cb9dce12cca69c9cf56f");
    let extended_hint = conversation_fingerprint(&extended).expect("extended fingerprint");
    assert_eq!(first_hint, extended_hint);

    let headers = HeaderMap::new();
    let first_id = stable_conversation_id(&headers, Some("key-a"), None, Some(&first_hint))
        .expect("conversation ID");
    assert_eq!(first_id, "bf57593b-2583-87b1-a786-44a2063ba6e2");
    assert_eq!(
        stable_conversation_id(&headers, Some("key-a"), Some("session-a"), None),
        Some("c47b7d8b-e08a-8f9b-b54b-68a03facf042".to_owned())
    );
    let second_id = stable_conversation_id(&headers, Some("key-a"), None, Some(&extended_hint))
        .expect("conversation ID");
    assert_eq!(first_id, second_id);
    assert_eq!(
        Uuid::parse_str(&first_id).expect("UUID").get_version_num(),
        8
    );
    assert_ne!(
        stable_conversation_id(&headers, Some("key-b"), None, Some(&first_hint)),
        Some(first_id.clone())
    );

    let mut session_headers = HeaderMap::new();
    session_headers.insert(
        "x-claude-code-session-id",
        HeaderValue::from_static("header-session"),
    );
    let explicit = stable_conversation_id(
        &session_headers,
        Some("key-a"),
        Some("explicit-session"),
        Some(&first_hint),
    );
    let without_header = stable_conversation_id(
        &HeaderMap::new(),
        Some("key-a"),
        Some("explicit-session"),
        Some(&first_hint),
    );
    assert_eq!(
        explicit, without_header,
        "explicit IDs take priority over headers"
    );
    for header in [
        "session-id",
        "thread-id",
        "x-client-request-id",
        "session_id",
    ] {
        let mut headers = HeaderMap::new();
        headers.insert(header, HeaderValue::from_static("codex-session"));
        let id = stable_conversation_id(&headers, Some("key-a"), None, Some(&first_hint));
        assert_eq!(
            id,
            stable_conversation_id(&headers, Some("key-a"), None, Some("different history"))
        );
        assert_ne!(
            id,
            stable_conversation_id(&headers, Some("key-b"), None, Some(&first_hint))
        );
        headers.insert(header, HeaderValue::from_static("another-codex-session"));
        assert_ne!(
            id,
            stable_conversation_id(&headers, Some("key-a"), None, Some(&first_hint))
        );
    }
}

#[test]
fn internal_response_errors_have_proxy_classification() {
    assert_eq!(
        classify_api_error(StatusCode::INTERNAL_SERVER_ERROR, "tokenizer worker failed",),
        ("proxy_internal_error", "internal")
    );

    let error = ApiError::response_assembly(
        "failed to assemble encrypted web-search response",
        ErrorFormat::Claude,
    );
    assert_eq!(error.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(error.error_code, "proxy_internal_error");
    assert_eq!(error.error_stage, "response_assembly");
}

#[tokio::test]
async fn timed_out_summary_task_still_runs_its_accounting_tail() {
    let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let task_completed = Arc::clone(&completed);
    let task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        task_completed.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(GeneratedCompactionSummary {
            content: "late summary".into(),
            usage: CompactionIterationUsage {
                input_tokens: 100,
                output_tokens: 20,
            },
        })
    });

    let error = match await_compaction_summary_task(task, 1).await {
        Ok(_) => panic!("summary should time out"),
        Err(error) => error,
    };
    assert!(error.message.contains("timed out"));
    tokio::time::timeout(Duration::from_secs(1), async {
        while !completed.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached accounting tail completed");
}

#[tokio::test]
async fn timed_out_summary_task_is_aborted_after_the_bounded_grace() {
    struct DropMarker(Arc<AtomicBool>);

    impl Drop for DropMarker {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let dropped = Arc::new(AtomicBool::new(false));
    let task_dropped = Arc::clone(&dropped);
    let task = tokio::spawn(async move {
        let _marker = DropMarker(task_dropped);
        std::future::pending::<Result<GeneratedCompactionSummary, CompactionSummaryFailure>>().await
    });

    let result = await_compaction_summary_task_with_policy(
        "trace_timeout_grace",
        task,
        1,
        CancellationToken::new(),
        Duration::from_millis(5),
        Duration::from_millis(5),
    )
    .await;
    let error = match result {
        Ok(_) => panic!("summary should time out"),
        Err(error) => error,
    };
    assert!(error.message.contains("timed out"));
    tokio::time::timeout(Duration::from_secs(1), async {
        while !dropped.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("summary task was aborted after the bounded grace");
}

#[tokio::test]
async fn service_quota_alert_does_not_cancel_the_daemon() {
    let directory = tempfile::tempdir().expect("tempdir");
    let paths = crosslane_core::paths::Paths::from_env_values(
        Some(directory.path().to_str().expect("utf8")),
        None,
        None,
        None,
    );
    crosslane_store::bootstrap::ensure_layout(&paths)
        .await
        .expect("layout");
    let accounts = crosslane_store::accounts::AccountStore::load(&paths.accounts_file)
        .await
        .expect("accounts");
    let state = Arc::new(AppState::new(
        paths,
        crosslane_store::config_loader::ConfigHandle::new(crosslane_core::config::Config::default()),
        accounts,
    ));

    crate::alerts::sync_service_quota(&state).await;

    let shutdown = state.shutdown.clone();
    assert!(
        tokio::time::timeout(Duration::from_millis(1_200), shutdown.cancelled())
            .await
            .is_err(),
        "quota degradation must not stop the daemon or administration plane"
    );
}

#[tokio::test]
async fn context_limits_use_the_resolved_kiro_model_alias() {
    let directory = tempfile::tempdir().expect("tempdir");
    let paths = crosslane_core::paths::Paths::from_env_values(
        Some(directory.path().to_str().expect("utf8")),
        None,
        None,
        None,
    );
    crosslane_store::bootstrap::ensure_layout(&paths)
        .await
        .expect("layout");
    let accounts = crosslane_store::accounts::AccountStore::load(&paths.accounts_file)
        .await
        .expect("accounts");
    let state = Arc::new(AppState::new(
        paths,
        crosslane_store::config_loader::ConfigHandle::new(crosslane_core::config::Config::default()),
        accounts,
    ));
    state.models.finish_refresh(vec![crosslane_kiro::ModelInfo {
        model_id: "claude-sonnet-4.6".into(),
        model_name: String::new(),
        description: String::new(),
        rate_multiplier: None,
        token_limits: Some(crosslane_kiro::client::TokenLimits {
            max_input_tokens: Some(1_000_000),
            max_output_tokens: Some(16_384),
        }),
        additional_model_request_fields_schema: None,
    }]);

    assert_eq!(
        model_token_limit(&state, "claude-4.6-sonnet", true),
        Some(1_000_000)
    );
    assert!(check_context_limit(&state, 900_000, false, "claude-4.6-sonnet").is_ok());
    assert!(check_context_limit(&state, 960_000, false, "claude-4.6-sonnet").is_err());
}

#[tokio::test]
async fn final_model_controls_do_not_borrow_another_versions_schema_or_disabled_mode() {
    let directory = tempfile::tempdir().unwrap();
    let paths = crosslane_core::paths::Paths::from_env_values(
        Some(directory.path().to_str().unwrap()),
        None,
        None,
        None,
    );
    crosslane_store::bootstrap::ensure_layout(&paths)
        .await
        .unwrap();
    let accounts = crosslane_store::accounts::AccountStore::load(&paths.accounts_file)
        .await
        .unwrap();
    let mut config = crosslane_core::config::Config::default();
    // Persisted legacy settings must not resurrect turn-based suppression.
    config.features.adaptive_thinking = true;
    config
        .model_thinking_mode
        .insert("claude-opus-4.7".into(), false);
    config
        .model_thinking_mode
        .insert("claude-haiku".into(), true);
    let state = AppState::new(
        paths,
        crosslane_store::config_loader::ConfigHandle::new(config),
        accounts,
    );
    state.models.finish_refresh(
        serde_json::from_value(json!([{
            "modelId":"claude-opus-4.6",
            "additionalModelRequestFieldsSchema":{"properties":{"output_config":{"properties":{
                "effort":{"enum":["low","medium","high"]}
            }}}}
        }, {"modelId":"claude-haiku-4.5", "additionalModelRequestFieldsSchema":null}]))
        .unwrap(),
    );
    let request: ClaudeRequest = serde_json::from_value(json!({
        "model":"claude-opus-4.6", "max_tokens":4096,
        "thinking":{"type":"adaptive"},
        "output_config":{"effort":"high"},
        "messages":[{"role":"user","content":"explain"}]
    }))
    .unwrap();
    let mut payload = claude_to_kiro(
        &request,
        &TranslationOptions::new("claude-opus-4.6", "AI_EDITOR"),
    );
    state.prepare_model_request(&mut payload);
    assert_eq!(
        payload.additional_model_request_fields.as_ref().unwrap()["output_config"]["effort"],
        "high"
    );

    payload
        .conversation_state
        .current_message
        .user_input_message
        .content = "continue".into();
    assert!(state.prepare_model_request(&mut payload).enabled);

    set_payload_model(&mut payload, "claude-opus-4.8");
    let decision = state.prepare_model_request(&mut payload);
    assert_eq!(
        decision.reason,
        crosslane_translate::model::ThinkingReason::ModelControlsUnavailable
    );
    assert!(payload.additional_model_request_fields.is_none());

    // An operator allow rule cannot create a capability the actual model lacks.
    set_payload_model(&mut payload, "claude-haiku-4.5");
    assert!(!state.prepare_model_request(&mut payload).enabled);
    assert!(serde_json::to_value(&payload)
        .unwrap()
        .get("additionalModelRequestFields")
        .is_none());

    set_payload_model(&mut payload, "claude-opus-4.7");
    let decision = state.prepare_model_request(&mut payload);
    assert!(!decision.enabled);
    assert!(!payload.thinking_enabled());
    assert!(payload.additional_model_request_fields.is_none());

    set_payload_model(&mut payload, "claude-opus-4.6");
    state.prepare_model_request(&mut payload);
    assert!(payload.thinking_enabled());
    assert_eq!(
        payload.additional_model_request_fields.as_ref().unwrap()["output_config"]["effort"],
        "high"
    );
}

#[tokio::test]
async fn continuation_budget_is_checked_after_tool_history_repair() {
    let mut payload: KiroPayload = serde_json::from_value(json!({
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
        }
    }))
    .expect("payload");
    let before_bytes = serde_json::to_vec(&payload).expect("before").len();
    let mut repaired = payload.clone();
    prepare_kiro_payload(&mut repaired, "test", "test repair").expect("repair");
    let repaired_bytes = serde_json::to_vec(&repaired).expect("after").len();
    assert!(repaired_bytes > before_bytes);

    let directory = tempfile::tempdir().expect("tempdir");
    let paths = crosslane_core::paths::Paths::from_env_values(
        Some(directory.path().to_str().expect("utf8")),
        None,
        None,
        None,
    );
    crosslane_store::bootstrap::ensure_layout(&paths)
        .await
        .expect("layout");
    let accounts = crosslane_store::accounts::AccountStore::load(&paths.accounts_file)
        .await
        .expect("accounts");
    let mut config = crosslane_core::config::Config::default();
    config.context.max_upstream_payload_bytes = repaired_bytes - 1;
    let state = Arc::new(AppState::new(
        paths,
        crosslane_store::config_loader::ConfigHandle::new(config),
        accounts,
    ));

    let error = validate_internal_continuation(
        &state,
        &mut payload,
        false,
        "test",
        "regression test",
        false,
    )
    .await
    .expect_err("repaired payload must be checked against the byte budget");

    assert_eq!(
        serde_json::to_vec(&payload).expect("prepared").len(),
        repaired_bytes
    );
    assert!(error.message.contains("too large after regression test"));
}

#[tokio::test]
async fn resolved_web_search_profile_is_saved_to_the_account_store() {
    let directory = tempfile::tempdir().expect("tempdir");
    let paths = crosslane_core::paths::Paths::from_env_values(
        Some(directory.path().to_str().expect("utf8")),
        None,
        None,
        None,
    );
    crosslane_store::bootstrap::ensure_layout(&paths)
        .await
        .expect("layout");
    let accounts_file = paths.accounts_file.clone();
    let mut accounts = crosslane_store::accounts::AccountStore::load(&accounts_file)
        .await
        .expect("accounts");
    accounts
        .insert(crosslane_core::account::Account {
            id: "acc_profile".into(),
            email: "profile@example.com".into(),
            label: None,
            enabled: true,
            machine_id: "a".repeat(64),
            profile_arn: None,
            upstream_user_id: None,
            credentials: crosslane_core::account::Credentials {
                access_token: "access-token".into(),
                refresh_token: None,
                client_id: None,
                client_secret: None,
                region: "us-east-1".into(),
                expires_at: i64::MAX,
                auth_method: crosslane_core::account::AuthMethod::Social,
            },
            usage: None,
            subscription: None,
            tags: Vec::new(),
            created_at: 0,
            credit_exhausted: false,
        })
        .expect("insert account");
    accounts.save().await.expect("save account");
    let state = Arc::new(AppState::new(
        paths,
        crosslane_store::config_loader::ConfigHandle::new(crosslane_core::config::Config::default()),
        accounts,
    ));
    let lease = state
        .pool()
        .acquire("", 0.0, &[])
        .await
        .expect("account lease");

    let resolved = ensure_web_search_profile_arn(&state, &lease)
        .await
        .expect("resolved account");

    const SOCIAL_PROFILE_ARN: &str =
        "arn:aws:codewhisperer:us-east-1:699475941385:profile/EHGA3GRVQMUK";
    assert_eq!(resolved.profile_arn.as_deref(), Some(SOCIAL_PROFILE_ARN));
    drop(lease);
    let persisted = crosslane_store::accounts::AccountStore::load(&accounts_file)
        .await
        .expect("persisted accounts");
    assert_eq!(
        persisted
            .find("acc_profile")
            .and_then(|account| account.profile_arn.as_deref()),
        Some(SOCIAL_PROFILE_ARN)
    );
}

#[test]
fn fallback_chooses_the_highest_lower_model_in_the_same_family() {
    let models = ["claude-opus-4-5", "claude-opus-4-6", "claude-sonnet-4-9"]
        .into_iter()
        .map(|model_id| crosslane_kiro::ModelInfo {
            model_id: model_id.into(),
            model_name: String::new(),
            description: String::new(),
            rate_multiplier: None,
            token_limits: None,
            additional_model_request_fields_schema: None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        find_model_fallback("claude-opus-4-7", &models).as_deref(),
        Some("claude-opus-4-6")
    );
}

#[test]
fn cold_start_resolves_enterprise_model_alias_from_static_catalog() {
    let account = crosslane_core::account::Account {
        id: "acc_enterprise".into(),
        email: "enterprise@example.com".into(),
        label: None,
        enabled: true,
        machine_id: "a".repeat(64),
        profile_arn: Some("arn:aws:codewhisperer:us-east-1:123456789012:profile/enterprise".into()),
        upstream_user_id: None,
        credentials: crosslane_core::account::Credentials {
            access_token: "access-token".into(),
            refresh_token: None,
            client_id: None,
            client_secret: None,
            region: "us-east-1".into(),
            expires_at: i64::MAX,
            auth_method: crosslane_core::account::AuthMethod::Idc,
        },
        usage: None,
        subscription: Some(crosslane_core::account::Subscription {
            kind: crosslane_core::account::SubscriptionKind::Power,
            title: Some("Kiro Power".into()),
            raw_type: Some("POWER".into()),
            expires_at: None,
            days_remaining: None,
        }),
        tags: Vec::new(),
        created_at: 0,
        credit_exhausted: false,
    };

    assert_eq!(
        resolve_static_model(&account, "claude-opus-4-6").as_deref(),
        Some("claude-opus-4.6")
    );
}

#[test]
fn remote_image_addresses_must_be_public() {
    for address in [
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
        IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1)),
        IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
        IpAddr::V6(Ipv6Addr::LOCALHOST),
        IpAddr::V6("fc00::1".parse().expect("valid IPv6")),
        IpAddr::V6("fe80::1".parse().expect("valid IPv6")),
        IpAddr::V6("::ffff:127.0.0.1".parse().expect("valid mapped IPv6")),
        IpAddr::V6("64:ff9b::7f00:1".parse().expect("valid NAT64 IPv6")),
        IpAddr::V6("2001:db8::1".parse().expect("valid documentation IPv6")),
    ] {
        assert!(!is_public_address(address), "{address} must be rejected");
    }
    assert!(is_public_address(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
    assert!(is_public_address(IpAddr::V6(
        "2606:4700:4700::1111".parse().expect("public IPv6")
    )));
}

#[test]
fn remote_attachment_types_require_matching_content_signatures() {
    let image_url = Url::parse("https://example.com/image.png").expect("URL");
    assert_eq!(
        validate_remote_media_type(
            RemoteAttachmentKind::Image,
            Some("image/png"),
            &image_url,
            b"\x89PNG\r\n\x1a\nrest"
        ),
        Some("image/png")
    );
    assert!(validate_remote_media_type(
        RemoteAttachmentKind::Image,
        Some("image/jpeg"),
        &image_url,
        b"\x89PNG\r\n\x1a\nrest"
    )
    .is_none());

    let pdf_url = Url::parse("https://example.com/report.pdf").expect("URL");
    assert_eq!(
        validate_remote_media_type(
            RemoteAttachmentKind::Document,
            Some("application/pdf"),
            &pdf_url,
            b"%PDF-1.7"
        ),
        Some("application/pdf")
    );
    assert!(validate_remote_media_type(
        RemoteAttachmentKind::Document,
        Some("application/pdf"),
        &pdf_url,
        b"not a PDF"
    )
    .is_none());

    let docx_url = Url::parse("https://example.com/report.docx").expect("URL");
    assert_eq!(
        validate_remote_media_type(
            RemoteAttachmentKind::Document,
            Some("application/octet-stream"),
            &docx_url,
            b"PK\x03\x04archive"
        ),
        Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document")
    );
}

#[test]
fn retry_limit_is_never_expanded_by_account_count() {
    assert_eq!(retry_attempt_count(3, 50), 4);
    assert_eq!(retry_attempt_count(3, 2), 2);
    assert_eq!(retry_attempt_count(0, 0), 1);
}

#[test]
fn opaque_upstream_bad_requests_are_reported_as_gateway_failures() {
    for message in ["Internal Server Error", "", "{}"] {
        let error = upstream_api_error(
            KiroError {
                status: Some(400),
                endpoint: "test".into(),
                message: message.into(),
            },
            RequestLogContext::default(),
            ErrorFormat::Claude,
        );
        assert_eq!(error.status, StatusCode::BAD_GATEWAY, "{message:?}");
    }

    let actionable = upstream_api_error(
        KiroError {
            status: Some(400),
            endpoint: "test".into(),
            message: "prompt is too long: 200001 tokens > 200000".into(),
        },
        RequestLogContext::default(),
        ErrorFormat::Claude,
    );
    assert_eq!(actionable.status, StatusCode::BAD_REQUEST);
}

#[test]
fn upstream_request_rejections_never_poison_an_account() {
    let error = upstream_api_error(
        KiroError {
            status: Some(503),
            endpoint: "test".into(),
            message: "tool schema payload too large".into(),
        },
        RequestLogContext::default(),
        ErrorFormat::Claude,
    );
    assert_eq!(error.status, StatusCode::BAD_GATEWAY);
    assert_eq!(error.error_code, "tool_budget_exceeded");
    assert!(!error.account_error);
}

#[test]
fn temporarily_unavailable_models_are_retryable_without_poisoning_accounts() {
    let error = upstream_api_error(
        KiroError {
            status: Some(500),
            endpoint: "AmazonQ".into(),
            message: r#"{"message":"Encountered unexpectedly high load when processing the request, please try again.","reason":"MODEL_TEMPORARILY_UNAVAILABLE"}"#.into(),
        },
        RequestLogContext::default(),
        ErrorFormat::Claude,
    );
    assert_eq!(error.status, StatusCode::BAD_GATEWAY);
    assert_eq!(error.error_code, "upstream_model_unavailable");
    assert!(error.retry_after);
    assert!(!error.account_error);
}

#[test]
fn model_resolution_failures_are_actionable_client_errors() {
    let attempts = vec![UpstreamAttemptLog {
        attempt: 1,
        account_id: "acc_1".into(),
        account_name: "enterprise@example.com".into(),
        model: "claude-fable-5".into(),
        available_models: vec!["claude-opus-5".into(), "claude-sonnet-5".into()],
        endpoint: "model-resolution".into(),
        status: None,
        error: "model is not present in this account's model cache".into(),
    }];
    let error = upstream_api_error(
        KiroError {
            status: None,
            endpoint: "model-resolution".into(),
            message: "no selected account can serve resolved model 'claude-fable-5'".into(),
        },
        RequestLogContext {
            attempts,
            ..RequestLogContext::default()
        },
        ErrorFormat::Claude,
    );

    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    assert_eq!(error.error_code, "model_not_available");
    assert_eq!(error.error_stage, "model_resolution");
    assert!(!error.retry_after);
    assert!(!error.account_error);
    assert_eq!(error.upstream_status, None);
    let response = error.with_request_id("trace_model").into_response();
    assert_eq!(
        response.headers()["x-crosslane-error-code"],
        "model_not_available"
    );
    assert_eq!(
        response.headers()["x-crosslane-error-stage"],
        "model_resolution"
    );
    assert!(response.headers().get(header::RETRY_AFTER).is_none());
}

#[test]
fn attempt_diagnostics_aggregate_accounts_models_and_reasons() {
    let attempts = vec![
        UpstreamAttemptLog {
            attempt: 1,
            account_id: "acc_b".into(),
            account_name: "b@example.com".into(),
            model: "missing".into(),
            available_models: vec!["sonnet".into(), "opus".into()],
            endpoint: "model-resolution".into(),
            status: None,
            error: "not in cache".into(),
        },
        UpstreamAttemptLog {
            attempt: 2,
            account_id: "acc_a".into(),
            account_name: "a@example.com".into(),
            model: "missing".into(),
            available_models: vec!["opus".into(), "haiku".into()],
            endpoint: "model-resolution".into(),
            status: None,
            error: "not in cache either".into(),
        },
    ];

    let diagnostics = attempt_diagnostics(&attempts);

    assert_eq!(diagnostics.account_ids, "acc_a,acc_b");
    assert_eq!(diagnostics.account_names, "a@example.com,b@example.com");
    assert_eq!(diagnostics.available_model_count, 3);
    assert_eq!(diagnostics.available_models, "haiku,opus,sonnet");
    assert!(diagnostics.errors.contains("attempt=1 account=acc_b"));
    assert!(diagnostics.errors.contains("attempt=2 account=acc_a"));
}

#[test]
fn proxy_budget_errors_remain_visible_to_claude_code() {
    let error = ApiError::new(
        StatusCode::BAD_REQUEST,
        "loaded tool definitions are too large",
        ErrorFormat::Claude,
    );
    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    let envelope = error_envelope(
        ErrorFormat::Claude,
        error.status.as_u16(),
        &error.message,
        None,
    );
    assert_eq!(envelope["error"]["type"], "invalid_request_error");
    let response = error.with_request_id("trace_test").into_response();
    assert_eq!(
        response.headers()["x-crosslane-error-code"],
        "tool_budget_exceeded"
    );
    assert_eq!(
        response.headers()["x-crosslane-error-stage"],
        "request_budget"
    );
    assert_eq!(response.headers()["request-id"], "trace_test");
}

#[test]
fn only_real_inbound_body_limits_use_request_too_large() {
    let error = ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "request body exceeds the 50 MiB limit",
        ErrorFormat::Claude,
    );
    assert_eq!(error.error_code, "request_body_too_large");
    let envelope = error_envelope(
        ErrorFormat::Claude,
        error.status.as_u16(),
        &error.message,
        None,
    );
    assert_eq!(envelope["error"]["type"], "request_too_large");
}

#[test]
fn upstream_413_preserves_diagnostics_without_triggering_claude_32mb_ui() {
    let error = upstream_api_error(
        KiroError {
            status: Some(413),
            endpoint: "test".into(),
            message: "translated upstream payload is too large".into(),
        },
        RequestLogContext::default(),
        ErrorFormat::Claude,
    );
    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    assert_eq!(error.upstream_status, Some(413));
    assert_eq!(error.error_code, "request_payload_exceeded");
}

#[test]
fn ordinary_tools_use_the_model_context_instead_of_the_tool_search_budget() {
    let context = crosslane_core::config::Config::default().context;
    let tool_tokens = 37_239;

    assert!(enforce_payload_budget_limits(
        &context,
        tool_tokens,
        512 * 1024,
        64,
        false,
        ErrorFormat::Claude,
    )
    .is_ok());

    let error = enforce_payload_budget_limits(
        &context,
        tool_tokens,
        512 * 1024,
        64,
        true,
        ErrorFormat::Claude,
    )
    .expect_err("deferred Tool Search working sets must retain their own budget");
    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    assert!(error.message.contains("Tool Search working set"));
}

#[test]
fn catalog_capacity_errors_are_invalid_requests_with_stable_codes() {
    for error in [
        ValidationError::TooManyDeferredTools,
        ValidationError::DeferredToolDefinitionsTooLarge,
    ] {
        let error = claude_validation_error(error);
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert_eq!(error.error_code, "tool_catalog_too_large");
    }

    for error in [
        ValidationError::TooManyTools,
        ValidationError::LoadedToolDefinitionsTooLarge,
        ValidationError::ToolDefinitionTooLarge,
        ValidationError::ToolDocumentationTooLarge,
    ] {
        let error = claude_validation_error(error);
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert_eq!(error.error_code, "tool_budget_exceeded");
    }
}

#[test]
fn model_path_omits_empty_and_duplicate_hops() {
    assert_eq!(
        build_model_path("client", "mapped", "kiro"),
        ["client", "mapped", "kiro"]
    );
    assert_eq!(build_model_path("same", "same", ""), ["same"]);
}

#[test]
fn credit_reservation_heuristic_is_configurable_and_capped() {
    let config = crosslane_core::config::PoolConfig {
        credit_estimate_per_1k_tokens: 2.0,
        credit_estimate_output_token_cap: 100,
        ..crosslane_core::config::PoolConfig::default()
    };
    assert_eq!(estimated_credits(900, 500, &config), 2.0);
}

#[test]
fn compaction_summary_parser_prefers_the_official_summary_block() {
    assert_eq!(
        parse_compaction_summary("preamble\n<summary>Current state\nNext step</summary>\nnoise")
            .expect("summary"),
        "Current state\nNext step"
    );
    assert!(parse_compaction_summary("<summary>unfinished")
        .expect_err("unterminated summaries must fall back")
        .contains("unterminated"));
    assert!(parse_compaction_summary("   ").is_err());
}

#[tokio::test]
async fn mapped_overflow_decision_uses_the_mapped_safe_window() {
    let directory = tempfile::tempdir().expect("tempdir");
    let paths = crosslane_core::paths::Paths::from_env_values(
        Some(directory.path().to_str().expect("utf8")),
        None,
        None,
        None,
    );
    crosslane_store::bootstrap::ensure_layout(&paths)
        .await
        .expect("layout");
    let accounts = crosslane_store::accounts::AccountStore::load(&paths.accounts_file)
        .await
        .expect("accounts");
    let state = Arc::new(AppState::new(
        paths,
        crosslane_store::config_loader::ConfigHandle::new(crosslane_core::config::Config::default()),
        accounts,
    ));
    state.models.finish_refresh(vec![crosslane_kiro::ModelInfo {
        model_id: "mapped-small".into(),
        model_name: String::new(),
        description: String::new(),
        rate_multiplier: None,
        token_limits: Some(crosslane_kiro::client::TokenLimits {
            max_input_tokens: Some(128_000),
            max_output_tokens: Some(16_384),
        }),
        additional_model_request_fields_schema: None,
    }]);

    let decision = initial_compaction_decision(&state, "mapped-small", 180_000, None, true)
        .expect("overflow decision");
    assert_eq!(decision.trigger_tokens, 121_600);
    assert_eq!(decision.target_tokens, 91_200);
    assert_eq!(decision.maximum_tokens, 126_720);
    assert_eq!(
        decision.reasons,
        vec![CompactionReason::MappedWindowOverflow]
    );

    let retry =
        upstream_overflow_compaction_decision(&state, "incorrectly-advertised-1m-model", 767_743)
            .expect("upstream overflow retry decision");
    assert_eq!(retry.maximum_tokens, 190_000);
    assert_eq!(retry.target_tokens, 142_500);
    assert_eq!(
        retry.reasons,
        vec![CompactionReason::UpstreamWindowOverflow]
    );
}

#[tokio::test]
async fn multiwave_summary_input_falls_back_without_dispatching_semantic_work() {
    let directory = tempfile::tempdir().expect("tempdir");
    let paths = crosslane_core::paths::Paths::from_env_values(
        Some(directory.path().to_str().expect("utf8")),
        None,
        None,
        None,
    );
    crosslane_store::bootstrap::ensure_layout(&paths)
        .await
        .expect("layout");
    let accounts = crosslane_store::accounts::AccountStore::load(&paths.accounts_file)
        .await
        .expect("accounts");
    let state = Arc::new(AppState::new(
        paths,
        crosslane_store::config_loader::ConfigHandle::new(crosslane_core::config::Config::default()),
        accounts,
    ));
    state.models.finish_refresh(vec![crosslane_kiro::ModelInfo {
        model_id: "mapped-tiny".into(),
        model_name: String::new(),
        description: String::new(),
        rate_multiplier: None,
        token_limits: Some(crosslane_kiro::client::TokenLimits {
            max_input_tokens: Some(10_000),
            max_output_tokens: Some(4_096),
        }),
        additional_model_request_fields_schema: None,
    }]);
    let request: ClaudeRequest = serde_json::from_value(json!({
        "model":"source-large",
        "max_tokens":64,
        "messages":[
            {"role":"user","content":"old history ".repeat(12000)},
            {"role":"assistant","content":"retain this conclusion"},
            {"role":"user","content":"continue"}
        ]
    }))
    .expect("request");
    let options = TranslationOptions::new("mapped-tiny", "AI_EDITOR");
    let source_payload = claude_to_kiro(&request, &options);
    let decision = CompactionDecision {
        reasons: vec![CompactionReason::MappedWindowOverflow],
        model: "mapped-tiny".into(),
        trigger_tokens: 9_500,
        target_tokens: 7_125,
        maximum_tokens: 9_900,
    };

    let run = match run_compaction(
        &state,
        CompactionRequest {
            trace_id: "trace_capacity",
            key_id: None,
            account_ids: Arc::new(
                state
                    .pool()
                    .snapshot()
                    .await
                    .into_iter()
                    .map(|account| account.id)
                    .collect(),
            ),
            allowed_models: &[],
            service_id: None,
            source_payload: &source_payload,
            decision: &decision,
            summary_model: "mapped-tiny",
            summary_timeout_ms: 30_000,
            preserve_recent_turns: 3,
            explicit_client_compaction: false,
        },
    )
    .await
    {
        Ok(run) => run,
        Err(error) => panic!("extractive fallback failed: {}", error.message),
    };
    assert_eq!(run.mode, "extractive_fallback");
    assert_eq!(run.fallback_reason, Some("summary_multiple_waves"));
    let source_tokens = state
        .tokenizer
        .estimate_kiro_payload(&source_payload)
        .await
        .expect("source tokens") as u64;
    let summary_tokens = run.summary_input_tokens.expect("summary tokens");
    // Diagnostics describe the full source transcript, not a lossy pre-excerpt.
    // Each planned part is checked against the model window before the proxy
    // decides that multiple dispatch waves cannot finish within one deadline.
    assert!(summary_tokens > 9_900);
    assert!(summary_tokens >= source_tokens);
    assert!(run.stats.compacted_tokens <= decision.target_tokens as usize);
    assert!(matches!(
        run.artifact,
        Some(CompactionArtifact::Extractive { .. })
    ));

    let mut minimum_payload = source_payload.clone();
    minimum_payload.retain_protected_history();
    let indivisible_tokens = state
        .tokenizer
        .estimate_kiro_payload(&minimum_payload)
        .await
        .expect("minimum payload") as u64;
    let relaxed = CompactionDecision {
        reasons: vec![CompactionReason::MappedWindowOverflow],
        model: "mapped-tiny".into(),
        trigger_tokens: indivisible_tokens + 100,
        target_tokens: indivisible_tokens.saturating_sub(1).max(1),
        maximum_tokens: indivisible_tokens + 1_000,
    };
    let relaxed_target = match compaction_operation_target(&state, &source_payload, &relaxed).await
    {
        Ok(target) => target,
        Err(error) => panic!("safe-window relaxation failed: {}", error.message),
    };
    assert_eq!(relaxed_target, relaxed.trigger_tokens);

    let client_trigger_below_current = CompactionDecision {
        reasons: vec![CompactionReason::ClientTrigger],
        model: "mapped-tiny".into(),
        trigger_tokens: indivisible_tokens.saturating_sub(1).max(1),
        target_tokens: indivisible_tokens.saturating_sub(2).max(1),
        maximum_tokens: indivisible_tokens + 1_000,
    };
    let client_relaxed_target =
        compaction_operation_target(&state, &source_payload, &client_trigger_below_current)
            .await
            .unwrap_or_else(|error| {
                panic!("client trigger became a hard limit: {}", error.message)
            });
    assert_eq!(
        client_relaxed_target,
        client_trigger_below_current.maximum_tokens
    );

    let oversized_request: ClaudeRequest = serde_json::from_value(json!({
        "model":"source-large",
        "max_tokens":64,
        "messages":[
            {"role":"user","content":"small old turn"},
            {"role":"assistant","content":"small old response"},
            {"role":"user","content":"oversized current turn ".repeat(12000)}
        ]
    }))
    .expect("request");
    let oversized_payload = claude_to_kiro(&oversized_request, &options);
    let error = match run_compaction(
        &state,
        CompactionRequest {
            trace_id: "trace_oversized_current",
            key_id: None,
            account_ids: Arc::new(
                state
                    .pool()
                    .snapshot()
                    .await
                    .into_iter()
                    .map(|account| account.id)
                    .collect(),
            ),
            allowed_models: &[],
            service_id: None,
            source_payload: &oversized_payload,
            decision: &decision,
            summary_model: "mapped-tiny",
            summary_timeout_ms: 30_000,
            preserve_recent_turns: 3,
            explicit_client_compaction: false,
        },
    )
    .await
    {
        Ok(_) => panic!("an indivisible current turn must not be summarized away"),
        Err(error) => error,
    };
    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    assert!(error.message.contains("mapped-tiny"));
    assert!(error.message.contains(" > 9900"));
    assert!(!error.message.contains(" > 7125"));
}

#[tokio::test]
async fn semantic_reapply_failure_keeps_the_context_limit_contract() {
    let directory = tempfile::tempdir().expect("tempdir");
    let paths = crosslane_core::paths::Paths::from_env_values(
        Some(directory.path().to_str().expect("utf8")),
        None,
        None,
        None,
    );
    crosslane_store::bootstrap::ensure_layout(&paths)
        .await
        .expect("layout");
    let accounts = crosslane_store::accounts::AccountStore::load(&paths.accounts_file)
        .await
        .expect("accounts");
    let state = Arc::new(AppState::new(
        paths,
        crosslane_store::config_loader::ConfigHandle::new(crosslane_core::config::Config::default()),
        accounts,
    ));
    let request: ClaudeRequest = serde_json::from_value(json!({
        "model":"source-large",
        "max_tokens":64,
        "messages":[
            {"role":"user","content":"old history ".repeat(4000)},
            {"role":"assistant","content":"old conclusion"},
            {"role":"user","content":"continue"}
        ]
    }))
    .expect("request");
    let source_payload = claude_to_kiro(
        &request,
        &TranslationOptions::new("mapped-model", "AI_EDITOR"),
    );
    let plan = state
        .tokenizer
        .plan_kiro_compaction(&source_payload, 1_000, 1)
        .await
        .expect("plan")
        .expect("history plan");
    let artifact = CompactionArtifact::Semantic {
        source_payload,
        plan,
        summary: "durable semantic checkpoint ".repeat(100),
        usage: CompactionIterationUsage {
            input_tokens: 4_000,
            output_tokens: 400,
        },
    };
    let decision = CompactionDecision {
        reasons: vec![CompactionReason::ResolvedWindowOverflow],
        model: "resolved-micro".into(),
        trigger_tokens: 60,
        target_tokens: 45,
        maximum_tokens: 60,
    };

    let error = match reapply_compaction(&state, &artifact, &decision).await {
        Ok(_) => panic!("semantic checkpoint must not fit the micro window"),
        Err(error) => error,
    };
    assert_eq!(error.status, StatusCode::BAD_REQUEST);
    assert!(error.message.contains("resolved-micro"));
    assert!(error.message.contains(" > 60"));
    assert_eq!(error.error_code, "context_length_exceeded");
}

#[test]
fn internal_rounds_receive_only_the_remaining_output_budget() {
    let request: ClaudeRequest = serde_json::from_value(json!({
        "model":"claude-sonnet-4",
        "max_tokens":100,
        "messages":[{"role":"user","content":"use a tool"}],
        "tools":[{"name":"lookup","input_schema":{"type":"object"}}]
    }))
    .expect("request");
    let mut payload = claude_to_kiro(
        &request,
        &TranslationOptions::new("dynamic-sonnet", "AI_EDITOR"),
    );
    assert_eq!(
        payload
            .inference_config
            .as_ref()
            .and_then(|value| value.max_tokens),
        Some(100)
    );
    assert!(apply_remaining_output_budget(&mut payload, Some(100), 35));
    assert_eq!(
        payload
            .inference_config
            .as_ref()
            .and_then(|value| value.max_tokens),
        Some(65)
    );
    assert!(!apply_remaining_output_budget(&mut payload, Some(100), 100));
    payload.inference_config = None;
    assert!(apply_remaining_output_budget(&mut payload, Some(100), 35));
    assert_eq!(payload.max_output_tokens(), Some(65));
    payload.inference_config = None;
    assert!(apply_remaining_output_budget(&mut payload, None, 9_000));
    assert!(payload.inference_config.is_none());
}

#[test]
fn nonstream_stop_discards_events_after_a_cross_chunk_match() {
    let mut decoded = DecodedResponse::default();
    let mut filter = StopSequenceFilter::new(&["<END>".into()]);
    let mut visible = String::new();
    let events = [
        KiroEvent::AssistantResponse {
            content: "before <E".into(),
        },
        KiroEvent::AssistantResponse {
            content: "ND>ignored".into(),
        },
        KiroEvent::Reasoning {
            content: "must not be retained".into(),
            signature: None,
            redacted_content: None,
        },
    ];
    for event in events {
        if push_nonstream_event(&mut decoded, &mut filter, &mut visible, event)
            .expect("decoded event")
        {
            break;
        }
    }

    assert_eq!(decoded.text, "before ");
    assert!(decoded.reasoning.is_empty());
    assert_eq!(decoded.stop_reason.as_deref(), Some("stop_sequence"));
    assert_eq!(decoded.stop_sequence.as_deref(), Some("<END>"));
}

#[test]
fn empty_tool_result_round_retries_once_then_fails() {
    let payload: KiroPayload = serde_json::from_value(json!({
        "conversationState": {
            "chatTriggerType": "MANUAL",
            "conversationId": "conversation",
            "history": [],
            "currentMessage": {"userInputMessage": {
                "content": "", "modelId": "model", "origin": "AI_EDITOR",
                "userInputMessageContext": {"toolResults": [{
                    "toolUseId": "call_1", "status": "success",
                    "content": [{"text": "done"}]
                }]}
            }}
        }
    }))
    .expect("payload");
    let mut decoded = DecodedResponse {
        reasoning: "reasoning alone is not a client continuation".into(),
        ..DecodedResponse::default()
    };

    assert_eq!(
        empty_tool_result_disposition(&payload, &decoded, false, 0),
        EmptyToolResultDisposition::Retry
    );
    assert_eq!(
        empty_tool_result_disposition(&payload, &decoded, false, 1),
        EmptyToolResultDisposition::Fail
    );

    decoded.text = "finished".into();
    assert_eq!(
        empty_tool_result_disposition(&payload, &decoded, false, 0),
        EmptyToolResultDisposition::Accept
    );
    decoded.text.clear();
    assert_eq!(
        empty_tool_result_disposition(&payload, &decoded, true, 0),
        EmptyToolResultDisposition::Accept
    );
}

#[test]
fn fallback_models_use_catalog_and_keep_configured_targets() {
    let mut config = crosslane_core::config::Config::default();
    config.features.default_model_id = "private-model".into();
    let models = fallback_models(&config);
    assert_eq!(
        models.first().map(|model| model.model_id.as_str()),
        crosslane_kiro::static_models()
            .first()
            .map(|model| model.model_id.as_str())
    );
    assert_eq!(
        models.last().map(|model| model.model_id.as_str()),
        Some("private-model")
    );
    assert!(models.iter().any(|model| model.model_id == "auto"));
    assert!(models
        .iter()
        .any(|model| model.model_id == "claude-sonnet-4.6"));
    assert!(models.iter().any(|model| model.model_id == "private-model"));
}
