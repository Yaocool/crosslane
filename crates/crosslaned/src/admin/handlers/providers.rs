use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crosslane_core::provider::{ProviderDescriptor, ProviderId, ALL_PROVIDERS};
use crosslane_ipc::protocol::{
    CopilotBrowserCredentials, ProviderAccountListResult, ProviderAccountSummary,
    ProviderModelListResult, ProviderSelector, RpcError,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{accounts::compare_account_names, parse_params, to_value, Handled};
use crate::state::AppState;

pub(super) async fn handle_capabilities(state: &Arc<AppState>) -> Handled {
    let descriptors = state.providers.descriptors().await;
    to_value(json!({
        "schema_version": 2,
        "copilot_headless_device_flow": cfg!(feature = "sso"),
        "rpc_versions": [1, 2],
        "provider_kinds": descriptors.iter().map(|provider| provider.kind.as_str()).collect::<BTreeSet<_>>(),
        "features": [
            "provider_registry",
            "provider_scoped_accounts",
            "provider_scoped_account_tags",
            "provider_scoped_account_probe",
            "provider_scoped_models",
            crosslane_ipc::protocol::GLOBAL_MODEL_DISABLE_FEATURE,
            "provider_scoped_model_mapping",
            "provider_scoped_statistics",
            "copilot_device_flow",
            "copilot_native_messages",
            "copilot_native_chat_completions",
            "copilot_native_responses",
            "copilot_protocol_adaptation",
            "provider_scoped_service_account_pools"
        ]
    }))
}

pub(super) async fn handle_provider_list(state: &Arc<AppState>, params: Value) -> Handled {
    let selector = parse_selector(params)?;
    let providers = filter_descriptors(state.providers.descriptors().await, &selector)?;
    to_value(json!({
        "schema_version": 2,
        "scope": selector,
        "providers": providers,
        "complete": true,
        "errors": {}
    }))
}

pub(super) async fn handle_provider_show(state: &Arc<AppState>, params: Value) -> Handled {
    #[derive(Deserialize)]
    struct Params {
        provider: String,
    }
    let params: Params = parse_params(params)?;
    let id = parse_provider_id(&params.provider)?;
    let descriptors = state.providers.descriptors().await;
    let descriptor = descriptors
        .iter()
        .find(|provider| provider.id == id)
        .ok_or_else(|| unknown_provider(id.as_str(), &descriptors))?;
    to_value(descriptor)
}

pub(super) async fn handle_account_list(state: &Arc<AppState>, params: Value) -> Handled {
    #[derive(Default, Deserialize)]
    struct Params {
        #[serde(flatten)]
        selector: ProviderSelector,
        tag: Option<String>,
        enabled_only: Option<bool>,
        status: Option<String>,
        sort: Option<String>,
    }
    let params: Params = if params.is_null() {
        Params::default()
    } else {
        parse_params(params)?
    };
    let descriptors = filter_descriptors(state.providers.descriptors().await, &params.selector)?;
    let mut accounts = Vec::new();
    let mut errors = BTreeMap::new();
    for descriptor in descriptors {
        match descriptor.kind.as_str() {
            "kiro" => accounts.extend(kiro_accounts(state, &descriptor).await),
            "copilot" => match state.providers.copilot(&descriptor.id).await {
                Some(runtime) => {
                    runtime.refresh_account_info(None, false).await;
                    accounts.extend(
                    runtime
                        .account_summaries()
                        .await
                        .into_iter()
                        .map(|account| {
                            let (enabled, health) =
                                copilot_account_state(descriptor.enabled, &account);
                            ProviderAccountSummary {
                                provider_id: account.provider_id,
                                provider_kind: account.provider_kind,
                                id: account.id,
                                display_name: account.login,
                                email: account.email,
                                label: account.label,
                                enabled,
                                health: health.into(),
                                auth_state: account.auth_state,
                                tags: account.tags,
                                quota_current: None,
                                quota_limit: None,
                                quota_unit: None,
                                supported_models: account.supported_models,
                                details: json!({"endpoint":account.endpoint,"last_error":account.last_error,"created_at":account.created_at,"info":account.info}),
                            }
                        }),
                    );
                }
                None => {
                    errors.insert(
                        descriptor.id.to_string(),
                        "Copilot runtime is unavailable".into(),
                    );
                }
            },
            _ => {
                errors.insert(
                    descriptor.id.to_string(),
                    format!("provider driver {} is unsupported", descriptor.kind),
                );
            }
        }
    }
    let config = state.config.current();
    for account in &mut accounts {
        if account.provider_kind == "copilot" {
            crosslane_ipc::copilot::enrich_account(account);
            account.details["local_aiu_usage"] = json!(state
                .meter
                .account_aiu_usage(&account.provider_id, &account.id));
        }
        // Discovery caches retain the full catalog for re-enabling. Operational
        // account/pool views must not advertise a denied model as schedulable.
        account
            .supported_models
            .retain(|model| !config.is_model_disabled(model));
    }
    accounts.retain(|account| {
        params
            .tag
            .as_ref()
            .is_none_or(|tag| account.tags.contains(tag))
            && (!params.enabled_only.unwrap_or(false) || account.enabled)
            && params
                .status
                .as_ref()
                .is_none_or(|status| account.health == *status)
    });
    match params.sort.as_deref() {
        Some("id") => accounts.sort_by(|left, right| {
            left.provider_id
                .cmp(&right.provider_id)
                .then_with(|| left.id.cmp(&right.id))
        }),
        Some("credit") => accounts.sort_by(|left, right| {
            right
                .quota_current
                .partial_cmp(&left.quota_current)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        Some("email") | None => accounts.sort_by(|left, right| {
            compare_account_names(&left.display_name, &right.display_name)
                .then_with(|| left.provider_id.cmp(&right.provider_id))
                .then_with(|| left.id.cmp(&right.id))
        }),
        Some(other) => {
            return Err(RpcError::bad_params(format!(
                "unknown account sort: {other}"
            )))
        }
    }
    to_value(ProviderAccountListResult {
        schema_version: 2,
        scope: params.selector,
        accounts,
        complete: errors.is_empty(),
        errors,
    })
}

pub(super) async fn handle_account_show(state: &Arc<AppState>, params: Value) -> Handled {
    let reference = parse_account_ref(state, params).await?;
    let result = account_list_for_provider(state, &reference.provider_id).await?;
    let account = result
        .accounts
        .into_iter()
        .find(|account| {
            account.id == reference.account_id
                || account.display_name == reference.account_id
                || account.email.as_deref() == Some(reference.account_id.as_str())
        })
        .ok_or_else(|| RpcError::bad_params(format!("account not found: {reference}")))?;
    to_value(account)
}

pub(super) async fn handle_account_export(state: &Arc<AppState>, params: Value) -> Handled {
    #[derive(Default, Deserialize)]
    struct Params {
        #[serde(flatten)]
        selector: ProviderSelector,
        #[serde(default)]
        redact: bool,
    }
    let params: Params = if params.is_null() {
        Params::default()
    } else {
        parse_params(params)?
    };
    let descriptors = filter_descriptors(state.providers.descriptors().await, &params.selector)?;
    let mut providers = BTreeMap::new();
    let mut errors = BTreeMap::new();
    for descriptor in descriptors {
        let value = match descriptor.kind.as_str() {
            "kiro" => super::handle_account_export(state, json!({"redact":params.redact})),
            "copilot" => match state.providers.copilot(&descriptor.id).await {
                Some(runtime) => Ok(runtime.export_accounts(params.redact).await),
                None => Err(RpcError::internal(format!(
                    "provider runtime {} is unavailable",
                    descriptor.id
                ))),
            },
            kind => Err(RpcError::bad_params(format!(
                "account export is not supported for provider kind {kind}"
            ))),
        };
        match value {
            Ok(accounts) => {
                providers.insert(descriptor.id.to_string(), accounts);
            }
            Err(error) => {
                errors.insert(descriptor.id.to_string(), error.message);
            }
        }
    }
    to_value(json!({
        "schema_version":2,
        "scope":params.selector,
        "redacted":params.redact,
        "providers":providers,
        "complete":errors.is_empty(),
        "errors":errors
    }))
}

pub(super) async fn handle_account_import_token(state: &Arc<AppState>, params: Value) -> Handled {
    #[derive(Deserialize)]
    struct Params {
        provider: String,
        token: String,
        label: Option<String>,
    }
    let params: Params = parse_params(params)?;
    if params.token.trim().is_empty() {
        return Err(RpcError::bad_params("token must not be empty"));
    }
    let runtime = copilot_runtime(state, &params.provider).await?;
    let result = runtime
        .import_token(params.token, params.label)
        .await
        .map_err(provider_rpc_error)?;
    to_value(result)
}

pub(super) async fn handle_login_start(state: &Arc<AppState>, params: Value) -> Handled {
    #[derive(Deserialize)]
    struct Params {
        provider: String,
        #[serde(default = "default_device_auth")]
        auth: String,
        label: Option<String>,
        browser: Option<CopilotBrowserCredentials>,
        #[serde(default)]
        capture_headers: bool,
    }
    fn default_device_auth() -> String {
        "device-flow".into()
    }
    let params: Params = parse_params(params).map_err(|_| {
        RpcError::bad_params("invalid provider login parameters or browser credential fields")
    })?;
    if !matches!(params.auth.as_str(), "device-flow" | "device-flow-headless") {
        return Err(RpcError::bad_params(format!(
            "provider login auth {} is unsupported",
            params.auth
        )));
    }
    if params.auth == "device-flow-headless" && params.browser.is_none() {
        return Err(RpcError::bad_params(
            "remote Device Flow requires browser credentials",
        ));
    }
    if params.capture_headers && params.browser.is_none() {
        return Err(RpcError::bad_params(
            "capture_headers requires remote browser credentials",
        ));
    }
    if let Some(credentials) = &params.browser {
        crate::providers::browser_login::BrowserLoginManager::validate(credentials)?;
    }
    let runtime = copilot_runtime(state, &params.provider).await?;
    let issued = runtime
        .start_device_login_for_user(
            params.label,
            params
                .browser
                .as_ref()
                .map(|credentials| credentials.github_username.clone()),
        )
        .await
        .map_err(provider_rpc_error)?;
    if let Some(credentials) = params.browser {
        let trace_path = params.capture_headers.then(|| {
            state
                .paths
                .data_dir
                .join("providers")
                .join(runtime.id().as_str())
                .join("login-traces")
                .join(format!("{}.headers.jsonl", issued.id))
        });
        match state.providers.browser_logins.start(
            Arc::clone(&runtime),
            issued.clone(),
            credentials,
            trace_path,
            &state.shutdown,
        ) {
            Ok(value) => to_value(value),
            Err(error) => {
                let _ = runtime.cancel_device_login(&issued.id).await;
                Err(error)
            }
        }
    } else {
        to_value(issued)
    }
}

pub(super) async fn handle_login_status(state: &Arc<AppState>, params: Value) -> Handled {
    let (provider, task_id) = parse_login_task(params)?;
    if let Some(result) = state.providers.browser_logins.status(&provider, &task_id) {
        return to_value(result);
    }
    let runtime = copilot_runtime(state, &provider).await?;
    to_value(
        runtime
            .poll_device_login(&task_id)
            .await
            .map_err(provider_rpc_error)?,
    )
}

pub(super) async fn handle_login_cancel(state: &Arc<AppState>, params: Value) -> Handled {
    let (provider, task_id) = parse_login_task(params)?;
    if let Some(result) = state.providers.browser_logins.cancel(&provider, &task_id) {
        return to_value(result);
    }
    let runtime = copilot_runtime(state, &provider).await?;
    to_value(
        runtime
            .cancel_device_login(&task_id)
            .await
            .map_err(provider_rpc_error)?,
    )
}

pub(super) async fn handle_login_submit_code(state: &Arc<AppState>, params: Value) -> Handled {
    #[derive(Deserialize)]
    struct Params {
        provider: String,
        task_id: String,
        code: String,
    }
    let params: Params = parse_params(params)
        .map_err(|_| RpcError::bad_params("invalid login task or one-time code parameters"))?;
    state
        .providers
        .browser_logins
        .submit_code(&params.provider, &params.task_id, params.code)?;
    to_value(json!({"submitted":true}))
}

pub(super) async fn handle_account_set_enabled(state: &Arc<AppState>, params: Value) -> Handled {
    #[derive(Deserialize)]
    struct Params {
        provider: Option<String>,
        id: String,
        enabled: bool,
    }
    let params: Params = parse_params(params)?;
    let reference = resolve_account_ref(state, params.provider.as_deref(), &params.id).await?;
    if reference.provider_id.as_str() == "kiro" {
        return super::handle_account_set_enabled(
            state,
            json!({"id":reference.account_id,"enabled":params.enabled}),
        )
        .await;
    }
    let runtime = copilot_runtime(state, reference.provider_id.as_str()).await?;
    to_value(
        runtime
            .set_account_enabled(&reference.account_id, params.enabled)
            .await
            .map_err(provider_rpc_error)?,
    )
}

pub(super) async fn handle_account_remove(state: &Arc<AppState>, params: Value) -> Handled {
    #[derive(Deserialize)]
    struct Params {
        provider: Option<String>,
        id: String,
    }
    let params: Params = parse_params(params)?;
    let reference = resolve_account_ref(state, params.provider.as_deref(), &params.id).await?;
    if reference.provider_id.as_str() == "kiro" {
        return super::handle_account_remove(state, json!({"id":reference.account_id})).await;
    }
    let runtime = copilot_runtime(state, reference.provider_id.as_str()).await?;
    let _mutation = state.lock_config_mutation().await;
    let account = runtime
        .account(&reference.account_id)
        .await
        .ok_or_else(|| RpcError::bad_params(format!("account not found: {reference}")))?;
    ensure_provider_account_unbound(state, &account.provider_id, &account.id, &account.tags)?;
    to_value(
        runtime
            .remove_account(&reference.account_id)
            .await
            .map_err(provider_rpc_error)?,
    )
}

pub(super) async fn handle_account_refresh(state: &Arc<AppState>, params: Value) -> Handled {
    #[derive(Deserialize)]
    struct Params {
        provider: Option<String>,
        id: String,
    }
    let params: Params = parse_params(params)?;
    let reference = resolve_account_ref(state, params.provider.as_deref(), &params.id).await?;
    if reference.provider_id.as_str() == "kiro" {
        return super::handle_account_refresh(
            state,
            json!({"id":reference.account_id,"all":false}),
        )
        .await;
    }
    let runtime = copilot_runtime(state, reference.provider_id.as_str()).await?;
    to_value(
        runtime
            .refresh_account(&reference.account_id)
            .await
            .map_err(provider_rpc_error)?,
    )
}

pub(super) async fn handle_account_tag(state: &Arc<AppState>, params: Value) -> Handled {
    #[derive(Deserialize)]
    struct Params {
        provider: Option<String>,
        id: Option<String>,
        ids: Option<Vec<String>>,
        #[serde(default)]
        add: Vec<String>,
        #[serde(default)]
        remove: Vec<String>,
    }
    let params: Params = parse_params(params)?;
    let single = params.id.clone();
    let ids = match (params.id, params.ids) {
        (Some(id), None) if !id.trim().is_empty() => vec![id],
        (None, Some(ids)) if !ids.is_empty() => ids,
        _ => return Err(RpcError::bad_params("specify either a non-empty id or ids")),
    };
    let normalize = |tags: Vec<String>| -> Result<Vec<String>, RpcError> {
        tags.into_iter()
            .map(|tag| {
                let tag = tag.trim();
                if tag.is_empty() {
                    Err(RpcError::bad_params("account tag must not be empty"))
                } else {
                    Ok(tag.to_owned())
                }
            })
            .collect()
    };
    let add = normalize(params.add)?;
    let remove = normalize(params.remove)?;
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut ordered = Vec::new();
    let mut seen = BTreeSet::new();
    // Resolve and validate the entire batch before any provider is mutated.
    for id in &ids {
        let reference = resolve_account_ref(state, params.provider.as_deref(), id).await?;
        let value = handle_account_show(
            state,
            json!({"provider":reference.provider_id,"id":reference.account_id}),
        )
        .await?;
        let account: ProviderAccountSummary =
            serde_json::from_value(value).map_err(|error| RpcError::internal(error.to_string()))?;
        let key = (account.provider_id.clone(), account.id.clone());
        if !seen.insert(key.clone()) {
            return Err(RpcError::bad_params(format!(
                "account specified more than once: {reference}"
            )));
        }
        let mut tags = account.tags.iter().cloned().collect::<BTreeSet<_>>();
        tags.extend(add.iter().cloned());
        tags.retain(|tag| !remove.contains(tag));
        if tags != account.tags.iter().cloned().collect() {
            ensure_provider_tags_change_safe(
                state,
                &account.provider_id,
                &account.id,
                &account.tags,
                &tags.iter().cloned().collect::<Vec<_>>(),
            )?;
        }
        if account.provider_id == "kiro" {
            let kiro = state
                .with_accounts(|store| store.find(&account.id).cloned())
                .ok_or_else(|| RpcError::bad_params(format!("account not found: {id}")))?;
            let mut tags = account.tags.iter().cloned().collect::<BTreeSet<_>>();
            tags.extend(add.iter().cloned());
            tags.retain(|tag| !remove.contains(tag));
            if tags != account.tags.iter().cloned().collect() {
                super::accounts::ensure_account_unbound(state, &kiro, "changing its tags")?;
            }
        }
        groups
            .entry(account.provider_id)
            .or_default()
            .push(account.id);
        ordered.push(key);
    }
    let mut results = BTreeMap::new();
    for (provider, ids) in groups {
        if provider == "kiro" {
            let value =
                super::handle_account_tag(state, json!({"ids":ids,"add":add,"remove":remove}))
                    .await?;
            let batch: crosslane_ipc::protocol::AccountTagBatchResult =
                serde_json::from_value(value)
                    .map_err(|error| RpcError::internal(error.to_string()))?;
            for account in batch.accounts {
                results.insert((provider.clone(), account.id.clone()), json!(account));
            }
        } else {
            let runtime = copilot_runtime(state, &provider).await?;
            let _mutation = state.lock_config_mutation().await;
            for id in &ids {
                let account = runtime
                    .account(id)
                    .await
                    .ok_or_else(|| RpcError::bad_params(format!("account not found: {id}")))?;
                let mut tags = account.tags.iter().cloned().collect::<BTreeSet<_>>();
                tags.extend(add.iter().cloned());
                tags.retain(|tag| !remove.contains(tag));
                if tags != account.tags.iter().cloned().collect() {
                    ensure_provider_tags_change_safe(
                        state,
                        &provider,
                        &account.id,
                        &account.tags,
                        &tags.into_iter().collect::<Vec<_>>(),
                    )?;
                }
            }
            for account in runtime
                .update_accounts_tags(&ids, &add, &remove)
                .await
                .map_err(provider_rpc_error)?
            {
                results.insert(
                    (provider.clone(), account.id.clone()),
                    json!({"id":account.id,"provider_id":provider,"tags":account.tags}),
                );
            }
        }
    }
    let mut accounts = ordered
        .into_iter()
        .filter_map(|key| results.remove(&key))
        .collect::<Vec<_>>();
    if let Some(id) = single {
        let mut result = accounts.remove(0);
        result["id"] = json!(id);
        Ok(result)
    } else {
        Ok(json!({"accounts":accounts}))
    }
}

fn ensure_provider_account_unbound(
    state: &AppState,
    provider: &str,
    id: &str,
    tags: &[String],
) -> Result<(), RpcError> {
    let config = state.config.current();
    let services = config
        .proxy_service
        .iter()
        .filter(|service| service.includes_provider_account(provider, id, tags))
        .map(|service| format!("{} ({})", service.name, service.id))
        .collect::<Vec<_>>();
    if services.is_empty() {
        return Ok(());
    }
    Err(RpcError::bad_params(format!("account {provider}/{id} is bound to proxy service(s) {}; unbind it before changing tags or removing it",services.join(", "))))
}

fn ensure_provider_tags_change_safe(
    state: &AppState,
    provider: &str,
    id: &str,
    before: &[String],
    after: &[String],
) -> Result<(), RpcError> {
    // Labels on global/manual pools do not change membership. Only removal from
    // an existing tagged pool needs an explicit service unbind first.
    let config = state.config.current();
    let leaves_pool = config.proxy_service.iter().any(|service| {
        service.includes_provider_account(provider, id, before)
            && !service.includes_provider_account(provider, id, after)
    });
    if leaves_pool {
        ensure_provider_account_unbound(state, provider, id, before)
    } else {
        Ok(())
    }
}

pub(super) async fn handle_account_probe(state: &Arc<AppState>, params: Value) -> Handled {
    #[derive(Deserialize)]
    struct Params {
        provider: Option<String>,
        id: String,
    }
    let params: Params = parse_params(params)?;
    let reference = resolve_account_ref(state, params.provider.as_deref(), &params.id).await?;
    if reference.provider_id.as_str() == "kiro" {
        return super::handle_account_probe(state, json!({"id":reference.account_id,"all":false}))
            .await;
    }
    let runtime = copilot_runtime(state, reference.provider_id.as_str()).await?;
    let account = runtime
        .refresh_account(&reference.account_id)
        .await
        .map_err(provider_rpc_error)?;
    to_value(json!({
        "provider_id":reference.provider_id,
        "account_id":account.id,
        "models":account.supported_models,
        "ok":true,
        "account":account
    }))
}

pub(super) async fn handle_account_reset_health(state: &Arc<AppState>, params: Value) -> Handled {
    #[derive(Deserialize)]
    struct Params {
        provider: Option<String>,
        id: String,
    }
    let params: Params = parse_params(params)?;
    let reference = resolve_account_ref(state, params.provider.as_deref(), &params.id).await?;
    if reference.provider_id.as_str() == "kiro" {
        return super::handle_account_reset_health(
            state,
            json!({"id":reference.account_id,"all":false}),
        )
        .await;
    }
    let runtime = copilot_runtime(state, reference.provider_id.as_str()).await?;
    let account = runtime
        .reset_account_health(&reference.account_id)
        .await
        .map_err(provider_rpc_error)?;
    to_value(json!({
        "provider_id":reference.provider_id,
        "reset":[account.id],
        "account":account
    }))
}

pub(super) async fn handle_models(state: &Arc<AppState>, params: Value) -> Handled {
    #[derive(Default, Deserialize)]
    struct Params {
        #[serde(flatten)]
        selector: ProviderSelector,
        #[serde(default)]
        refresh: bool,
    }
    let params: Params = if params.is_null() {
        Params::default()
    } else {
        parse_params(params)?
    };
    let mut descriptors =
        filter_descriptors(state.providers.descriptors().await, &params.selector)?;
    if params
        .selector
        .provider
        .as_deref()
        .is_none_or(|provider| provider == ALL_PROVIDERS)
    {
        descriptors.retain(|provider| provider.enabled);
    }
    if descriptors.is_empty() {
        return Err(RpcError::bad_params(
            "no enabled provider matches the requested scope",
        ));
    }
    let mut refresh_errors = BTreeMap::new();
    if params.refresh
        && descriptors
            .iter()
            .any(|provider| provider.kind == "kiro" && provider.enabled)
    {
        if let Err(error) = crate::tasks::refresh_models_for(state, Some("kiro")).await {
            tracing::warn!(%error, "provider-scoped Kiro model refresh failed");
            refresh_errors.insert("kiro".into(), error.to_string());
        }
    }
    let ids = descriptors
        .into_iter()
        .map(|provider| provider.id)
        .collect::<Vec<_>>();
    let (models, mut errors) = state.providers.models(&ids, params.refresh).await;
    errors.append(&mut refresh_errors);
    to_value(ProviderModelListResult {
        disabled_models: state.config.current().models.disabled.clone(),
        schema_version: 2,
        scope: params.selector,
        models,
        complete: errors.is_empty(),
        errors,
    })
}

fn copilot_account_state(
    provider_enabled: bool,
    account: &crosslane_copilot::CopilotAccountSummary,
) -> (bool, &'static str) {
    let enabled = provider_enabled && account.enabled;
    let health = if !enabled {
        "disabled"
    } else if account.is_available() {
        "available"
    } else {
        "unavailable"
    };
    (enabled, health)
}

async fn kiro_accounts(
    state: &Arc<AppState>,
    descriptor: &ProviderDescriptor,
) -> Vec<ProviderAccountSummary> {
    let config = state.config.current();
    let pool = state.pool();
    let mut output = Vec::new();
    for account in pool.snapshot().await {
        let health = if descriptor.enabled {
            super::effective_account_health(&pool, &account, &config.pool).await
        } else {
            "disabled".into()
        };
        let supported_models = match pool.get(&account.id).await {
            Some(runtime) if runtime.has_model_cache().await => runtime.supported_models().await,
            _ => Vec::new(),
        };
        output.push(ProviderAccountSummary {
            provider_id: descriptor.id.to_string(),
            provider_kind: "kiro".into(),
            id: account.id.clone(),
            display_name: account.display_name().to_owned(),
            email: Some(account.email.clone()),
            label: account.label.clone(),
            enabled: descriptor.enabled && account.enabled,
            health,
            auth_state: if account.credentials.expires_at > 0
                && account.credentials.expires_at <= crate::meter::now_secs()
            {
                "expired"
            } else {
                "ready"
            }
            .into(),
            tags: account.tags.clone(),
            quota_current: account.usage.as_ref().map(|usage| usage.current),
            quota_limit: account
                .usage
                .as_ref()
                .map(|usage| crosslane_pool::effective_credit_limit(usage, &config.pool)),
            quota_unit: Some("kiro_credits".into()),
            supported_models,
            details: json!({
                "region":account.credentials.region,
                "auth_method":account.credentials.auth_method,
                "machine_id":account.machine_id,
                "overage_enabled":config.pool.enable_overage,
                "overage_cap":account.usage.as_ref().and_then(|usage| usage.overage_cap),
                "kiro_overage_cap":account.usage.as_ref().and_then(|usage| usage.overage_cap),
                "kiro_overage_total_limit":account.usage.as_ref().and_then(|usage| {
                    usage.overage_cap.map(|_| usage.limit)
                }),
                "max_overage_credits_per_account":config.pool.max_overage_credits_per_account,
                "effective_overage_cap":account.usage.as_ref().and_then(|usage| {
                    usage.overage_cap.map(|_| {
                        crosslane_pool::effective_credit_limit(usage, &config.pool)
                            - usage.limit_without_overage()
                    })
                }),
                "created_at":account.created_at,
                "token_expires_at":account.credentials.expires_at,
                "subscription":account.subscription.as_ref().map(|subscription| {
                    subscription.title.clone().unwrap_or_else(|| format!("{:?}", subscription.kind))
                })
            }),
        });
    }
    output
}

fn parse_selector(params: Value) -> Result<ProviderSelector, RpcError> {
    if params.is_null() {
        Ok(ProviderSelector::default())
    } else {
        parse_params(params)
    }
}

fn filter_descriptors(
    descriptors: Vec<ProviderDescriptor>,
    selector: &ProviderSelector,
) -> Result<Vec<ProviderDescriptor>, RpcError> {
    if selector.provider.as_deref() == Some("") {
        return Err(RpcError::bad_params("provider must not be empty"));
    }
    if let Some(id) = selector
        .provider
        .as_deref()
        .filter(|id| *id != ALL_PROVIDERS)
    {
        if !descriptors
            .iter()
            .any(|provider| provider.id.as_str() == id)
        {
            return Err(unknown_provider(id, &descriptors));
        }
    }
    let mut output = descriptors
        .into_iter()
        .filter(|descriptor| {
            selector.provider.as_deref().is_none_or(|selected| {
                selected == ALL_PROVIDERS || descriptor.id.as_str() == selected
            }) && selector
                .provider_kind
                .as_deref()
                .is_none_or(|kind| descriptor.kind == kind)
        })
        .collect::<Vec<_>>();
    output.sort_by(|left, right| left.id.cmp(&right.id));
    if output.is_empty() {
        return Err(RpcError::bad_params(
            "no provider matches the requested scope",
        ));
    }
    Ok(output)
}

pub(super) fn unknown_provider(id: &str, descriptors: &[ProviderDescriptor]) -> RpcError {
    let mut available = descriptors
        .iter()
        .map(|provider| format!("{} (kind={})", provider.id, provider.kind))
        .collect::<Vec<_>>();
    available.sort();
    RpcError::bad_params(format!(
        "provider not found: {id}; 可用实例 ID：{}。--provider 使用实例 ID，不是类型名称；运行 `crosslane provider list` 查看",
        if available.is_empty() { "无".into() } else { available.join(", ") }
    ))
}

async fn account_list_for_provider(
    state: &Arc<AppState>,
    provider_id: &ProviderId,
) -> Result<ProviderAccountListResult, RpcError> {
    let value = handle_account_list(state, json!({"provider":provider_id.as_str()})).await?;
    serde_json::from_value(value).map_err(|error| RpcError::internal(error.to_string()))
}

#[derive(Debug)]
pub(super) struct ResolvedAccountRef {
    pub(super) provider_id: ProviderId,
    pub(super) account_id: String,
}

impl std::fmt::Display for ResolvedAccountRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.provider_id, self.account_id)
    }
}

