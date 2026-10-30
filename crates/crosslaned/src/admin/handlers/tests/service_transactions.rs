use super::*;

fn port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn create(state: &Arc<AppState>, name: &str) -> serde_json::Value {
    expect_ok(dispatch(state, Request::new(1, method::SERVICE_CREATE,
        serde_json::json!({"name":name,"host":"127.0.0.1","port":port(),"account_tag":"manual"})
    )).await)
}

#[tokio::test]
async fn service_mutations_preserve_pending_client_key_rotations() {
    for operation in ["create", "delete", "bind", "unbind"] {
        let (_directory, state) = state_with(vec![sample_account(
            "acc_00000001",
            "fixture@example.invalid",
            true,
        )])
        .await;
        let target = create(&state, "target").await;
        let retained = create(&state, "retained").await;
        if operation == "unbind" {
            expect_ok(
                dispatch(
                    &state,
                    Request::new(
                        2,
                        method::SERVICE_ACCOUNT_ADD,
                        serde_json::json!({"service":"target","accounts":["acc_00000001"]}),
                    ),
                )
                .await,
            );
        }
        let old_key = retained["api_key"]["key"].as_str().unwrap();
        let new_key = "sk-fixture-pending-client-key-rotation";
        let mut disk = state.config.current().as_ref().clone();
        let key_id = retained["api_key"]["id"].as_str().unwrap();
        disk.api_key
            .iter_mut()
            .find(|key| key.id.as_deref() == Some(key_id))
            .unwrap()
            .key = new_key.into();
        tokio::fs::write(
            &state.paths.config_file,
            format!(
                "# pending key rotation\n{}",
                toml::to_string(&disk).unwrap()
            ),
        )
        .await
        .unwrap();
        assert!(state.meter.authenticate(Some(old_key)).is_ok());
        let (method, params) = match operation {
            "create" => (
                method::SERVICE_CREATE,
                serde_json::json!({"name":"third","host":"127.0.0.1","port":port()}),
            ),
            "delete" => (
                method::SERVICE_DELETE,
                serde_json::json!({"service":target["service"]["id"]}),
            ),
            "bind" => (
                method::SERVICE_ACCOUNT_ADD,
                serde_json::json!({"service":"target","accounts":["acc_00000001"]}),
            ),
            _ => (
                method::SERVICE_ACCOUNT_REMOVE,
                serde_json::json!({"service":"target","accounts":["acc_00000001"]}),
            ),
        };
        expect_ok(dispatch(&state, Request::new(3, method, params)).await);
        let raw = tokio::fs::read_to_string(&state.paths.config_file)
            .await
            .unwrap();
        assert!(raw.starts_with("# pending key rotation"));
        let saved: Config = toml::from_str(&raw).unwrap();
        assert_eq!(
            saved
                .api_key
                .iter()
                .find(|key| key.id.as_deref() == Some(key_id))
                .unwrap()
                .key,
            new_key,
            "{operation}"
        );
        assert!(
            state.meter.authenticate(Some(old_key)).is_err(),
            "{operation} revived a revoked key"
        );
        assert!(
            state.meter.authenticate(Some(new_key)).is_ok(),
            "{operation}"
        );
        assert_eq!(
            state
                .runtime_config_snapshot()
                .api_key
                .iter()
                .find(|key| key.id.as_deref() == Some(key_id))
                .unwrap()
                .key,
            new_key
        );
        state.shutdown.cancel();
    }
}

#[tokio::test]
async fn service_writes_validate_the_persisted_restart_configuration() {
    let (_directory, state) = state_with(vec![]).await;
    create(&state, "target").await;
    let mut disk = state.config.current().as_ref().clone();
    let reserved = port();
    disk.status_http.enabled = true;
    disk.status_http.port = reserved;
    disk.validate().unwrap();
    let raw = toml::to_string(&disk).unwrap();
    tokio::fs::write(&state.paths.config_file, &raw)
        .await
        .unwrap();
    let (mut effective, _) = merge_hot_reload(&state.config.current(), disk);
    assert!(!effective.status_http.enabled);
    effective.proxy_service[0].port = reserved;
    effective.validate().unwrap();
    let error = persist_service_config_locked(&state, &raw, &effective)
        .await
        .unwrap_err();
    assert!(error.message.contains("reserved"));
    assert_eq!(
        tokio::fs::read_to_string(&state.paths.config_file)
            .await
            .unwrap(),
        raw
    );
    assert_ne!(state.config.current().proxy_service[0].port, reserved);
    state.shutdown.cancel();
}

#[test]
fn automatic_service_ports_reserve_both_running_and_pending_status_ports() {
    let mut config = Config::default();
    config.status_http.enabled = true;
    config.status_http.port = config.server.port + 1;
    assert_eq!(
        allocate_service_port(&config, None, Some(config.server.port)).unwrap(),
        config.server.port + 2
    );
    assert_eq!(
        allocate_service_port(
            &config,
            Some(config.server.port + 2),
            Some(config.server.port + 2)
        )
        .unwrap(),
        config.server.port + 3
    );
}


