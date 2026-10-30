use super::*;

async fn fixture() -> (TempDir, Arc<AppState>) {
    let mut first = sample_account("acc_00000001", "first@example.invalid", true);
    first.tags = vec!["team-a".into()];
    let mut second = sample_account("acc_00000002", "second@example.invalid", true);
    second.tags = first.tags.clone();
    let (directory, state) = state_with(vec![first, second]).await;
    for (name, tag) in [
        ("global", None),
        ("tagged", Some("team-a")),
        ("manual", Some("team-b")),
        ("excluded", None),
        ("unrelated", Some("team-c")),
    ] {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        expect_ok(dispatch(&state, Request::new(1, method::SERVICE_CREATE,
            serde_json::json!({"name":name,"host":"127.0.0.1","port":port,"account_tag":tag})
        )).await);
    }
    let mut config = state.config.current().as_ref().clone();
    for service in &mut config.proxy_service {
        match service.name.as_str() {
            "global" | "manual" => {
                service.account_ids = vec!["acc_00000001".into(), "kiro/acc_00000001".into()]
            }
            "excluded" => {
                service.account_ids = vec!["acc_00000001".into()];
                service.excluded_account_ids = vec!["kiro/acc_00000001".into()];
            }
            _ => {}
        }
        if service.name == "tagged" {
            service.enabled = false;
        }
    }
    config.validate().unwrap();
    tokio::fs::write(
        &state.paths.config_file,
        format!(
            "# preserved removal comment\n{}",
            toml::to_string(&config).unwrap()
        ),
    )
    .await
    .unwrap();
    state.apply_config_transaction(&config).await.unwrap();
    (directory, state)
}

#[tokio::test]
async fn force_removal_unbinds_all_pools_and_aliases_and_preserves_other_accounts() {
    for method in [method::ACCOUNT_REMOVE, method::V2_ACCOUNT_REMOVE] {
        let (_directory, state) = fixture().await;
        let before = state.config.current();
        let target = state.with_accounts(|store| store.find("acc_00000001").unwrap().clone());
        let other = state.with_accounts(|store| store.find("acc_00000002").unwrap().clone());
        let blocked = dispatch(
            &state,
            Request::new(
                2,
                method,
                serde_json::json!({"id":target.id,"provider":"kiro","force":false}),
            ),
        )
        .await;
        assert!(
            matches!(blocked, Response::Err { error, .. } if error.message.contains("bound to proxy service"))
        );
        let removed = expect_ok(
            dispatch(
                &state,
                Request::new(
                    3,
                    method,
                    serde_json::json!({"id":target.email,"provider":"kiro","force":true}),
                ),
            )
            .await,
        );
        assert_eq!(removed["removed"], target.id);
        assert!(state.with_accounts(|store| store.find(&target.id).is_none()));
        assert!(state.pool().get(&target.id).await.is_none());
        let disk = AccountStore::load(&state.paths.accounts_file)
            .await
            .unwrap();
        assert!(disk.find(&target.id).is_none());
        assert_eq!(
            serde_json::to_value(disk.find(&other.id)).unwrap(),
            serde_json::to_value(&other).unwrap()
        );
        let after = state.config.current();
        for (previous, service) in before.proxy_service.iter().zip(&after.proxy_service) {
            assert!(!service.includes_account(&target), "{}", service.name);
            assert_eq!(
                service.includes_account(&other),
                previous.includes_account(&other)
            );
            assert!(!service
                .account_ids
                .iter()
                .any(|id| id == &target.id || id == &format!("kiro/{}", target.id)));
            assert_eq!(service.enabled, previous.enabled);
            assert_eq!(service.account_tag, previous.account_tag);
            if service.name == "unrelated" {
                assert_eq!(service, previous);
            }
        }
        let raw = tokio::fs::read_to_string(&state.paths.config_file)
            .await
            .unwrap();
        assert!(raw.starts_with("# preserved removal comment"));
        let persisted: Config = toml::from_str(&raw).unwrap();
        assert_eq!(persisted.proxy_service, after.proxy_service);
        state
            .apply_config_transaction(&persisted)
            .await
            .expect("no dangling manual references");
        state.shutdown.cancel();
    }
}

#[tokio::test]
async fn force_removal_missing_account_does_not_change_config_or_accounts() {
    let (_directory, state) = fixture().await;
    let raw = tokio::fs::read(&state.paths.config_file).await.unwrap();
    let result = dispatch(
        &state,
        Request::new(
            2,
            method::ACCOUNT_REMOVE,
            serde_json::json!({"id":"missing@example.invalid","force":true}),
        ),
    )
    .await;
    assert!(
        matches!(result, Response::Err { error, .. } if error.message.contains("account not found"))
    );
    assert_eq!(
        tokio::fs::read(&state.paths.config_file).await.unwrap(),
        raw
    );
    assert_eq!(state.with_accounts(|store| store.all().len()), 2);
    state.shutdown.cancel();
}

