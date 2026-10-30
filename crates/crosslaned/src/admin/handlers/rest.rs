//! REST-specific mutations reuse the control plane's storage transactions.

use super::*;

pub(in crate::admin) async fn account_snapshot(
    state: &Arc<AppState>,
    provider: &str,
    id: &str,
) -> Handled {
    providers::cached_account_show(state, serde_json::json!({"provider":provider,"id":id})).await
}

pub(in crate::admin) fn validate_kiro_account(account: &Account) -> Result<(), RpcError> {
    accounts::validate_account_input(account)
}

pub(in crate::admin) async fn create_kiro_account(
    state: &Arc<AppState>,
    account: Account,
) -> Handled {
    accounts::validate_account_input(&account)?;
    let id = account.id.clone();
    accounts::commit_account_change(state, move |store| {
        if store
            .all()
            .iter()
            .any(|current| current.email.eq_ignore_ascii_case(&account.email))
        {
            return Err(RpcError {
                code: 409,
                message: "account email is already configured".into(),
            });
        }
        store.insert(account).map_err(|_| RpcError {
            code: 409,
            message: "account already exists".into(),
        })?;
        Ok(())
    })
    .await?;
    providers::handle_account_show(state, serde_json::json!({"provider":"kiro", "id":id})).await
}

pub(in crate::admin) async fn set_service_enabled(
    state: &Arc<AppState>,
    selector: &str,
    enabled: bool,
) -> Handled {
    let _file_lock = crosslane_store::atomic::lock_file_exclusive(&state.paths.config_file)
        .await
        .map_err(|error| RpcError::internal(error.to_string()))?;
    let _mutation = state.lock_config_mutation().await;
    let raw = tokio::fs::read_to_string(&state.paths.config_file)
        .await
        .map_err(|error| RpcError::internal(error.to_string()))?;
    let disk = load_config(&state.paths.config_file)
        .await
        .map_err(|_| RpcError::internal("could not load service configuration"))?;
    let (mut next, _) = merge_hot_reload(&state.config.current(), disk);
    let service = index_by_id_or_name(&next.proxy_service, selector, |service| {
        (Some(service.id.as_str()), Some(service.name.as_str()))
    })
    .and_then(|index| next.proxy_service.get_mut(index))
    .ok_or_else(|| RpcError {
        code: 404,
        message: "service not found".into(),
    })?;
    service.enabled = enabled;
    let id = service.id.clone();
    next.validate()
        .map_err(|error| RpcError::bad_params(error.to_string()))?;
    persist_service_config_locked(state, &raw, &next).await?;
    let view = state
        .proxy_services
        .views(&next)
        .await
        .into_iter()
        .find(|service| service.id == id)
        .ok_or_else(|| RpcError::internal("updated service is unavailable"))?;
    to_value(view)
}

pub(in crate::admin) async fn refresh_kiro_api_key_usage(
    state: &Arc<AppState>,
    id: &str,
) -> Handled {
    let changed = crate::tasks::refresh_account_usage(state, &state.pool(), id)
        .await
        .map_err(|error| RpcError {
            code: 502,
            message: crosslane_translate::sanitize_error_message(&error.to_string()),
        })?;
    persist_pool_snapshot(state).await?;
    Ok(serde_json::json!({"ok":true,"refreshed":false,"persisted":true,"usage_refreshed":changed}))
}