async fn parse_account_ref(
    state: &Arc<AppState>,
    params: Value,
) -> Result<ResolvedAccountRef, RpcError> {
    #[derive(Deserialize)]
    struct Params {
        provider: Option<String>,
        id: String,
    }
    let params: Params = parse_params(params)?;
    resolve_account_ref(state, params.provider.as_deref(), &params.id).await
}

pub(super) async fn resolve_account_ref(
    state: &Arc<AppState>,
    explicit_provider: Option<&str>,
    id: &str,
) -> Result<ResolvedAccountRef, RpcError> {
    let explicit_provider = explicit_provider.filter(|provider| *provider != "all");
    let id = id.trim();
    let (qualified_provider, account_id) = id
        .split_once('/')
        .map_or((None, id), |(provider, account)| (Some(provider), account));
    if explicit_provider.is_some()
        && qualified_provider.is_some()
        && explicit_provider != qualified_provider
    {
        return Err(RpcError::bad_params(
            "qualified account reference conflicts with --provider",
        ));
    }
    let list: ProviderAccountListResult =
        if let Some(provider) = explicit_provider.or(qualified_provider) {
            account_list_for_provider(state, &parse_provider_id(provider)?).await?
        } else {
            serde_json::from_value(handle_account_list(state, Value::Null).await?)
                .map_err(|error| RpcError::internal(error.to_string()))?
        };
    if !list.complete {
        return Err(RpcError::bad_params("cannot resolve an unqualified account while provider discovery is incomplete; specify --provider"));
    }
    let matches = list
        .accounts
        .into_iter()
        .filter(|account| {
            account.id == account_id
                || account.display_name.eq_ignore_ascii_case(account_id)
                || account
                    .email
                    .as_deref()
                    .is_some_and(|email| email.eq_ignore_ascii_case(account_id))
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [account] => Ok(ResolvedAccountRef {
            provider_id: parse_provider_id(&account.provider_id)?,
            account_id: account.id.clone(),
        }),
        [] => Err(RpcError::bad_params(format!("account not found: {id}"))),
        _ => Err(RpcError::bad_params(format!(
            "account reference {id} is ambiguous; use provider/account_id"
        ))),
    }
}

fn parse_provider_id(value: &str) -> Result<ProviderId, RpcError> {
    ProviderId::parse(value).map_err(|error| RpcError::bad_params(error.to_string()))
}

async fn copilot_runtime(
    state: &Arc<AppState>,
    provider: &str,
) -> Result<Arc<crosslane_copilot::CopilotProvider>, RpcError> {
    let id = parse_provider_id(provider)?;
    let descriptors = state.providers.descriptors().await;
    let descriptor = descriptors
        .iter()
        .find(|descriptor| descriptor.id == id)
        .ok_or_else(|| unknown_provider(provider, &descriptors))?;
    if descriptor.kind != "copilot" {
        return Err(RpcError::bad_params(format!(
            "provider {id} 的类型为 {}；该命令仅支持 GitHub Copilot (kind=copilot)",
            descriptor.kind
        )));
    }
    state.providers.copilot(&id).await.ok_or_else(|| {
        RpcError::bad_params(format!(
            "Copilot 实例 {id} 当前不可用；运行 `crosslane provider show {id}` 查看状态"
        ))
    })
}

fn parse_login_task(params: Value) -> Result<(String, String), RpcError> {
    #[derive(Deserialize)]
    struct Params {
        provider: String,
        task_id: String,
    }
    let params: Params = parse_params(params)?;
    Ok((params.provider, params.task_id))
}

fn provider_rpc_error(error: crosslane_provider::ProviderError) -> RpcError {
    RpcError {
        code: i32::from(error.status.as_u16()),
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosslane_copilot::CopilotAccountSummary;

    fn account() -> CopilotAccountSummary {
        CopilotAccountSummary {
            provider_id: "copilot".into(),
            provider_kind: "copilot".into(),
            id: "gh_1".into(),
            login: "octocat".into(),
            email: None,
            label: None,
            enabled: true,
            auth_state: "ready".into(),
            tags: Vec::new(),
            supported_models: vec!["gpt-test".into()],
            endpoint: None,
            created_at: 0,
            last_error: None,
            info: Default::default(),
        }
    }

    #[test]
    fn disabled_provider_makes_its_copilot_accounts_unavailable() {
        assert_eq!(
            copilot_account_state(false, &account()),
            (false, "disabled")
        );
        assert_eq!(copilot_account_state(true, &account()), (true, "available"));
    }

    #[test]
    fn missing_provider_errors_list_actual_ids_without_substituting_driver_names() {
        let descriptors = ["work-cp", "personal-cp"].map(|id| ProviderDescriptor {
            id: ProviderId::parse(id).unwrap(),
            kind: "copilot".into(),
            enabled: true,
            status: "ready".into(),
            error: None,
            capabilities: Default::default(),
        });
        let selector = ProviderSelector {
            provider: Some("github-copilot".into()),
            provider_kind: None,
        };
        let error = filter_descriptors(descriptors.to_vec(), &selector).unwrap_err();
        assert_eq!(error.code, 400);
        assert!(error.message.contains("work-cp (kind=copilot)"));
        assert!(error.message.contains("personal-cp (kind=copilot)"));
        assert!(error.message.contains("crosslane provider list"));
        let selected = filter_descriptors(
            descriptors.to_vec(),
            &ProviderSelector {
                provider: Some("work-cp".into()),
                provider_kind: Some("copilot".into()),
            },
        )
        .unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id.as_str(), "work-cp");
    }
}
