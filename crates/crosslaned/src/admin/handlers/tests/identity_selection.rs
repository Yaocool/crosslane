use super::*;

async fn fixture() -> (TempDir, Arc<AppState>, String, String) {
    let (directory, state) = state_with(vec![sample_account(
        "acc_00000001",
        "fixture@example.invalid",
        true,
    )])
    .await;
    let id = format!("svc_{}", uuid::Uuid::new_v4().simple());
    let other_id = format!("svc_{}", uuid::Uuid::new_v4().simple());
    let mut config = state.config.current().as_ref().clone();
    config.api_key = vec![ApiKeyConfig {
        id: Some("ak_fixture".into()),
        name: "fixture".into(),
        key: "fixture-identity-key".into(),
        ..ApiKeyConfig::default()
    }];
    // Identity and binding checks also apply to disabled services. Keeping
    // these fixtures disabled avoids unrelated TCP-port races in parallel tests.
    config.proxy_service = [
        (other_id.as_str(), id.as_str(), 6201),
        (id.as_str(), "zz-target", 6202),
    ]
    .map(|(id, name, port)| ProxyServiceConfig {
        id: id.into(),
        name: name.into(),
        host: "127.0.0.1".into(),
        port,
        enabled: false,
        api_key_ids: vec!["ak_fixture".into()],
        account_tag: Some("manual".into()),
        ..ProxyServiceConfig::default()
    })
    .to_vec();
    config.validate().unwrap();
    tokio::fs::write(&state.paths.config_file, toml::to_string(&config).unwrap())
        .await
        .unwrap();
    state.apply_config_transaction(&config).await.unwrap();
    (directory, state, id, other_id)
}

#[tokio::test]
async fn management_id_collision_never_redirects_service_operations_to_a_named_neighbor() {
    for operation in ["keys", "pool", "bind", "unbind", "enabled", "delete"] {
        let (_directory, state, id, other_id) = fixture().await;
        for (selector, expected) in [
            ("zz-target", id.as_str()),
            (other_id.as_str(), other_id.as_str()),
        ] {
            let result = expect_ok(
                dispatch(
                    &state,
                    Request::new(
                        3,
                        method::SERVICE_APIKEYS,
                        serde_json::json!({"service":selector}),
                    ),
                )
                .await,
            );
            assert_eq!(result["service_id"], expected);
        }
        let result = match operation {
            "enabled" => rest::set_service_enabled(&state, &id, false).await.unwrap(),
            _ => {
                let name = match operation {
                    "keys" => method::SERVICE_APIKEYS,
                    "pool" => method::SERVICE_ACCOUNTS,
                    "bind" => method::SERVICE_ACCOUNT_ADD,
                    "unbind" => method::SERVICE_ACCOUNT_REMOVE,
                    _ => method::SERVICE_DELETE,
                };
                expect_ok(
                    dispatch(
                        &state,
                        Request::new(
                            3,
                            name,
                            serde_json::json!({"service":id,"accounts":["acc_00000001"]}),
                        ),
                    )
                    .await,
                )
            }
        };
        let selected = if operation == "enabled" {
            &result["id"]
        } else {
            &result["service_id"]
        };
        assert_eq!(
            selected.as_str(),
            Some(id.as_str()),
            "{operation} selected a name instead of the exact ID"
        );
        let current = state.config.current();
        let other = current
            .proxy_service
            .iter()
            .find(|service| service.id == other_id)
            .unwrap();
        assert!(!other.enabled);
        assert!(other.account_ids.is_empty() && other.excluded_account_ids.is_empty());
        if operation == "delete" {
            assert!(current.proxy_service.iter().all(|service| service.id != id));
            assert_eq!(current.proxy_service.len(), 1);
        }
        state.shutdown.cancel();
    }
}

