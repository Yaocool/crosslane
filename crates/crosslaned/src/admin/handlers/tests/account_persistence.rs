use super::*;

#[tokio::test]
async fn failed_multi_account_delta_write_restores_the_entire_previous_generation() {
    let (_directory, state) = state_with(
        (1..=12)
            .map(|index| {
                sample_account(
                    &format!("acc_{index:08}"),
                    &format!("fixture-{index}@example.invalid"),
                    true,
                )
            })
            .collect(),
    )
    .await;
    let mut config = state.config.current().as_ref().clone();
    config.storage.compression_threshold = 10;
    config.storage.incremental_write = true;
    state.apply_config_transaction(&config).await.unwrap();
    let mut initial = state.with_accounts(Clone::clone);
    state.configure_account_store(&mut initial);
    initial.set_incremental_write(false);
    initial.save().await.unwrap();
    state.apply_account_file_reload(initial).await.unwrap();

    let delta_directory =
        std::path::PathBuf::from(format!("{}.d", state.paths.accounts_file.display()));
    // The first row's delta commits before the second row's rename fails.
    tokio::fs::create_dir_all(delta_directory.join("acc_00000002.json"))
        .await
        .unwrap();
    let error = accounts::commit_account_change(&state, |store| {
        for id in ["acc_00000001", "acc_00000002"] {
            assert!(store.update(id, |account| account.label = Some("modified".into())));
        }
        Ok(())
    })
    .await
    .unwrap_err();
    assert!(error
        .message
        .contains("previous account generation restored"));
    let disk = AccountStore::load(&state.paths.accounts_file)
        .await
        .unwrap();
    assert_eq!(disk.len(), 12);
    assert!(!delta_directory.join("acc_00000001.json").exists());
    for id in ["acc_00000001", "acc_00000002"] {
        assert!(disk.find(id).unwrap().label.is_none());
        assert!(state.with_accounts(|store| store.find(id).unwrap().label.is_none()));
        assert!(state
            .pool()
            .get(id)
            .await
            .unwrap()
            .account
            .read()
            .await
            .label
            .is_none());
    }
    state.shutdown.cancel();
}

#[cfg(unix)]
#[tokio::test]
async fn committed_account_addition_is_published_when_stale_delta_cleanup_fails() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let (directory, state) = state_with(vec![sample_account(
        "acc_00000001",
        "fixture-before@example.invalid",
        true,
    )])
    .await;
    if std::fs::metadata(directory.path()).unwrap().uid() == 0 {
        // Root bypasses directory permissions, so this fault is not reproducible.
        state.shutdown.cancel();
        return;
    }
    let original = state.with_accounts(|store| store.all()[0].clone());
    let delta_directory =
        std::path::PathBuf::from(format!("{}.d", state.paths.accounts_file.display()));
    tokio::fs::create_dir_all(&delta_directory).await.unwrap();
    let delta = delta_directory.join("acc_00000001.json");
    tokio::fs::write(
        &delta,
        serde_json::to_vec(&serde_json::json!({"id":original.id,"account":original})).unwrap(),
    )
    .await
    .unwrap();
    std::fs::set_permissions(&delta_directory, std::fs::Permissions::from_mode(0o555)).unwrap();

    let result = accounts::commit_account_change(&state, |store| {
        store
            .insert(sample_account(
                "acc_00000002",
                "fixture-after@example.invalid",
                true,
            ))
            .map_err(|error| RpcError::internal(error.to_string()))?;
        Ok("acc_00000002")
    })
    .await;
    // Restore permissions before any assertion to keep the temporary directory removable.
    std::fs::set_permissions(&delta_directory, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(result.unwrap(), "acc_00000002");
    assert!(
        delta.exists(),
        "the cleanup failure must actually be exercised"
    );
    let disk = AccountStore::load(&state.paths.accounts_file)
        .await
        .unwrap();
    assert_eq!(disk.len(), 2);
    for id in ["acc_00000001", "acc_00000002"] {
        assert!(disk.find(id).is_some());
        assert!(state.with_accounts(|store| store.find(id).is_some()));
        assert!(state.pool().get(id).await.is_some());
    }
    state.shutdown.cancel();
}
