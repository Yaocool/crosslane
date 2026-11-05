use super::*;

#[tokio::test]
async fn admin_token_changes_preserve_applied_settings_and_latest_auth_on_rejection() {
    let mut account = sample_account("acc_00000001", "snapshot@example.invalid", true);
    account.tags = vec!["team-a".into()];
    let (_directory, state) = state_with(vec![account]).await;
    create_service(&state, "team", Some("team-a")).await;
    expect_ok(
        dispatch(
            &state,
            Request::new(
                1,
                method::ADMIN_TOKEN_CREATE,
                serde_json::json!({"name":"ops"}),
            ),
        )
        .await,
    );
    let path = &state.paths.config_file;
    let applied = format!(
        "# keep the confirmed configuration comment\n{}",
        tokio::fs::read_to_string(path).await.unwrap()
    );
    tokio::fs::write(path, &applied).await.unwrap();
    state.remember_applied_config_file();

    let mut pending = state.config.current().as_ref().clone();
    pending.proxy_service[0].account_tag = None;
    pending.log.level = "debug".into();
    tokio::fs::write(path, toml::to_string(&pending).unwrap())
        .await
        .unwrap();
    for (method, params) in [
        (
            method::ADMIN_TOKEN_CREATE,
            serde_json::json!({"name":"backup"}),
        ),
        (method::ADMIN_TOKEN_ROTATE, serde_json::json!({"id":"ops"})),
        (
            method::ADMIN_TOKEN_SET_ENABLED,
            serde_json::json!({"id":"backup","enabled":false}),
        ),
        (method::ADMIN_TOKEN_REVOKE, serde_json::json!({"id":"ops"})),
    ] {
        expect_ok(dispatch(&state, Request::new(2, method, params)).await);
        let snapshot: Config = toml::from_str(&state.applied_config_text().unwrap()).unwrap();
        let current = state.config.current();
        assert_eq!(snapshot.proxy_service, current.proxy_service, "{method}");
        assert_eq!(snapshot.log.level, current.log.level, "{method}");
        assert_eq!(
            serde_json::to_value(&snapshot.admin.rest).unwrap(),
            serde_json::to_value(&current.admin.rest).unwrap(),
            "{method}"
        );
        let disk: Config = toml::from_str(&tokio::fs::read_to_string(path).await.unwrap()).unwrap();
        assert!(disk.proxy_service[0].account_tag.is_none(), "{method}");
        assert_eq!(disk.log.level, "debug", "{method}");
    }

    let current = state.config.current();
    let rejected = tokio::fs::read_to_string(path).await.unwrap();
    let pending: Config = toml::from_str(&rejected).unwrap();
    let error = reload_pool_guard(&state, &current, &pending)
        .await
        .expect_err("removing the tag empties the enabled service");
    crate::config_rejection::restore_rejected(&state, &error).await;
    let restored_text = tokio::fs::read_to_string(path).await.unwrap();
    assert!(restored_text.starts_with("# keep the confirmed configuration comment\n"));
    let restored: Config = toml::from_str(&restored_text).unwrap();
    assert_eq!(restored.proxy_service, current.proxy_service);
    assert_eq!(restored.log.level, current.log.level);
    assert_eq!(
        serde_json::to_value(&restored.admin.rest).unwrap(),
        serde_json::to_value(&current.admin.rest).unwrap()
    );
    assert_eq!(restored.admin.rest.tokens.len(), 1);
    assert_eq!(restored.admin.rest.tokens[0].name, "backup");
    assert!(!restored.admin.rest.tokens[0].enabled);
    assert!(state.config_drift().is_none());
    state.shutdown.cancel();
}

#[tokio::test]
async fn admin_token_changes_keep_unknown_snapshot_drift_until_a_full_reload() {
    let mut account = sample_account("acc_00000001", "snapshot@example.invalid", true);
    account.tags = vec!["team-a".into()];
    let (_directory, state) = state_with(vec![account]).await;
    create_service(&state, "team", Some("team-a")).await;
    let path = &state.paths.config_file;
    let hidden = path.with_extension("hidden");
    tokio::fs::rename(path, &hidden).await.unwrap();
    state.remember_applied_config_file();
    tokio::fs::rename(&hidden, path).await.unwrap();

    let current = state.config.current();
    let mut pending = current.as_ref().clone();
    pending.proxy_service[0].account_tag = None;
    tokio::fs::write(path, toml::to_string(&pending).unwrap())
        .await
        .unwrap();
    let error = reload_pool_guard(&state, &current, &pending)
        .await
        .expect_err("must reject the tag edit");
    crate::config_rejection::restore_rejected(&state, &error).await;
    let drift = state.config_drift().expect("unknown snapshot reported");
    assert_eq!(drift.kind, "applied_text_unknown");
    expect_ok(
        dispatch(
            &state,
            Request::new(
                1,
                method::ADMIN_TOKEN_CREATE,
                serde_json::json!({"name":"ops"}),
            ),
        )
        .await,
    );
    assert!(state.applied_config_text().is_none());
    let status = expect_ok(
        dispatch(
            &state,
            Request::new(2, method::STATUS, serde_json::json!({})),
        )
        .await,
    );
    assert_eq!(
        status["config_drift"],
        serde_json::to_value(&drift).unwrap()
    );
    assert_eq!(
        state.config.current().proxy_service[0].selected_account_tags(),
        ["team-a"]
    );

    let reloaded = expect_ok(
        dispatch(
            &state,
            Request::new(3, method::CONFIG_RELOAD, serde_json::json!({"force":true})),
        )
        .await,
    );
    assert_eq!(reloaded["applied"], true);
    assert!(state.config_drift().is_none());
    assert!(state.applied_config_text().is_some());
    assert_eq!(state.config.current().admin.rest.tokens[0].name, "ops");
    state.shutdown.cancel();
}