#[tokio::test]
async fn management_id_collision_never_redirects_rest_service_lookup_or_delete() {
    use axum::{body::Body, http::Request as HttpRequest};
    use tower::ServiceExt;

    for operation in ["GET", "DELETE"] {
        let (_directory, state, id, other_id) = fixture().await;
        let token = "fixture-identity-selection-token";
        let mut config = state.config.current().as_ref().clone();
        config.status_http.enabled = true;
        config.admin.rest.enabled = true;
        config.admin.rest.token_sha256 = format!("{:x}", Sha256::digest(token.as_bytes()));
        tokio::fs::write(&state.paths.config_file, toml::to_string(&config).unwrap())
            .await
            .unwrap();
        state.apply_config_transaction(&config).await.unwrap();
        let response = crate::status_http::router(Arc::clone(&state))
            .oneshot(
                HttpRequest::builder()
                    .method(operation)
                    .uri(format!("/admin/v1/services/{id}"))
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let result: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let selected = if operation == "GET" {
            &result["data"]["service"]["id"]
        } else {
            &result["data"]["service_id"]
        };
        assert_eq!(
            selected.as_str(),
            Some(id.as_str()),
            "{operation} selected a name instead of the exact ID"
        );
        assert!(state
            .config
            .current()
            .proxy_service
            .iter()
            .any(|service| service.id == other_id));
        state.shutdown.cancel();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn management_alias_does_not_retarget_a_neighbor_after_the_original_service_disappears() {
    use axum::{body::Body, http::Request as HttpRequest};
    use tower::ServiceExt;

    for (method, suffix) in [
        ("DELETE", ""),
        ("POST", "/accounts"),
        ("DELETE", "/accounts"),
    ] {
        let (_directory, state, id, other_id) = fixture().await;
        let token = "fixture-service-selector-race";
        let mut config = state.config.current().as_ref().clone();
        config.status_http.enabled = true;
        config.admin.rest.enabled = true;
        config.admin.rest.token_sha256 = format!("{:x}", Sha256::digest(token.as_bytes()));
        tokio::fs::write(&state.paths.config_file, toml::to_string(&config).unwrap())
            .await
            .unwrap();
        state.apply_config_transaction(&config).await.unwrap();
        let mutation = state.lock_config_mutation().await;
        // Fixture setup has no watcher or active file writer. Recreating this
        // private lock file is a barrier proving that the HTTP request reached
        // its write transaction and is waiting for the configuration mutex.
        let lock_path =
            std::path::PathBuf::from(format!("{}.lock", state.paths.config_file.display()));
        if lock_path.exists() {
            tokio::fs::remove_file(&lock_path).await.unwrap();
        }
        let mut request = Box::pin(
            crate::status_http::router(Arc::clone(&state)).oneshot(
                HttpRequest::builder()
                    .method(method)
                    .uri(format!("/admin/v1/services/zz-target{suffix}"))
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"accounts":["acc_00000001"]}).to_string(),
                    ))
                    .unwrap(),
            ),
        );
        tokio::select! {
            _ = &mut request => panic!("service write bypassed the held transaction lock"),
            _ = async { while !lock_path.exists() { tokio::task::yield_now().await; } } => {},
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => panic!("service write did not reach the file transaction"),
        }
        // Model an external config edit while the request waits. The neighbor
        // retains the old ID as its name, but never had the requested alias.
        config.proxy_service.retain(|service| service.id != id);
        tokio::fs::write(&state.paths.config_file, toml::to_string(&config).unwrap())
            .await
            .unwrap();
        state
            .apply_config_transaction_locked(&config)
            .await
            .unwrap();
        drop(mutation);
        let response = tokio::time::timeout(std::time::Duration::from_secs(5), request)
            .await
            .unwrap()
            .unwrap();
        state.shutdown.cancel();
        assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND,
            "an alias must not be converted to an old ID and then reinterpreted as another service's name");
        let current = state.config.current();
        let other = current
            .proxy_service
            .iter()
            .find(|service| service.id == other_id)
            .unwrap();
        assert!(other.account_ids.is_empty() && other.excluded_account_ids.is_empty());
    }
}