#[tokio::test]
async fn copilot_local_mutations_use_the_generation_published_while_waiting_for_config_lock() {
    for method in [
        method::V2_ACCOUNT_SET_ENABLED,
        method::V2_ACCOUNT_REMOVE,
        method::V2_ACCOUNT_TAG,
        method::V2_ACCOUNT_RESET_HEALTH,
    ] {
        let (_directory, state) = state_with_copilot().await;
        let provider_id = crosslane_core::provider::ProviderId::parse("copilot").unwrap();
        let file = state.paths.data_dir.join("providers/copilot/accounts.json");
        let mut accounts: serde_json::Value =
            serde_json::from_slice(&tokio::fs::read(&file).await.unwrap()).unwrap();
        accounts[0]["last_error"] = serde_json::json!("fixture-before-reset");
        tokio::fs::write(&file, accounts.to_string()).await.unwrap();
        let mut config = state.config.current().as_ref().clone();
        config
            .provider
            .iter_mut()
            .find(|provider| provider.id == "copilot")
            .unwrap()
            .pool
            .max_concurrent_per_account += 1;
        state.apply_config_transaction(&config).await.unwrap();
        let old = state.providers.copilot(&provider_id).await.unwrap();
        let lock = state.lock_config_mutation().await;
        let mut request = Box::pin(dispatch(
            &state,
            Request::new(
                1,
                method,
                serde_json::json!({"provider":"copilot","id":"acc_b85b28b7","enabled":false,"add":["next"]}),
            ),
        ));
        assert!(
            futures::poll!(&mut request).is_pending(),
            "{method} bypassed the configuration transaction"
        );
        let mut readable = Box::pin(old.account("acc_b85b28b7"));
        assert!(
            futures::poll!(&mut readable).is_ready(),
            "{method} locked an old account generation before the configuration transaction"
        );
        config
            .provider
            .iter_mut()
            .find(|provider| provider.id == "copilot")
            .unwrap()
            .pool
            .max_concurrent_per_account += 1;
        state
            .apply_config_transaction_locked(&config)
            .await
            .unwrap();
        let current = state.providers.copilot(&provider_id).await.unwrap();
        assert!(!Arc::ptr_eq(&old, &current));
        drop(lock);
        expect_ok(request.await);
        let account = current.account("acc_b85b28b7").await;
        match method {
            method::V2_ACCOUNT_REMOVE => assert!(account.is_none()),
            method::V2_ACCOUNT_SET_ENABLED => assert!(!account.unwrap().enabled),
            method::V2_ACCOUNT_TAG => assert_eq!(account.unwrap().tags, ["next"]),
            _ => assert!(account.unwrap().last_error.is_none()),
        }
        state.shutdown.cancel();
    }
}

#[tokio::test]
async fn copilot_import_rejects_a_replaced_runtime_after_upstream_validation_and_can_retry() {
    use wiremock::{matchers, Mock, MockServer, ResponseTemplate};

    let upstream = MockServer::start().await;
    Mock::given(matchers::method("GET"))
        .and(matchers::path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id":99,"login":"staged-account","email":"fixture-staged@example.invalid"
        })))
        .mount(&upstream)
        .await;
    Mock::given(matchers::method("GET"))
        .and(matchers::path("/copilot_internal/v2/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token":"fixture-staged-api-token","expires_at":4_000_000_000u64,
            "endpoints":{"api":upstream.uri()}
        })))
        .mount(&upstream)
        .await;
    let models_seen = Arc::new(tokio::sync::Notify::new());
    let notification = Arc::clone(&models_seen);
    Mock::given(matchers::method("GET"))
        .and(matchers::path("/models"))
        .respond_with(move |_: &wiremock::Request| {
            notification.notify_one();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data":[{"id":"gpt-test","capabilities":{"supported_endpoints":["/chat/completions"]}}]
            }))
        })
        .expect(2)
        .mount(&upstream)
        .await;

    let (_directory, state) = state_with_copilot().await;
    let mut config = state.config.current().as_ref().clone();
    let provider = config
        .provider
        .iter_mut()
        .find(|provider| provider.id == "copilot")
        .unwrap();
    for key in ["github_api_base", "oauth_base"] {
        provider
            .settings
            .insert(key.into(), serde_json::json!(upstream.uri()));
    }
    provider
        .settings
        .insert("allow_insecure_http".into(), serde_json::json!(true));
    state.apply_config_transaction(&config).await.unwrap();
    let provider_id = crosslane_core::provider::ProviderId::parse("copilot").unwrap();
    let old = state.providers.copilot(&provider_id).await.unwrap();
    let file = state.paths.data_dir.join("providers/copilot/accounts.json");
    let before = tokio::fs::read(&file).await.unwrap();
    let params = serde_json::json!({"provider":"copilot","token":"fixture-staged-github-token","tags":["team-a"]});
    let lock = state.lock_config_mutation().await;
    let mut request = Box::pin(dispatch(
        &state,
        Request::new(1, method::V2_ACCOUNT_IMPORT_TOKEN, params.clone()),
    ));
    tokio::select! {
        response = &mut request => panic!("import bypassed configuration lock: {}", matches!(response, Response::Ok { .. })),
        _ = models_seen.notified() => {},
        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => panic!("upstream validation held the config lock"),
    }
    config
        .provider
        .iter_mut()
        .find(|provider| provider.id == "copilot")
        .unwrap()
        .pool
        .max_concurrent_per_account += 1;
    state
        .apply_config_transaction_locked(&config)
        .await
        .unwrap();
    let current = state.providers.copilot(&provider_id).await.unwrap();
    assert!(!Arc::ptr_eq(&old, &current));
    drop(lock);
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), request)
        .await
        .unwrap();
    assert!(matches!(response, Response::Err { error, .. }
        if error.code == 409 && error.message.contains("configuration changed during account validation")));
    assert_eq!(tokio::fs::read(&file).await.unwrap(), before);
    assert_eq!(old.account_summaries().await.len(), 1);
    assert_eq!(current.account_summaries().await.len(), 1);

    let result = expect_ok(
        dispatch(
            &state,
            Request::new(2, method::V2_ACCOUNT_IMPORT_TOKEN, params),
        )
        .await,
    );
    let id = result["id"].as_str().unwrap();
    assert_eq!(current.account(id).await.unwrap().tags, ["team-a"]);
    let disk: serde_json::Value =
        serde_json::from_slice(&tokio::fs::read(&file).await.unwrap()).unwrap();
    assert_eq!(disk["accounts"].as_array().unwrap().len(), 2);
    assert_eq!(current.account_summaries().await.len(), 2);
    assert_eq!(old.account_summaries().await.len(), 1);
    state.shutdown.cancel();
}