#[tokio::test]
async fn force_removal_includes_bindings_written_before_the_watcher_publishes_them() {
    let (_directory, state) = fixture().await;
    let mut disk = state.config.current().as_ref().clone();
    let service = disk
        .proxy_service
        .iter_mut()
        .find(|service| service.name == "unrelated")
        .unwrap();
    service.account_ids = vec!["kiro/acc_00000001".into()];
    let pending = service.clone();
    tokio::fs::write(&state.paths.config_file, toml::to_string(&disk).unwrap())
        .await
        .unwrap();
    assert!(state
        .config
        .current()
        .proxy_service
        .iter()
        .find(|service| service.name == "unrelated")
        .unwrap()
        .account_ids
        .is_empty());
    expect_ok(
        dispatch(
            &state,
            Request::new(
                2,
                method::V2_ACCOUNT_REMOVE,
                serde_json::json!({"provider":"kiro","id":"acc_00000001","force":true}),
            ),
        )
        .await,
    );
    let saved: Config = toml::from_str(
        &tokio::fs::read_to_string(&state.paths.config_file)
            .await
            .unwrap(),
    )
    .unwrap();
    let service = saved
        .proxy_service
        .iter()
        .find(|service| service.name == "unrelated")
        .unwrap();
    assert!(service.account_ids.is_empty());
    assert_eq!(service.excluded_account_ids, ["acc_00000001"]);
    assert_eq!(service.account_tag, pending.account_tag);
    state
        .apply_config_transaction(&saved)
        .await
        .expect("persisted references must remain valid after deletion");
    state.shutdown.cancel();
}

#[tokio::test]
async fn force_removal_publishes_pending_unbindings_before_deleting_the_account() {
    let (_directory, state) = fixture().await;
    let target = state.with_accounts(|store| store.find("acc_00000001").unwrap().clone());
    let mut disk = state.config.current().as_ref().clone();
    for service in &mut disk.proxy_service {
        if service.includes_account(&target) {
            service.excluded_account_ids.push(target.id.clone());
        }
        service
            .account_ids
            .retain(|id| id != &target.id && id != &format!("kiro/{}", target.id));
    }
    disk.validate().unwrap();
    tokio::fs::write(&state.paths.config_file, toml::to_string(&disk).unwrap())
        .await
        .unwrap();
    assert!(!state.account_service_bindings(&target).is_empty());
    expect_ok(
        dispatch(
            &state,
            Request::new(
                2,
                method::V2_ACCOUNT_REMOVE,
                serde_json::json!({"provider":"kiro","id":target.id,"force":true}),
            ),
        )
        .await,
    );
    assert!(state.with_accounts(|store| store.find(&target.id).is_none()));
    assert_eq!(state.config.current().proxy_service, disk.proxy_service);
    state.apply_config_transaction(&disk).await.unwrap();
    state.shutdown.cancel();
}

#[tokio::test]
async fn force_removal_account_write_failure_restores_durable_and_live_bindings() {
    let (_directory, state) = fixture().await;
    let mut config = state.config.current().as_ref().clone();
    config.storage.compression_threshold = 2;
    config.storage.incremental_write = true;
    tokio::fs::write(&state.paths.config_file, toml::to_string(&config).unwrap())
        .await
        .unwrap();
    state.apply_config_transaction(&config).await.unwrap();
    // Keep the compressed base above the threshold after removing one row.
    let mut accounts = AccountStore::load(&state.paths.accounts_file)
        .await
        .unwrap();
    for index in 3..=4 {
        accounts
            .insert(sample_account(
                &format!("acc_{index:08}"),
                &format!("extra{index}@example.invalid"),
                true,
            ))
            .unwrap();
    }
    state.configure_account_store(&mut accounts);
    accounts.set_incremental_write(false);
    accounts.save().await.unwrap();
    state.apply_account_file_reload(accounts).await.unwrap();
    let blocker = std::path::PathBuf::from(format!(
        "{}.d/acc_00000001.json",
        state.paths.accounts_file.display()
    ));
    tokio::fs::create_dir_all(&blocker).await.unwrap();
    let before = tokio::fs::read(&state.paths.config_file).await.unwrap();
    let account = state.with_accounts(|store| store.find("acc_00000001").unwrap().clone());
    let result = dispatch(
        &state,
        Request::new(
            2,
            method::V2_ACCOUNT_REMOVE,
            serde_json::json!({"provider":"kiro","id":account.id,"force":true}),
        ),
    )
    .await;
    assert!(
        matches!(result, Response::Err { error, .. } if error.message.contains("remove account"))
    );
    assert!(state.with_accounts(|store| store.find(&account.id).is_some()));
    assert!(state.pool().get(&account.id).await.is_some());
    let disk = AccountStore::load(&state.paths.accounts_file)
        .await
        .unwrap();
    assert!(disk.find(&account.id).is_some());
    assert_eq!(disk.all().len(), 4);
    assert_eq!(
        tokio::fs::read(&state.paths.config_file).await.unwrap(),
        before
    );
    assert_eq!(state.config.current().proxy_service, config.proxy_service);
    assert_eq!(state.account_service_bindings(&account).len(), 3);
    state.shutdown.cancel();
}
