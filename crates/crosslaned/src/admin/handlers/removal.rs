//! Forced removal uses the same durable exclusions as service unbinding.

use super::*;

pub(super) async fn force_remove(state: &Arc<AppState>, provider: &str, selector: &str) -> Handled {
    // Match the service-binding lock order and keep watchers and concurrent
    // bindings out until both the config and account generation are durable.
    let _file = crosslane_store::atomic::lock_file_exclusive(&state.paths.config_file)
        .await
        .map_err(|error| RpcError::internal(format!("lock config: {error}")))?;
    let _mutation = state.lock_config_mutation().await;
    let _accounts = state
        .lock_account_storage()
        .await
        .map_err(|error| RpcError::internal(format!("lock accounts: {error}")))?;
    let mut durable = AccountStore::load(&state.paths.accounts_file)
        .await
        .map_err(|error| RpcError::internal(format!("load accounts: {error}")))?;
    state.configure_account_store(&mut durable);
    let disk = serde_json::to_value(durable.all())
        .map_err(|error| RpcError::internal(error.to_string()))?;
    let memory = state
        .with_accounts(|store| serde_json::to_value(store.all()))
        .map_err(|error| RpcError::internal(error.to_string()))?;
    if disk != memory {
        state
            .apply_account_file_reload(durable.clone())
            .await
            .map_err(RpcError::bad_params)?;
    }

    let runtime = if provider == "kiro" {
        None
    } else {
        Some(providers::copilot_runtime(state, provider).await?)
    };
    let (id, tags) = if let Some(runtime) = &runtime {
        let account = runtime.account(selector).await.ok_or_else(|| {
            RpcError::bad_params(format!("account not found: {provider}/{selector}"))
        })?;
        (account.id, account.tags)
    } else {
        let account = durable
            .find(selector)
            .ok_or_else(|| RpcError::bad_params(format!("account not found: {selector}")))?;
        (account.id.clone(), account.tags.clone())
    };

    let previous = state.config.current().as_ref().clone();
    // A cooperating file writer may have added bindings that the config
    // watcher has not published yet. Never delete against a stale snapshot.
    let raw = tokio::fs::read_to_string(&state.paths.config_file)
        .await
        .map_err(|error| RpcError::internal(format!("read config: {error}")))?;
    let disk = load_config(&state.paths.config_file)
        .await
        .map_err(|_| RpcError::bad_params("could not load current service configuration"))?;
    let (mut next, _) = merge_hot_reload(&previous, disk);
    let changed_services = unbind(&mut next, provider, &id, &tags);
    // A pending edit may already exclude this account on disk while the live
    // services still include it. Publish that generation before deletion too.
    let mut config_changed = changed_services > 0 || next.proxy_service != previous.proxy_service;
    if config_changed {
        next.validate()
            .map_err(|error| RpcError::bad_params(error.to_string()))?;
        persist_service_config_locked(state, &raw, &next).await?;
    }

    let result = if runtime.is_some() {
        async {
            // Applying pending provider settings can replace its runtime. Delete
            // from the registered generation, not the handle captured above.
            let runtime = providers::copilot_runtime(state, provider).await?;
            let account = runtime.account(&id).await.ok_or_else(|| {
                RpcError::bad_params(format!("account not found: {provider}/{id}"))
            })?;
            let mut current = state.config.current().as_ref().clone();
            if unbind(&mut current, provider, &id, &account.tags) > 0 {
                current
                    .validate()
                    .map_err(|error| RpcError::bad_params(error.to_string()))?;
                persist_service_config_locked(state, &raw, &current).await?;
                config_changed = true;
            }
            runtime
                .remove_account(&id)
                .await
                .map_err(providers::provider_rpc_error)
                .and_then(to_value)
        }
        .await
    } else {
        remove_kiro(state, durable, &id).await
    };
    if let Err(error) = result {
        if config_changed {
            // Deletion did not commit: restore both the file and live pools.
            let disk = crosslane_store::atomic::write_bytes_atomically(
                &state.paths.config_file,
                raw.as_bytes(),
                Some(0o600),
            )
            .await;
            let runtime = state.apply_config_transaction_locked(&previous).await;
            if disk.is_err() || runtime.is_err() {
                return Err(RpcError::internal(format!(
                    "{}; restoring service bindings failed: disk={:?}, runtime={:?}",
                    error.message,
                    disk.err(),
                    runtime.err()
                )));
            }
            state.mark_config_reloaded(now_secs());
        }
        return Err(error);
    }
    tracing::info!(
        event = "account.force_removed",
        provider_id = provider,
        account_id = id,
        changed_services,
        "account unbound and removed"
    );
    result
}

fn unbind(config: &mut Config, provider: &str, id: &str, tags: &[String]) -> usize {
    let qualified = format!("{provider}/{id}");
    let matches =
        |reference: &String| reference == &qualified || (provider == "kiro" && reference == id);
    let canonical = if provider == "kiro" { id } else { &qualified };
    let mut changed = 0;
    for service in &mut config.proxy_service {
        let bound = service.includes_provider_account(provider, id, tags);
        let manual = service.account_ids.iter().any(matches);
        if !bound && !manual {
            continue;
        }
        service.account_ids.retain(|reference| !matches(reference));
        if bound {
            service.excluded_account_ids.push(canonical.to_owned());
            service.excluded_account_ids.sort();
            service.excluded_account_ids.dedup();
        }
        changed += 1;
    }
    changed
}

async fn remove_kiro(state: &Arc<AppState>, mut store: AccountStore, id: &str) -> Handled {
    let backup = store.clone();
    store
        .remove(id)
        .ok_or_else(|| RpcError::bad_params(format!("account not found: {id}")))?;
    state
        .validate_or_restore_account_store(&store)
        .await
        .map_err(RpcError::bad_params)?;
    accounts::save_account_store(state, &mut store, backup)
        .await
        .map_err(|mut error| {
            error.message = format!("remove account: {}", error.message);
            error
        })?;
    let pool_accounts = store.all().to_vec();
    state.replace_accounts(store);
    state.pool().replace_accounts(pool_accounts).await;
    accounts::account_removed(state, id);
    state.request_model_refresh();
    crate::alerts::sync_quota_incidents(state).await;
    to_value(serde_json::json!({"removed":id}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unbinding_is_provider_scoped_and_idempotent() {
        let mut config = Config::default();
        config.proxy_service.push(ProxyServiceConfig {
            allowed_providers: vec!["kiro".into(), "copilot-a".into(), "copilot-b".into()],
            account_ids: vec![
                "acc_same".into(),
                "kiro/acc_same".into(),
                "copilot-a/acc_same".into(),
                "copilot-b/acc_same".into(),
            ],
            ..ProxyServiceConfig::default()
        });
        assert_eq!(unbind(&mut config, "copilot-a", "acc_same", &[]), 1);
        let service = &config.proxy_service[0];
        assert_eq!(
            service.account_ids,
            ["acc_same", "kiro/acc_same", "copilot-b/acc_same"]
        );
        assert_eq!(service.excluded_account_ids, ["copilot-a/acc_same"]);
        assert!(service.includes_provider_account("kiro", "acc_same", &[]));
        assert!(service.includes_provider_account("copilot-b", "acc_same", &[]));
        assert!(!service.includes_provider_account("copilot-a", "acc_same", &[]));
        assert_eq!(unbind(&mut config, "copilot-a", "acc_same", &[]), 0);
    }
}