#[tokio::test]
async fn rest_account_control_and_binding_operations_do_not_contact_upstreams() {
    use axum::{body::Body, http::Request as HttpRequest};
    use tower::ServiceExt;

    let upstream = wiremock::MockServer::start().await;
    let (_directory, state) = state_with_copilot().await;
    let token = "fixture-local-management-token";
    let mut config = state.config.current().as_ref().clone();
    config.status_http.enabled = true;
    config.status_http.port = port();
    config.admin.rest.enabled = true;
    config.admin.rest.token_sha256 = format!("{:x}", Sha256::digest(token.as_bytes()));
    let provider = config
        .provider
        .iter_mut()
        .find(|provider| provider.id == "copilot")
        .unwrap();
    provider
        .settings
        .insert("github_api_base".into(), serde_json::json!(upstream.uri()));
    provider
        .settings
        .insert("oauth_base".into(), serde_json::json!(upstream.uri()));
    provider
        .settings
        .insert("allow_insecure_http".into(), serde_json::json!(true));
    tokio::fs::write(&state.paths.config_file, toml::to_string(&config).unwrap())
        .await
        .unwrap();
    state.apply_config_transaction(&config).await.unwrap();
    let app = crate::status_http::router(Arc::clone(&state));
    let created = expect_ok(
        dispatch(
            &state,
            Request::new(
                1,
                method::SERVICE_CREATE,
                serde_json::json!({"name":"local","host":"127.0.0.1","port":port(),
            "allowed_providers":["copilot"],"default_provider":"copilot"}),
            ),
        )
        .await,
    );
    let service_id = created["service"]["id"].as_str().unwrap();
    for (method, path, body) in [
        (
            "GET",
            "/admin/v1/accounts/copilot/acc_b85b28b7/services".into(),
            None,
        ),
        (
            "POST",
            "/admin/v1/accounts/copilot/acc_b85b28b7/enabled".into(),
            Some(serde_json::json!({"enabled":false})),
        ),
        (
            "POST",
            "/admin/v1/accounts/copilot/acc_b85b28b7/enabled".into(),
            Some(serde_json::json!({"enabled":true})),
        ),
        (
            "POST",
            "/admin/v1/accounts/copilot/acc_b85b28b7/tags".into(),
            Some(serde_json::json!({"add":["local"]})),
        ),
        (
            "POST",
            "/admin/v1/accounts/copilot/acc_b85b28b7/reset-health".into(),
            None,
        ),
        (
            "POST",
            format!("/admin/v1/services/{service_id}/accounts"),
            Some(serde_json::json!({"accounts":["copilot/acc_b85b28b7"]})),
        ),
        (
            "DELETE",
            format!("/admin/v1/services/{service_id}/accounts"),
            Some(serde_json::json!({"accounts":["copilot/acc_b85b28b7"]})),
        ),
        (
            "DELETE",
            "/admin/v1/accounts/copilot/acc_b85b28b7?force=true".into(),
            None,
        ),
    ] {
        let request = HttpRequest::builder()
            .method(method)
            .uri(&path)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(
                body.map(|body| body.to_string()).unwrap_or_default(),
            ))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status().as_u16(), 200, "{method} {path}");
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "{method} {path} contacted the upstream"
        );
    }
    state.shutdown.cancel();
}
