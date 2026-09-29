//! 管理面各方法实现。

use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

use futures::{stream, StreamExt};
use kproxy_core::account::Account;
use kproxy_core::config::{
    ApiKeyConfig, ApiKeyFormat, Config, PoolConfig, ProxyServiceConfig, ServiceAccountTags,
};
use kproxy_core::ids::{new_account_id, new_machine_id};
use kproxy_ipc::protocol::{
    method, AccountAddSsoParams, AccountDetail, AccountImportParams, AccountImportResult,
    AccountListParams, AccountListResult, AccountRefParams, AccountServiceBinding,
    AccountServicesResult, AccountSetEnabledParams, AccountSummary, AccountTagBatchParams,
    AccountTagBatchResult, AccountTagParams, AccountTagResult, ConfigPathResult,
    ConfigReloadResult, ConfigShowResult, CreatedApiKey, LogFileView, LogFilesResult,
    LogTraceEntry, LogTraceResult, ModelResolutionAccount, ModelResolutionResult,
    ProviderAccountListResult, ProviderStatusView, ProxyServiceAccountChangeParams,
    ProxyServiceAccountsParams, ProxyServiceAccountsResult, ProxyServiceApiKeyView,
    ProxyServiceApiKeysParams, ProxyServiceApiKeysResult, ProxyServiceCreateParams,
    ProxyServiceCreateResult, ProxyServiceDeleteParams, ProxyServiceDeleteResult,
    ProxyServiceListResult, Request, Response, RpcError, StatusResult,
};
use kproxy_pool::{
    account_credit_state, effective_credit_limit, remaining_credit_percent, AccountCreditState,
    AccountPool,
};
use kproxy_store::accounts::AccountStore;
use kproxy_store::config_loader::{load_config, merge_hot_reload};
use rand::RngCore;
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::state::AppState;

type Handled = Result<serde_json::Value, RpcError>;

mod providers;

/// 分发一个 RPC 请求。
pub async fn dispatch(state: &Arc<AppState>, request: Request) -> Response {
    let id = request.id;
    let outcome = match request.method.as_str() {
        method::V2_CAPABILITIES => providers::handle_capabilities(state).await,
        method::V2_PROVIDER_LIST => providers::handle_provider_list(state, request.params).await,
        method::V2_PROVIDER_SHOW => providers::handle_provider_show(state, request.params).await,
        method::V2_ACCOUNT_LIST => providers::handle_account_list(state, request.params).await,
        method::V2_ACCOUNT_SHOW => providers::handle_account_show(state, request.params).await,
        method::V2_ACCOUNT_IMPORT_TOKEN => {
            providers::handle_account_import_token(state, request.params).await
        }
        method::V2_LOGIN_START => providers::handle_login_start(state, request.params).await,
        method::V2_LOGIN_STATUS => providers::handle_login_status(state, request.params).await,
        method::V2_LOGIN_CANCEL => providers::handle_login_cancel(state, request.params).await,
        method::V2_LOGIN_SUBMIT_CODE => {
            providers::handle_login_submit_code(state, request.params).await
        }
        method::V2_ACCOUNT_SET_ENABLED => {
            providers::handle_account_set_enabled(state, request.params).await
        }
        method::V2_ACCOUNT_REMOVE => providers::handle_account_remove(state, request.params).await,
        method::V2_ACCOUNT_REFRESH => {
            providers::handle_account_refresh(state, request.params).await
        }
        method::V2_ACCOUNT_TAG => providers::handle_account_tag(state, request.params).await,
        method::V2_ACCOUNT_PROBE => providers::handle_account_probe(state, request.params).await,
        method::V2_ACCOUNT_RESET_HEALTH => {
            providers::handle_account_reset_health(state, request.params).await
        }
        method::V2_ACCOUNT_EXPORT => providers::handle_account_export(state, request.params).await,
        method::V2_MODELS => providers::handle_models(state, request.params).await,
        method::STATUS => handle_status(state, request.params).await,
        method::CONFIG_SHOW => handle_config_show(state).await,
        method::CONFIG_PATH => to_value(handle_config_path(state)),
        method::CONFIG_RELOAD => handle_config_reload(state).await,
        method::ACCOUNT_LIST => handle_account_list(state, request.params).await,
        method::ACCOUNT_SHOW => handle_account_show(state, request.params).await,
        method::ACCOUNT_SERVICES => handle_account_services(state, request.params).await,
        method::ACCOUNT_IMPORT => handle_account_import(state, request.params).await,
        method::ACCOUNT_EXPORT => handle_account_export(state, request.params),
        method::ACCOUNT_ADD_SSO => handle_account_add_sso(state, request.params).await,
        method::ACCOUNT_REMOVE => handle_account_remove(state, request.params).await,
        method::ACCOUNT_SET_ENABLED => handle_account_set_enabled(state, request.params).await,
        method::ACCOUNT_TAG => providers::handle_account_tag(state, request.params).await,
        method::ACCOUNT_REGEN_MACHINE_ID => {
            handle_regenerate_machine_id(state, request.params).await
        }
        method::ACCOUNT_REFRESH => handle_account_refresh(state, request.params).await,
        method::ACCOUNT_PROBE | method::DIAGNOSE_ACCOUNT => {
            handle_account_probe(state, request.params).await
        }
        method::DIAGNOSE_ENDPOINTS => handle_diagnose_endpoints(request.params).await,
        method::SUBSCRIPTIONS => handle_subscriptions(state, request.params).await,
        method::ACCOUNT_RESET_HEALTH => handle_account_reset_health(state, request.params).await,
        method::POOL => handle_pool(state, request.params).await,
        method::TASKS => Ok(state.task_registry.snapshot(state)),
        method::TASK_RUN => handle_task_run(state, request.params).await,
        method::STATS => handle_stats(state, request.params).await,
        method::LOGS => handle_logs(state, request.params).await,
        method::LOG_FILES => handle_log_files(state).await,
        method::LOG_TRACE => handle_log_trace(state, request.params).await,
        method::MODELS => handle_models(state).await,
        method::MODEL_RESOLVE => handle_model_resolve(state, request.params).await,
        method::APIKEY_LIST => handle_apikey_list(state, request.params).await,
        method::APIKEY_RESET_USAGE => handle_apikey_reset(state, request.params).await,
        method::SERVICE_LIST => handle_service_list(state, request.params).await,
        method::SERVICE_CREATE => handle_service_create(state, request.params).await,
        method::SERVICE_DELETE => handle_service_delete(state, request.params).await,
        method::SERVICE_APIKEYS => handle_service_apikeys(state, request.params),
        method::SERVICE_ACCOUNTS => handle_service_accounts(state, request.params).await,
        method::SERVICE_ACCOUNT_ADD => {
            handle_service_account_change(state, request.params, true).await
        }
        method::SERVICE_ACCOUNT_REMOVE => {
            handle_service_account_change(state, request.params, false).await
        }
        method::WEBHOOK_LIST => handle_webhook_list(state),
        method::WEBHOOK_TEST => handle_webhook_test(state, request.params),
        method::WEBHOOK_LOGS => handle_webhook_logs(state, request.params),
        other => Err(RpcError::unknown_method(other)),
    };
    match outcome {
        Ok(result) => Response::ok(id, result),
        Err(error) => Response::err(id, error),
    }
}

async fn handle_logs(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    #[derive(serde::Deserialize, Default)]
    struct Params {
        after_request_id: Option<String>,
        #[serde(default = "default_tail")]
        tail: usize,
        #[serde(default)]
        wait_ms: u64,
        level: Option<String>,
        account: Option<String>,
        provider: Option<String>,
    }
    fn default_tail() -> usize {
        50
    }
    let params: Params = parse_params(params)?;
    let entries = state
        .stats
        .follow(
            params.after_request_id.as_deref(),
            params.tail,
            params.wait_ms,
            params.level.as_deref(),
            params.account.as_deref(),
            params.provider.as_deref(),
        )
        .await;
    to_value(serde_json::json!({"entries":entries}))
}

async fn handle_log_files(state: &Arc<AppState>) -> Handled {
    let config = state.config.current();
    let base_path = resolved_log_base_path(state, &config.log);
    let scan_path = base_path.clone();
    let files = tokio::task::spawn_blocking(move || crate::logging::discover_log_files(&scan_path))
        .await
        .map_err(|error| RpcError::internal(format!("log file scan failed: {error}")))?
        .map_err(|error| RpcError::internal(format!("log file scan failed: {error}")))?
        .into_iter()
        .map(|file| LogFileView {
            path: file.path.display().to_string(),
            host_path: None,
            level: file.level,
            date: file.date,
            size_bytes: file.size_bytes,
            modified_at: file.modified_at,
        })
        .collect();
    let directory = base_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    to_value(LogFilesResult {
        base_path: base_path.display().to_string(),
        host_base_path: None,
        directory: directory.display().to_string(),
        host_directory: None,
        format: config.log.format.clone(),
        level_filter: config.log.level.clone(),
        files,
    })
}

async fn handle_log_trace(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    #[derive(serde::Deserialize)]
    struct Params {
        trace_id: String,
        #[serde(default = "default_tail")]
        tail: usize,
        level: Option<String>,
    }
    fn default_tail() -> usize {
        200
    }

    let params: Params = parse_params(params)?;
    validate_trace_id(&params.trace_id)?;
    let level = params
        .level
        .as_deref()
        .map(normalize_log_level)
        .transpose()?;
    let config = state.config.current();
    let base_path = resolved_log_base_path(state, &config.log);
    let trace_id = params.trace_id.clone();
    let scan_trace_id = trace_id.clone();
    let scan = tokio::task::spawn_blocking(move || {
        crate::logging::scan_trace_logs(&base_path, &scan_trace_id, level.as_deref(), params.tail)
    })
    .await
    .map_err(|error| RpcError::internal(format!("trace log scan failed: {error}")))?
    .map_err(|error| RpcError::internal(format!("trace log scan failed: {error}")))?;
    to_value(LogTraceResult {
        trace_id,
        entries: scan
            .entries
            .into_iter()
            .map(|entry| LogTraceEntry {
                path: entry.path.display().to_string(),
                level: entry.level,
                date: entry.date,
                line: entry.line,
                record: entry.record,
            })
            .collect(),
        files_scanned: scan.files_scanned,
        bytes_scanned: scan.bytes_scanned,
        matched_records: scan.matched_records,
        truncated: scan.truncated,
    })
}

fn validate_trace_id(trace_id: &str) -> Result<(), RpcError> {
    let suffix = trace_id
        .strip_prefix("trace_")
        .ok_or_else(|| RpcError::bad_params("trace_id must start with trace_"))?;
    if suffix.len() != 32
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(RpcError::bad_params(
            "trace_id must be trace_ followed by 32 lowercase hexadecimal characters",
        ));
    }
    Ok(())
}

fn normalize_log_level(level: &str) -> Result<String, RpcError> {
    match level.to_ascii_lowercase().as_str() {
        "trace" | "debug" | "info" | "warn" | "error" => Ok(level.to_ascii_lowercase()),
        "warning" => Ok("warn".into()),
        _ => Err(RpcError::bad_params(
            "level must be trace, debug, info, warn, or error",
        )),
    }
}

fn resolved_log_base_path(
    state: &AppState,
    config: &kproxy_core::config::LogConfig,
) -> std::path::PathBuf {
    let path = crate::logging::configured_log_path(
        config,
        &state.paths.data_dir.join("logs").join("kproxyd.log"),
    );
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(path)
    }
}

fn handle_webhook_list(state: &Arc<AppState>) -> Handled {
    let logs = state.notifier().logs(1_000);
    let mut targets = state
        .config
        .current()
        .webhook
        .iter()
        .map(|target| {
            let latest = logs.iter().find(|log| log.target == target.name);
            serde_json::json!({
                "name":target.name,
                "type":target.kind,
                "events":target.events,
                "enabled":target.enabled,
                "status":latest.map(|log| if log.success {"ok"} else {"failed"}).unwrap_or("never"),
                "last_delivery_at":latest.map(|log| log.timestamp)
            })
        })
        .collect::<Vec<_>>();
    targets.sort_by(|left, right| {
        compare_display_text(
            left["name"].as_str().unwrap_or_default(),
            right["name"].as_str().unwrap_or_default(),
        )
    });
    to_value(targets)
}

pub(super) fn handle_account_export(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    #[derive(serde::Deserialize, Default)]
    struct Params {
        #[serde(default)]
        redact: bool,
    }
    let params: Params = if params.is_null() {
        Params::default()
    } else {
        parse_params(params)?
    };
    let mut accounts = state.with_accounts(|store| store.all().to_vec());
    accounts.sort_by(|left, right| {
        compare_account_identity(&left.email, &left.id, &right.email, &right.id)
    });
    if params.redact {
        for account in &mut accounts {
            account.credentials.access_token = "<redacted>".into();
            account.credentials.refresh_token = account
                .credentials
                .refresh_token
                .as_ref()
                .map(|_| "<redacted>".into());
            account.credentials.client_secret = account
                .credentials
                .client_secret
                .as_ref()
                .map(|_| "<redacted>".into());
        }
    }
    to_value(accounts)
}

async fn handle_diagnose_endpoints(params: serde_json::Value) -> Handled {
    #[derive(serde::Deserialize)]
    struct Params {
        #[serde(default = "default_region")]
        region: String,
    }
    fn default_region() -> String {
        "us-east-1".into()
    }
    let params: Params = parse_params(params)?;
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|error| RpcError::internal(error.to_string()))?;
    let endpoints = vec![
        (
            "codewhisperer",
            kproxy_kiro::endpoint::CODEWHISPERER_URL.to_string(),
        ),
        ("amazonq", kproxy_kiro::endpoint::AMAZONQ_URL.to_string()),
        (
            "oidc",
            format!("https://oidc.{}.amazonaws.com/token", params.region),
        ),
    ];
    let probes = futures::future::join_all(endpoints.into_iter().map(|(name, url)| {
        let client = client.clone();
        async move {
            let started = std::time::Instant::now();
            match client.post(&url).json(&serde_json::json!({})).send().await {
                Ok(response) => serde_json::json!({
                    "name":name,"url":url,"reachable":true,
                    "status":response.status().as_u16(),
                    "latency_ms":started.elapsed().as_millis()
                }),
                Err(error) => serde_json::json!({
                    "name":name,"url":url,"reachable":false,
                    "latency_ms":started.elapsed().as_millis(),
                    "error":kproxy_translate::sanitize_error_message(&error.to_string())
                }),
            }
        }
    }))
    .await;
    to_value(probes)
}

async fn handle_subscriptions(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    #[derive(serde::Deserialize)]
    struct Params {
        id: Option<String>,
        provider: Option<String>,
    }
    let params: Params = parse_params(params)?;
    let provider = params.provider.as_deref().unwrap_or("kiro");
    let configured = state.config.current().effective_providers();
    if provider == "all" {
        let mut providers = Vec::new();
        for candidate in configured.into_iter().filter(|candidate| candidate.enabled) {
            if candidate.kind == "kiro" {
                match kiro_subscriptions(state, params.id.as_deref()).await {
                    Ok(value) => providers.push(serde_json::json!({
                        "provider_id":candidate.id,
                        "supported":true,
                        "result":value
                    })),
                    Err(error) => providers.push(serde_json::json!({
                        "provider_id":candidate.id,
                        "supported":true,
                        "error":error.message
                    })),
                }
            } else {
                providers.push(serde_json::json!({
                    "provider_id":candidate.id,
                    "supported":false,
                    "reason":format!("{} does not expose authoritative subscription data", candidate.kind)
                }));
            }
        }
        return to_value(serde_json::json!({"providers":providers}));
    }
    let candidate = configured
        .iter()
        .find(|candidate| candidate.id == provider)
        .ok_or_else(|| RpcError::bad_params(format!("unknown provider {provider}")))?;
    if candidate.kind != "kiro" {
        return to_value(serde_json::json!({
            "provider_id":candidate.id,
            "supported":false,
            "reason":format!("{} does not expose authoritative subscription data", candidate.kind)
        }));
    }
    kiro_subscriptions(state, params.id.as_deref()).await
}

async fn kiro_subscriptions(state: &Arc<AppState>, id: Option<&str>) -> Handled {
    let account = state
        .pool()
        .snapshot()
        .await
        .into_iter()
        .find(|account| {
            account.enabled && id.is_none_or(|id| account.id == id || account.email == id)
        })
        .ok_or_else(|| RpcError::bad_params("no matching enabled account"))?;
    let subscriptions = state
        .kiro()
        .list_subscriptions(&account)
        .await
        .map_err(|error| {
            RpcError::internal(kproxy_translate::sanitize_error_message(&error.to_string()))
        })?;
    to_value(serde_json::json!({
        "provider_id":"kiro",
        "supported":true,
        "account_id":account.id,
        "plans":subscriptions.subscription_plans,
        "disclaimer":subscriptions.disclaimer
    }))
}

fn to_value<T: serde::Serialize>(value: T) -> Handled {
    serde_json::to_value(value).map_err(|error| RpcError::internal(error.to_string()))
}

fn parse_params<T: serde::de::DeserializeOwned>(params: serde_json::Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|error| RpcError::bad_params(error.to_string()))
}

fn compare_display_text(left: &str, right: &str) -> std::cmp::Ordering {
    left.to_ascii_lowercase()
        .cmp(&right.to_ascii_lowercase())
        .then_with(|| left.cmp(right))
}

#[derive(Debug, Default, serde::Deserialize)]
struct TimeRangeParams {
    since_secs: Option<u64>,
    start_secs: Option<i64>,
    end_secs: Option<i64>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct StatusParams {
    #[serde(flatten)]
    range: TimeRangeParams,
    provider: Option<String>,
}

#[derive(Debug)]
struct ResolvedTimeRange {
    start: Option<i64>,
    end: Option<i64>,
    filtered: bool,
    truncated: bool,
}

fn resolve_time_range(
    params: &TimeRangeParams,
    default_start: Option<i64>,
    clamp_start: Option<i64>,
) -> Result<ResolvedTimeRange, RpcError> {
    if params.since_secs.is_some() && (params.start_secs.is_some() || params.end_secs.is_some()) {
        return Err(RpcError::bad_params(
            "since_secs cannot be combined with start_secs or end_secs",
        ));
    }
    let now = now_secs();
    let filtered =
        params.since_secs.is_some() || params.start_secs.is_some() || params.end_secs.is_some();
    let requested_start = match params.since_secs {
        Some(seconds) => Some(now.saturating_sub(
            i64::try_from(seconds).map_err(|_| RpcError::bad_params("since_secs is too large"))?,
        )),
        None => params.start_secs,
    };
    let requested_end = if filtered || default_start.is_some() {
        Some(params.end_secs.unwrap_or(now).min(now))
    } else {
        None
    };
    if requested_start
        .zip(requested_end)
        .is_some_and(|(start, end)| start > end)
    {
        return Err(RpcError::bad_params(
            "statistics start time must not be after end time",
        ));
    }
    let mut start = requested_start.or(default_start);
    let mut truncated = false;
    if let Some(minimum) = clamp_start {
        if requested_end.is_some_and(|end| end < minimum) {
            return Err(RpcError::bad_params(
                "statistics range ends before the current daemon session started",
            ));
        }
        let unclamped = start.unwrap_or(minimum);
        truncated = unclamped < minimum;
        start = Some(unclamped.max(minimum));
    }
    Ok(ResolvedTimeRange {
        start,
        end: requested_end,
        filtered,
        truncated,
    })
}

async fn handle_status(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: StatusParams = if params.is_null() {
        StatusParams::default()
    } else {
        parse_params(params)?
    };
    let provider_filter = validated_provider_filter(state, params.provider.as_deref()).await?;
    let session_started_at = state.stats.session_started_at();
    let range = resolve_time_range(
        &params.range,
        Some(session_started_at),
        Some(session_started_at),
    )?;
    let config = state.config.current();
    let legacy_kiro_only = config.provider.is_empty();
    let mut services = state.proxy_services.views(&config.proxy_service).await;
    if let Some(provider) = provider_filter.as_deref() {
        services.retain(|service| {
            if service.allowed_providers.is_empty() {
                provider == "kiro"
            } else {
                service
                    .allowed_providers
                    .iter()
                    .any(|allowed| allowed == provider)
            }
        });
    }
    let running_services = services
        .iter()
        .filter(|service| service.running)
        .collect::<Vec<_>>();
    let listen = if running_services.is_empty() {
        "-".to_string()
    } else {
        running_services
            .iter()
            .map(|service| format!("{}:{}", service.host, service.port))
            .collect::<Vec<_>>()
            .join(",")
    };
    let account_list: ProviderAccountListResult = serde_json::from_value(
        providers::handle_account_list(
            state,
            serde_json::json!({"provider":provider_filter.as_deref().unwrap_or("all")}),
        )
        .await?,
    )
    .map_err(|error| RpcError::internal(error.to_string()))?;
    let total = account_list.accounts.len();
    let enabled = account_list
        .accounts
        .iter()
        .filter(|account| account.enabled)
        .count();
    let count_health = |health: &str| {
        account_list
            .accounts
            .iter()
            .filter(|account| account.health == health)
            .count()
    };
    let available_accounts = account_list
        .accounts
        .iter()
        .filter(|account| account.enabled && account.health == "available")
        .map(|account| account.provider_id.as_str())
        .collect::<BTreeSet<_>>();
    let hint = if total == 0 {
        if legacy_kiro_only {
            Some("无可用账号，请先添加：kproxy account import".to_string())
        } else {
            Some(format!(
                "提供源 {} 没有账号，请先使用 kproxy account add/import",
                provider_filter.as_deref().unwrap_or("all")
            ))
        }
    } else if enabled == 0 {
        Some("所有账号均已停用，代理无法服务请求".to_string())
    } else {
        None
    };
    let pool = state.pool();
    let mut stats = if range.filtered {
        state.stats.session_window(range.start, range.end, None)
    } else {
        // The default status view is the full process session and can use the
        // O(1) cumulative counter instead of walking every retained minute.
        state.stats.session_window(None, None, None)
    };
    if let Some(provider) = provider_filter.as_deref() {
        stats.retain_provider(provider);
    }
    let request_count = stats.total.requests;
    let success_rate = if request_count == 0 {
        0.0
    } else {
        stats.total.successes as f64 * 100.0 / request_count as f64
    };
    let average_latency_ms = stats
        .total
        .duration_ms
        .checked_div(stats.total.duration_samples)
        .unwrap_or(0);
    let (daily_credit_day, daily_credit_used, daily_credit_reserved, daily_credit_limit) =
        state.meter.daily_snapshot();
    let enabled_services = services.iter().filter(|service| service.enabled).count();
    let mut readiness_reasons = state.task_registry.readiness_issues(state);
    if enabled_services == 0 {
        readiness_reasons.push("no enabled proxy service is configured".to_string());
    } else if running_services.len() < enabled_services {
        readiness_reasons.push(format!(
            "only {} of {enabled_services} enabled proxy services are running",
            running_services.len()
        ));
    }
    let mut readiness_providers = BTreeSet::new();
    if legacy_kiro_only {
        if enabled_services > 0 && !available_accounts.contains("kiro") {
            readiness_reasons.push("no account is currently available".to_string());
        }
        readiness_providers.insert("kiro".to_string());
    } else {
        for service in services.iter().filter(|service| service.enabled) {
            let allowed = if service.allowed_providers.is_empty() {
                vec!["kiro"]
            } else {
                service
                    .allowed_providers
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
            };
            let relevant = allowed
                .into_iter()
                .filter(|provider| {
                    provider_filter
                        .as_deref()
                        .is_none_or(|selected| *provider == selected)
                })
                .collect::<Vec<_>>();
            readiness_providers.extend(relevant.iter().map(|provider| (*provider).to_string()));
            if !relevant
                .iter()
                .any(|provider| available_accounts.contains(*provider))
            {
                readiness_reasons.push(format!(
                    "proxy service {} has no available account in its provider scope",
                    service.name
                ));
            }
        }
    }
    for (provider, error) in &account_list.errors {
        if readiness_providers.contains(provider) {
            readiness_reasons.push(format!(
                "provider {provider} account status failed: {error}"
            ));
        }
    }
    if let Some(error) = state.meter.recovery_error() {
        readiness_reasons.push(format!("metering recovery required: {error}"));
    }
    let ready = readiness_reasons.is_empty();
    let descriptors = state
        .providers
        .descriptors()
        .await
        .into_iter()
        .filter(|descriptor| {
            provider_filter
                .as_deref()
                .is_none_or(|provider| descriptor.id.as_str() == provider)
        })
        .collect::<Vec<_>>();
    let provider_views = descriptors
        .into_iter()
        .map(|descriptor| {
            let accounts = account_list
                .accounts
                .iter()
                .filter(|account| account.provider_id == descriptor.id.as_str())
                .collect::<Vec<_>>();
            ProviderStatusView {
                id: descriptor.id.to_string(),
                kind: descriptor.kind,
                enabled: descriptor.enabled,
                status: descriptor.status,
                error: descriptor.error,
                account_total: accounts.len(),
                account_enabled: accounts.iter().filter(|account| account.enabled).count(),
                account_available: accounts
                    .iter()
                    .filter(|account| account.enabled && account.health == "available")
                    .count(),
            }
        })
        .collect();
    to_value(StatusResult {
        version: env!("CARGO_PKG_VERSION").to_string(),
        pid: std::process::id(),
        uptime_secs: state.uptime_secs(),
        listen,
        proxy_service_total: services.len(),
        proxy_service_running: running_services.len(),
        admin_socket: state.admin_socket().display().to_string(),
        account_total: total,
        account_enabled: enabled,
        account_available: count_health("available"),
        account_protected: count_health("low_credit"),
        account_cooling: count_health("cooling"),
        account_exhausted: count_health("exhausted"),
        account_banned: count_health("banned"),
        account_refreshing: count_health("refreshing"),
        active_requests: if legacy_kiro_only || provider_filter.as_deref() == Some("kiro") {
            pool.active().await
        } else {
            state.admission.current()
        },
        max_concurrent_requests: state.admission.maximum(),
        queued_requests: pool.queued(),
        request_count,
        success_rate,
        average_latency_ms,
        credits: stats.total.credits,
        stats_scope: "session".to_string(),
        stats_start: range.start,
        stats_end: range.end,
        stats_resolution_secs: 60,
        stats_truncated: range.truncated,
        daily_credit_day,
        daily_credit_used,
        daily_credit_reserved,
        daily_credit_limit,
        config_path: state.paths.config_file.display().to_string(),
        config_reloaded_at: state.config_reloaded_at(),
        hint,
        ready,
        readiness_reasons,
        provider_scope: provider_filter.unwrap_or_else(|| "all".into()),
        providers: provider_views,
    })
}

async fn handle_config_show(state: &Arc<AppState>) -> Handled {
    let raw = tokio::fs::read_to_string(&state.paths.config_file)
        .await
        .unwrap_or_default();
    let effective_json = serde_json::to_value(state.config.current().as_ref())
        .map_err(|error| RpcError::internal(error.to_string()))?;
    to_value(ConfigShowResult {
        path: state.paths.config_file.display().to_string(),
        raw,
        effective_json,
    })
}

fn handle_config_path(state: &Arc<AppState>) -> ConfigPathResult {
    let config = state.config.current();
    let log_base_path = resolved_log_base_path(state, &config.log);
    let log_directory = log_base_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    ConfigPathResult {
        config_file: state.paths.config_file.display().to_string(),
        accounts_file: state.paths.accounts_file.display().to_string(),
        daily_file: state.paths.daily_file.display().to_string(),
        stats_file: state.paths.stats_file.display().to_string(),
        admin_socket: state.admin_socket().display().to_string(),
        log_base_path: log_base_path.display().to_string(),
        log_directory: log_directory.display().to_string(),
    }
}

async fn handle_config_reload(state: &Arc<AppState>) -> Handled {
    // The CLI holds the cross-process config file lock for an enclosing
    // mutation/reload transaction. Serialize before reading so an older reload
    // cannot be applied after a newer snapshot.
    let _mutation = state.lock_config_mutation().await;
    let next = match load_config(&state.paths.config_file).await {
        Ok(config) => config,
        Err(error) => {
            return to_value(ConfigReloadResult {
                applied: false,
                error: Some(error.to_string()),
                needs_restart: vec![],
            });
        }
    };
    if let Err(error) = next.validate() {
        return to_value(ConfigReloadResult {
            applied: false,
            error: Some(error.to_string()),
            needs_restart: vec![],
        });
    }

    let current = state.config.current();
    let (next, needs_restart) = merge_hot_reload(&current, next);
    let needs_restart = needs_restart
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();
    for field in &needs_restart {
        warn!(field = %field, "configuration field requires restart");
    }
    if let Err(error) = state.apply_config_transaction_locked(&next).await {
        warn!(%error, "configuration reload rolled back");
        return to_value(ConfigReloadResult {
            applied: false,
            error: Some(error),
            needs_restart,
        });
    }
    state.mark_config_reloaded(now_secs());
    to_value(ConfigReloadResult {
        applied: true,
        error: None,
        needs_restart,
    })
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn summarize(account: &Account, pool: &PoolConfig) -> AccountSummary {
    let mut tags = account.tags.clone();
    tags.sort();
    AccountSummary {
        id: account.id.clone(),
        email: account.email.clone(),
        label: account.label.clone(),
        enabled: account.enabled,
        health: None,
        tags,
        subscription: account
            .subscription
            .as_ref()
            .map(|subscription| format!("{:?}", subscription.kind)),
        credit_current: account.usage.as_ref().map(|usage| usage.current),
        credit_limit: account
            .usage
            .as_ref()
            .map(|usage| effective_credit_limit(usage, pool)),
        token_expires_at: account.credentials.expires_at,
        credit_exhausted: account.credit_exhausted,
    }
}

mod accounts;

#[cfg(test)]
use accounts::{authenticated_sso_user_id, sso_identities_match};
use accounts::{
    compare_account_identity, effective_account_health, handle_account_add_sso,
    handle_account_import, handle_account_list, handle_account_probe, handle_account_refresh,
    handle_account_remove, handle_account_reset_health, handle_account_services,
    handle_account_set_enabled, handle_account_show, handle_account_tag,
    handle_regenerate_machine_id,
};

#[derive(serde::Deserialize)]
struct PoolParams {
    #[serde(default = "default_pool_model")]
    model: String,
}

#[derive(serde::Serialize)]
struct PoolAccountView {
    #[serde(flatten)]
    score: kproxy_pool::ScoreExplanation,
    account_name: String,
}

fn default_pool_model() -> String {
    "minimax-m2.5".into()
}

async fn handle_pool(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: PoolParams = if params.is_null() {
        PoolParams {
            model: default_pool_model(),
        }
    } else {
        parse_params(params)?
    };
    let pool = state.pool();
    let account_names = pool
        .snapshot()
        .await
        .into_iter()
        .map(|account| (account.id.clone(), account.display_name().to_owned()))
        .collect::<HashMap<_, _>>();
    let scores = pool
        .explain(&params.model)
        .await
        .into_iter()
        .map(|score| PoolAccountView {
            account_name: account_names
                .get(&score.account_id)
                .cloned()
                .unwrap_or_default(),
            score,
        })
        .collect::<Vec<_>>();
    let pool_config = state.runtime_config_snapshot().pool;
    to_value(serde_json::json!({
        "model": params.model,
        "queued": pool.queued(),
        "accounts": scores,
        "scoring": {
            "weight_active": pool_config.balance.weight_active,
            "weight_credit": pool_config.balance.weight_credit,
            "weight_idle": pool_config.balance.weight_idle,
            "max_concurrent_per_account": pool_config.max_concurrent_per_account,
            "idle_window_ms": pool_config.balance.idle_window_ms,
        }
    }))
}

#[derive(serde::Deserialize, Default)]
struct StatsParams {
    #[serde(default)]
    detail: bool,
    recent: Option<usize>,
    #[serde(flatten)]
    range: TimeRangeParams,
    by: Option<String>,
    account: Option<String>,
    provider: Option<String>,
    level: Option<String>,
}

async fn handle_stats(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let mut params: StatsParams = if params.is_null() {
        StatsParams::default()
    } else {
        parse_params(params)?
    };
    if params.provider.as_deref() == Some("all") {
        params.provider = None;
    }
    if let Some(provider) = params.provider.as_deref() {
        let provider = kproxy_core::provider::ProviderId::parse(provider)
            .map_err(|error| RpcError::bad_params(error.to_string()))?;
        if !state
            .providers
            .descriptors()
            .await
            .iter()
            .any(|descriptor| descriptor.id == provider)
        {
            return Err(RpcError::bad_params(format!(
                "provider not found: {provider}"
            )));
        }
    }
    let range = resolve_time_range(&params.range, None, None)?;
    if params.by.as_deref() == Some("apikey") {
        if !params.detail {
            return Err(RpcError::bad_params("stats grouping requires detail=true"));
        }
        if range.filtered {
            return Err(RpcError::bad_params(
                "time ranges are not supported for API key usage grouping; use `kproxy apikey history`",
            ));
        }
        let by_apikey = if let Some(provider) = params.provider.as_deref() {
            handle_apikey_list(state, serde_json::json!({"provider":provider})).await?
        } else {
            serde_json::to_value(state.meter.list())
                .map_err(|error| RpcError::internal(error.to_string()))?
        };
        return to_value(serde_json::json!({
            "scope":"persistent",
            "range":{"start":null,"end":null,"resolution_secs":60,"truncated":false},
            "by_apikey":by_apikey
        }));
    }
    let available_start = state.stats.persistent_history_started_at();
    let prefix_complete = state.stats.persistent_history_prefix_complete();
    let requested_recent = if params.detail {
        params.recent
    } else {
        // Summary-only queries never serialize request diagnostics. Avoid even
        // cloning references for the bounded diagnostic ring.
        Some(0)
    };
    let mut stats = state
        .stats
        .window_between(range.start, range.end, requested_recent)
        .await
        .map_err(|error| RpcError::internal(format!("read statistics history: {error}")))?;
    if let Some(provider) = params.provider.as_deref() {
        stats.retain_provider(provider);
    }
    let missing_ranges = if range.filtered {
        stats
            .history_gaps
            .iter()
            .filter(|gap| {
                range.start.is_none_or(|start| gap.end >= start)
                    && range.end.is_none_or(|end| gap.start <= end)
            })
            .cloned()
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let history_complete = prefix_complete && stats.history_gaps.is_empty();
    let prefix_truncated = !prefix_complete
        && available_start
            .is_none_or(|available| range.start.is_none_or(|start| start < available));
    let truncated = range.filtered && (prefix_truncated || !missing_ranges.is_empty());
    if params.account.is_some() || params.provider.is_some() || params.level.is_some() {
        stats.recent_requests.retain(|request| {
            params
                .account
                .as_deref()
                .is_none_or(|account| request.account_id == account)
                && params.provider.as_deref().is_none_or(|provider| {
                    let request_provider = if request.provider_id.is_empty() {
                        "kiro"
                    } else {
                        &request.provider_id
                    };
                    request_provider == provider
                })
                && params.level.as_deref().is_none_or(|level| match level {
                    "error" => request.status >= 500,
                    "warn" => request.status >= 400,
                    _ => true,
                })
        });
    }
    let percentiles = stats.percentiles();
    let average_ms = stats
        .total
        .duration_ms
        .checked_div(stats.total.duration_samples)
        .unwrap_or_else(|| {
            stats
                .latencies_ms
                .iter()
                .fold(0u64, |total, value| total.saturating_add(*value))
                .checked_div(stats.latencies_ms.len() as u64)
                .unwrap_or(0)
        });
    let range_json = serde_json::json!({
        "start":range.start,
        "end":range.end,
        "available_start":available_start,
        "history_complete":history_complete,
        "prefix_truncated":range.filtered && prefix_truncated,
        "missing_ranges":missing_ranges,
        "resolution_secs":60,
        "truncated":truncated
    });
    if !params.detail {
        return to_value(serde_json::json!({
            "scope":"persistent",
            "range":range_json,
            "summary":stats.total,
            "latency":{"average_ms":average_ms,"p50_ms":percentiles.0,"p95_ms":percentiles.1,"p99_ms":percentiles.2}
        }));
    }
    let grouped = match params.by.as_deref() {
        Some("provider") => serde_json::to_value(&stats.by_provider),
        Some("account") => serde_json::to_value(&stats.by_account),
        Some("endpoint") => serde_json::to_value(&stats.by_endpoint),
        Some("model") => serde_json::to_value(&stats.by_model),
        Some(other) => {
            return Err(RpcError::bad_params(format!(
                "unknown stats group: {other}"
            )))
        }
        None => serde_json::to_value(&stats.total),
    }
    .map_err(|error| RpcError::internal(error.to_string()))?;
    to_value(serde_json::json!({
        "scope":"persistent",
        "range":range_json,
        "stats":stats,
        "grouped":grouped,
        "latency":{"average_ms":average_ms,"p50_ms":percentiles.0,"p95_ms":percentiles.1,"p99_ms":percentiles.2}
    }))
}

#[derive(serde::Deserialize)]
struct TaskRunParams {
    name: String,
    provider: Option<String>,
}

async fn handle_task_run(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: TaskRunParams = parse_params(params)?;
    if params.name == "model_cache_refresh" {
        let result = crate::tasks::refresh_models_for(state, params.provider.as_deref())
            .await
            .map_err(|error| RpcError::bad_params(error.to_string()))?;
        state.task_registry.record(&params.name, result.clone());
        return to_value(serde_json::json!({
            "name":params.name,
            "provider":params.provider.unwrap_or_else(|| "all".into()),
            "result":result
        }));
    }
    if params
        .provider
        .as_deref()
        .is_some_and(|provider| provider != "all")
    {
        return Err(RpcError::bad_params(
            "--provider is only supported for model_cache_refresh",
        ));
    }
    crate::tasks::run_named(state, &params.name)
        .await
        .map_err(|error| RpcError::bad_params(error.to_string()))
}

async fn handle_models(state: &Arc<AppState>) -> Handled {
    let config = state.config.current();
    if !config.models.dynamic_discovery {
        let mut models = crate::http::fallback_models(&config);
        sort_models_for_display(&mut models);
        return to_value(models);
    }
    let (mut cached, fresh) = state.models.get(config.models.cache_ttl_ms);
    if fresh {
        sort_models_for_display(&mut cached);
        return to_value(cached);
    }
    let account = state
        .pool()
        .snapshot()
        .await
        .into_iter()
        .find(|account| account.enabled)
        .ok_or_else(|| RpcError::bad_params("no enabled account for model discovery"))?;
    let models = state
        .kiro()
        .list_models(&account)
        .await
        .map_err(|error| RpcError::internal(error.to_string()))?;
    if let Some(runtime) = state.pool().get(&account.id).await {
        runtime
            .set_supported_models(models.iter().map(|model| model.model_id.clone()))
            .await;
    }
    state.models.finish_refresh(models.clone());
    let mut output = models;
    sort_models_for_display(&mut output);
    to_value(output)
}

#[derive(serde::Deserialize)]
struct ModelResolveParams {
    model: String,
    api_key: Option<String>,
}

async fn handle_model_resolve(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: ModelResolveParams = parse_params(params)?;
    let input_model = params.model.trim();
    if input_model.is_empty() {
        return Err(RpcError::bad_params("model must not be empty"));
    }
    let config = state.config.current();
    let api_key_id = params
        .api_key
        .as_deref()
        .map(|selector| {
            config
                .api_key
                .iter()
                .find(|key| key.id.as_deref() == Some(selector) || key.name == selector)
                .and_then(|key| key.id.clone())
                .ok_or_else(|| RpcError::bad_params(format!("API key not found: {selector}")))
        })
        .transpose()?;
    let initial_route = kproxy_translate::model::map_model(
        input_model,
        &config.model_mapping,
        api_key_id.as_deref(),
        None,
        "",
    );
    let pool = state.pool();
    let mut accounts = pool
        .snapshot()
        .await
        .into_iter()
        .filter(|account| account.enabled)
        .collect::<Vec<_>>();
    accounts.sort_by(|left, right| {
        compare_account_identity(
            left.display_name(),
            &left.id,
            right.display_name(),
            &right.id,
        )
    });
    if accounts.is_empty() {
        return Err(RpcError::bad_params(
            "no enabled account for model resolution",
        ));
    }

    let mut results = Vec::with_capacity(accounts.len());
    for account in accounts {
        let account_name = account.display_name().to_owned();
        let health = effective_account_health(&pool, &account, &config.pool).await;
        let schedulable = health == "available";
        let remaining = account
            .usage
            .as_ref()
            .and_then(|usage| remaining_credit_percent(usage, &config.pool));
        let route = kproxy_translate::model::map_model(
            input_model,
            &config.model_mapping,
            api_key_id.as_deref(),
            remaining,
            "",
        );
        let runtime = pool.get(&account.id).await;
        let cached_runtime = if let Some(runtime) = runtime.as_ref() {
            runtime.has_model_cache().await.then_some(runtime)
        } else {
            None
        };
        let cache_loaded = cached_runtime.is_some();
        let (available_models, model_source) = if let Some(runtime) = cached_runtime {
            (runtime.supported_models().await, "account_cache")
        } else {
            (
                kproxy_kiro::static_models_for_account(&account)
                    .into_iter()
                    .map(|model| model.model_id)
                    .collect(),
                "static_catalog",
            )
        };
        let mut resolved_model =
            kproxy_translate::model::resolve_dynamic_model(&route.mapped, &available_models);
        let mut used_default = false;
        if resolved_model.is_none()
            && cache_loaded
            && !config.features.default_model_id.trim().is_empty()
        {
            resolved_model = kproxy_translate::model::resolve_dynamic_model(
                &config.features.default_model_id,
                &available_models,
            );
            used_default = resolved_model.is_some();
        }
        let error = resolved_model.is_none().then(|| {
            if config.features.default_model_id.trim().is_empty() || !cache_loaded {
                format!(
                    "model '{}' is not present in this account's {model_source}",
                    route.mapped
                )
            } else {
                format!(
                    "model '{}' and default model '{}' are not present in this account's model cache",
                    route.mapped, config.features.default_model_id
                )
            }
        });
        results.push(ModelResolutionAccount {
            account_id: account.id,
            account_name,
            health,
            schedulable,
            mapped_model: route.mapped,
            mapping_rule: route.rule,
            resolved_model,
            used_default,
            model_source: model_source.into(),
            available_model_count: available_models.len(),
            error,
        });
    }

    let possible_models = results
        .iter()
        .filter(|account| account.schedulable)
        .filter_map(|account| account.resolved_model.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let resolved_model = (possible_models.len() == 1).then(|| possible_models[0].clone());
    let matched_accounts = results
        .iter()
        .filter(|account| account.schedulable && account.resolved_model.is_some())
        .count();
    let schedulable_accounts = results.iter().filter(|account| account.schedulable).count();
    to_value(ModelResolutionResult {
        input_model: input_model.into(),
        mapped_model: initial_route.mapped,
        mapping_rule: initial_route.rule,
        resolved_model,
        possible_models,
        matched_accounts,
        total_accounts: schedulable_accounts,
        accounts: results,
    })
}

fn sort_models_for_display(models: &mut [kproxy_kiro::ModelInfo]) {
    models.sort_by(|left, right| {
        compare_display_text(&left.model_id, &right.model_id)
            .then_with(|| compare_display_text(&left.model_name, &right.model_name))
    });
}

#[derive(serde::Deserialize)]
struct WebhookTestParams {
    name: Option<String>,
}

fn handle_webhook_test(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    use kproxy_notify::{WebhookEvent, WebhookEventKind};
    let params: WebhookTestParams = parse_params(params)?;
    let event = WebhookEvent::new(
        WebhookEventKind::Test,
        "KProxy 告警测试",
        "- **状态：** 测试消息\n- **说明：** Webhook 配置和消息投递链路可用",
    );
    let notifier = state.notifier();
    let queued = notifier.emit_test(params.name.as_deref(), event);
    if queued == 0 {
        return Err(RpcError::bad_params(format!(
            "webhook not found, disabled, or not subscribed: {}",
            params.name.as_deref().unwrap_or("all")
        )));
    }
    to_value(
        serde_json::json!({"name":params.name.unwrap_or_else(|| "all".into()),"queued":queued}),
    )
}

#[derive(serde::Deserialize)]
struct WebhookLogsParams {
    #[serde(default = "default_webhook_tail")]
    tail: usize,
}

fn default_webhook_tail() -> usize {
    50
}

fn handle_webhook_logs(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: WebhookLogsParams = if params.is_null() {
        WebhookLogsParams {
            tail: default_webhook_tail(),
        }
    } else {
        parse_params(params)?
    };
    to_value(state.notifier().logs(params.tail))
}

async fn handle_apikey_list(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    #[derive(Default, serde::Deserialize)]
    struct Params {
        provider: Option<String>,
    }
    let params: Params = if params.is_null() {
        Params::default()
    } else {
        parse_params(params)?
    };
    let provider = validated_provider_filter(state, params.provider.as_deref()).await?;
    let mut keys = state.meter.list();
    if let Some(provider) = provider.as_deref() {
        keys.retain(|key| {
            if key.allowed_providers.is_empty() {
                provider == "kiro"
            } else {
                key.allowed_providers
                    .iter()
                    .any(|allowed| allowed == provider)
            }
        });
        for key in &mut keys {
            key.usage.retain_provider(provider);
        }
    }
    to_value(keys)
}

async fn handle_service_list(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    #[derive(Default, serde::Deserialize)]
    struct Params {
        provider: Option<String>,
    }
    let params: Params = if params.is_null() {
        Params::default()
    } else {
        parse_params(params)?
    };
    let provider = validated_provider_filter(state, params.provider.as_deref()).await?;
    let config = state.config.current();
    let mut services = state.proxy_services.views(&config.proxy_service).await;
    if let Some(provider) = provider.as_deref() {
        services.retain(|service| {
            if service.allowed_providers.is_empty() {
                provider == "kiro"
            } else {
                service
                    .allowed_providers
                    .iter()
                    .any(|allowed| allowed == provider)
            }
        });
    }
    services.sort_by(|left, right| {
        compare_display_text(&left.name, &right.name).then_with(|| left.id.cmp(&right.id))
    });
    to_value(ProxyServiceListResult { services })
}

async fn validated_provider_filter(
    state: &Arc<AppState>,
    provider: Option<&str>,
) -> Result<Option<String>, RpcError> {
    let Some(provider) = provider.filter(|provider| *provider != "all") else {
        return Ok(None);
    };
    let id = kproxy_core::provider::ProviderId::parse(provider)
        .map_err(|error| RpcError::bad_params(error.to_string()))?;
    if !state
        .providers
        .descriptors()
        .await
        .iter()
        .any(|descriptor| descriptor.id == id)
    {
        return Err(RpcError::bad_params(format!("provider not found: {id}")));
    }
    Ok(Some(id.to_string()))
}

fn handle_service_apikeys(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: ProxyServiceApiKeysParams = parse_params(params)?;
    let selector = params.service.trim();
    if selector.is_empty() {
        return Err(RpcError::bad_params("service ID or name must not be empty"));
    }
    let config = state.config.current();
    let service = config
        .proxy_service
        .iter()
        .find(|service| service.id == selector || service.name == selector)
        .ok_or_else(|| RpcError::bad_params(format!("proxy service not found: {selector}")))?;
    let mut api_keys = service
        .api_key_ids
        .iter()
        .filter_map(|key_id| {
            config
                .api_key
                .iter()
                .find(|key| key.id.as_deref() == Some(key_id.as_str()))
                .map(|key| {
                    let policy = kproxy_core::config::resolve_user_agent_check_policy(
                        config.server.enforce_user_agent_check,
                        service.skip_user_agent_check,
                        key.skip_user_agent_check,
                    );
                    ProxyServiceApiKeyView {
                        id: key_id.clone(),
                        name: key.name.clone(),
                        format: match key.format {
                            ApiKeyFormat::Sk => "sk",
                            ApiKeyFormat::Simple => "simple",
                            ApiKeyFormat::Token => "token",
                        }
                        .to_string(),
                        enabled: key.enabled,
                        skip_user_agent_check: key.skip_user_agent_check,
                        user_agent_check_enforced: policy.enforced(),
                        user_agent_check_reason: policy.reason().to_string(),
                        credits_limit: key.credits_limit,
                        key: params.show_secret.then(|| key.key.clone()),
                    }
                })
        })
        .collect::<Vec<_>>();
    api_keys.sort_by(|left, right| {
        compare_display_text(&left.name, &right.name).then_with(|| left.id.cmp(&right.id))
    });
    to_value(ProxyServiceApiKeysResult {
        service_id: service.id.clone(),
        service_name: service.name.clone(),
        api_keys,
    })
}

async fn handle_service_accounts(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: ProxyServiceAccountsParams = parse_params(params)?;
    let selector = params.service.trim();
    if selector.is_empty() {
        return Err(RpcError::bad_params("service ID or name must not be empty"));
    }
    let config = state.config.current();
    let service = config
        .proxy_service
        .iter()
        .find(|service| service.id == selector || service.name == selector)
        .ok_or_else(|| RpcError::bad_params(format!("proxy service not found: {selector}")))?;
    service_accounts_result(state, service).await
}

async fn handle_service_account_change(
    state: &Arc<AppState>,
    params: serde_json::Value,
    add: bool,
) -> Handled {
    let params: ProxyServiceAccountChangeParams = parse_params(params)?;
    let selector = params.service.trim();
    if selector.is_empty() {
        return Err(RpcError::bad_params("service ID or name must not be empty"));
    }
    if params.accounts.is_empty() {
        return Err(RpcError::bad_params("at least one account is required"));
    }

    let file_lock = kproxy_store::atomic::lock_file_exclusive(&state.paths.config_file)
        .await
        .map_err(|error| RpcError::internal(format!("lock config: {error}")))?;
    let config_mutation = state.lock_config_mutation().await;
    let account_transaction = state
        .lock_account_storage()
        .await
        .map_err(|error| RpcError::internal(format!("lock accounts: {error}")))?;
    let mut durable_accounts = AccountStore::load(&state.paths.accounts_file)
        .await
        .map_err(|error| RpcError::internal(format!("load accounts: {error}")))?;
    state.configure_account_store(&mut durable_accounts);
    let disk_accounts = serde_json::to_value(durable_accounts.all())
        .map_err(|error| RpcError::internal(format!("serialize disk accounts: {error}")))?;
    let memory_accounts = state
        .with_accounts(|store| serde_json::to_value(store.all()))
        .map_err(|error| RpcError::internal(format!("serialize runtime accounts: {error}")))?;
    if disk_accounts != memory_accounts {
        state
            .apply_account_file_reload(durable_accounts)
            .await
            .map_err(RpcError::bad_params)?;
    }
    let accounts = state.with_accounts(|store| store.all().to_vec());
    let mut resolved_ids = Vec::new();
    for reference in params.accounts {
        let reference = reference.trim();
        if reference.is_empty() {
            return Err(RpcError::bad_params("account references must not be empty"));
        }
        let account = accounts
            .iter()
            .find(|account| {
                account.id == reference || account.email.eq_ignore_ascii_case(reference)
            })
            .ok_or_else(|| RpcError::bad_params(format!("account not found: {reference}")))?;
        if !resolved_ids.contains(&account.id) {
            resolved_ids.push(account.id.clone());
        }
    }

    let previous = state.config.current().as_ref().clone();
    let mut next = previous.clone();
    let service = next
        .proxy_service
        .iter_mut()
        .find(|service| service.id == selector || service.name == selector)
        .ok_or_else(|| RpcError::bad_params(format!("proxy service not found: {selector}")))?;
    let original_service = service.clone();
    if add {
        service
            .excluded_account_ids
            .retain(|account_id| !resolved_ids.contains(account_id));
        for account_id in &resolved_ids {
            if !service.account_ids.contains(account_id) {
                service.account_ids.push(account_id.clone());
            }
        }
    } else {
        service
            .account_ids
            .retain(|account_id| !resolved_ids.contains(account_id));
        for account_id in &resolved_ids {
            if !service.excluded_account_ids.contains(account_id) {
                service.excluded_account_ids.push(account_id.clone());
            }
        }
    }
    service.account_ids.sort();
    service.excluded_account_ids.sort();
    let service_id = service.id.clone();
    let changed = service != &original_service;

    if !changed {
        let service = service.clone();
        drop(account_transaction);
        drop(config_mutation);
        drop(file_lock);
        return service_accounts_result(state, &service).await;
    }

    next.validate()
        .map_err(|error| RpcError::bad_params(error.to_string()))?;
    let raw = tokio::fs::read_to_string(&state.paths.config_file)
        .await
        .map_err(|error| RpcError::internal(error.to_string()))?;
    persist_service_config_locked(state, &raw, &next).await?;
    let service = next
        .proxy_service
        .iter()
        .find(|service| service.id == service_id)
        .cloned()
        .ok_or_else(|| RpcError::internal("updated proxy service is missing"))?;
    drop(account_transaction);
    drop(config_mutation);
    drop(file_lock);
    service_accounts_result(state, &service).await
}

async fn service_accounts_result(state: &Arc<AppState>, service: &ProxyServiceConfig) -> Handled {
    let source = state.with_accounts(|store| store.all().to_vec());
    let pool = state.pool();
    let pool_config = state.runtime_config_snapshot().pool;
    let mut accounts = Vec::new();
    for account in source
        .iter()
        .filter(|account| service.includes_account(account))
    {
        let mut summary = summarize(account, &pool_config);
        summary.health = Some(effective_account_health(&pool, account, &pool_config).await);
        accounts.push(summary);
    }
    accounts.sort_by(|left, right| {
        compare_account_identity(&left.email, &left.id, &right.email, &right.id)
    });
    to_value(ProxyServiceAccountsResult {
        service_id: service.id.clone(),
        service_name: service.name.clone(),
        account_tag: service
            .account_tag
            .as_ref()
            .and_then(ServiceAccountTags::single_tag)
            .map(str::to_owned),
        account_tags: service.selected_account_tags().to_vec(),
        uses_global_pool: service.uses_global_account_pool(),
        account_ids: service.account_ids.clone(),
        excluded_account_ids: service.excluded_account_ids.clone(),
        accounts,
    })
}

async fn persist_service_config_locked(
    state: &Arc<AppState>,
    raw: &str,
    next: &Config,
) -> Result<(), RpcError> {
    let output = render_service_config_update(raw, next)?;
    kproxy_store::atomic::write_bytes_atomically(
        &state.paths.config_file,
        output.as_bytes(),
        Some(0o600),
    )
    .await
    .map_err(|error| RpcError::internal(error.to_string()))?;
    if let Err(error) = state.apply_config_transaction_locked(next).await {
        kproxy_store::atomic::write_bytes_atomically(
            &state.paths.config_file,
            raw.as_bytes(),
            Some(0o600),
        )
        .await
        .map_err(|rollback_error| {
            RpcError::internal(format!(
                "config apply failed ({error}); disk rollback failed: {rollback_error}"
            ))
        })?;
        return Err(RpcError::bad_params(format!(
            "proxy service update rejected: {error}"
        )));
    }
    state.mark_config_reloaded(now_secs());
    Ok(())
}

async fn handle_service_create(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: ProxyServiceCreateParams = parse_params(params)?;
    let name = params.name.trim();
    if name.is_empty() {
        return Err(RpcError::bad_params("service name must not be empty"));
    }
    let _file_lock = kproxy_store::atomic::lock_file_exclusive(&state.paths.config_file)
        .await
        .map_err(|error| RpcError::internal(format!("lock config: {error}")))?;
    let _mutation = state.lock_config_mutation().await;
    let previous = state.config.current().as_ref().clone();
    if previous
        .proxy_service
        .iter()
        .any(|service| service.name == name)
    {
        return Err(RpcError::bad_params(format!(
            "proxy service name already exists: {name}"
        )));
    }
    let account_tag = match params.account_tag {
        Some(selection) => {
            let mut tags = selection
                .tags()
                .iter()
                .map(|tag| tag.trim().to_owned())
                .collect::<Vec<_>>();
            if tags.is_empty() || tags.iter().any(String::is_empty) {
                return Err(RpcError::bad_params("account tags must not be empty"));
            }
            tags.sort();
            tags.dedup();
            ServiceAccountTags::from_tags(tags)
        }
        None => None,
    };

    let format_name = params.api_key_format.as_deref().unwrap_or("sk");
    let format = match format_name {
        "sk" => ApiKeyFormat::Sk,
        "token" => ApiKeyFormat::Token,
        "simple" => ApiKeyFormat::Simple,
        other => {
            return Err(RpcError::bad_params(format!(
                "unsupported API key format: {other}"
            )))
        }
    };
    let key = generate_api_key(format);
    let key_id = api_key_id(&key);
    let key_name = params
        .api_key_name
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| format!("{name}-default"));
    let allowed_providers = params.allowed_providers;
    let default_provider = params
        .default_provider
        .filter(|value| !value.trim().is_empty())
        .or_else(|| allowed_providers.first().cloned())
        .unwrap_or_else(|| "kiro".into());
    let service = ProxyServiceConfig {
        id: format!("svc_{}", uuid::Uuid::new_v4().simple()),
        name: name.to_string(),
        host: params.host.unwrap_or_else(|| previous.server.host.clone()),
        port: params.port.unwrap_or(previous.server.port),
        enabled: true,
        skip_user_agent_check: params.skip_user_agent_check,
        api_key_ids: vec![key_id.clone()],
        account_tag,
        account_ids: Vec::new(),
        excluded_account_ids: Vec::new(),
        default_provider,
        allowed_providers: allowed_providers.clone(),
        created_at: now_secs(),
    };
    let key_config = ApiKeyConfig {
        id: Some(key_id.clone()),
        name: key_name.clone(),
        key: key.clone(),
        format,
        enabled: true,
        skip_user_agent_check: false,
        credits_limit: None,
        allowed_providers,
        allowed_models: Vec::new(),
    };

    let mut next = previous.clone();
    next.api_key.push(key_config);
    next.proxy_service.push(service.clone());
    next.validate()
        .map_err(|error| RpcError::bad_params(error.to_string()))?;

    let raw = tokio::fs::read_to_string(&state.paths.config_file)
        .await
        .map_err(|error| RpcError::internal(error.to_string()))?;
    let output = render_service_config_update(&raw, &next)?;
    kproxy_store::atomic::write_bytes_atomically(
        &state.paths.config_file,
        output.as_bytes(),
        Some(0o600),
    )
    .await
    .map_err(|error| RpcError::internal(error.to_string()))?;

    state.apply_runtime_config(&next);
    state.config.replace(next.clone());
    state.mark_config_reloaded(now_secs());
    let failures = state.reconcile_proxy_services(&next).await;
    if let Some((_, error)) = failures
        .into_iter()
        .find(|(service_id, _)| service_id == &service.id)
    {
        let rollback_write = kproxy_store::atomic::write_bytes_atomically(
            &state.paths.config_file,
            raw.as_bytes(),
            Some(0o600),
        )
        .await;
        state.apply_runtime_config(&previous);
        state.config.replace(previous.clone());
        let _rollback_failures = state.reconcile_proxy_services(&previous).await;
        if let Err(rollback_error) = rollback_write {
            return Err(RpcError::internal(format!(
                "proxy service failed to start ({error}); config rollback failed: {rollback_error}"
            )));
        }
        return Err(RpcError::bad_params(format!(
            "proxy service failed to start: {error}"
        )));
    }

    let view = state
        .proxy_services
        .views(&next.proxy_service)
        .await
        .into_iter()
        .find(|view| view.id == service.id)
        .ok_or_else(|| RpcError::internal("created proxy service is missing"))?;
    to_value(ProxyServiceCreateResult {
        service: view,
        api_key: CreatedApiKey {
            id: key_id,
            name: key_name,
            key,
        },
    })
}

async fn handle_service_delete(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: ProxyServiceDeleteParams = parse_params(params)?;
    let selector = params.service.trim();
    if selector.is_empty() {
        return Err(RpcError::bad_params("service ID or name must not be empty"));
    }

    let _file_lock = kproxy_store::atomic::lock_file_exclusive(&state.paths.config_file)
        .await
        .map_err(|error| RpcError::internal(format!("lock config: {error}")))?;
    let _mutation = state.lock_config_mutation().await;
    let previous = state.config.current().as_ref().clone();
    let index = previous
        .proxy_service
        .iter()
        .position(|service| service.id == selector || service.name == selector)
        .ok_or_else(|| RpcError::bad_params(format!("proxy service not found: {selector}")))?;

    let mut next = previous.clone();
    let removed = next.proxy_service.remove(index);
    let (deleted_api_key_ids, retained_api_key_ids) =
        remove_unshared_service_api_keys(&mut next, &removed);
    next.validate()
        .map_err(|error| RpcError::bad_params(error.to_string()))?;
    let raw = tokio::fs::read_to_string(&state.paths.config_file)
        .await
        .map_err(|error| RpcError::internal(error.to_string()))?;
    let output = render_service_config_update(&raw, &next)?;
    kproxy_store::atomic::write_bytes_atomically(
        &state.paths.config_file,
        output.as_bytes(),
        Some(0o600),
    )
    .await
    .map_err(|error| RpcError::internal(error.to_string()))?;

    state.apply_runtime_config(&next);
    state.config.replace(next.clone());
    state.mark_config_reloaded(now_secs());
    let failures = state.reconcile_proxy_services(&next).await;
    for (service_id, error) in failures {
        warn!(%service_id, %error, "proxy service failed after service deletion");
    }

    to_value(ProxyServiceDeleteResult {
        service_id: removed.id,
        service_name: removed.name,
        deleted_api_key_ids,
        retained_api_key_ids,
    })
}

fn render_service_config_update(raw: &str, next: &Config) -> Result<String, RpcError> {
    let before = raw
        .parse::<toml::Value>()
        .map_err(|error| RpcError::internal(format!("parse config: {error}")))?;
    let mut after = before.clone();
    let table = after
        .as_table_mut()
        .ok_or_else(|| RpcError::internal("config root must be a TOML table"))?;
    table.insert(
        "api_key".into(),
        toml::Value::try_from(&next.api_key)
            .map_err(|error| RpcError::internal(format!("serialize API keys: {error}")))?,
    );
    table.insert(
        "proxy_service".into(),
        toml::Value::try_from(&next.proxy_service)
            .map_err(|error| RpcError::internal(format!("serialize proxy services: {error}")))?,
    );
    kproxy_store::config_update::render_update_preserving_comments(raw, &before, &after)
        .map_err(|error| RpcError::internal(format!("update config: {error}")))
}

fn remove_unshared_service_api_keys(
    config: &mut Config,
    removed: &ProxyServiceConfig,
) -> (Vec<String>, Vec<String>) {
    let is_still_referenced = |key_id: &str| {
        config
            .proxy_service
            .iter()
            .any(|service| service.api_key_ids.iter().any(|id| id == key_id))
    };
    let retained = removed
        .api_key_ids
        .iter()
        .filter(|key_id| is_still_referenced(key_id))
        .cloned()
        .collect::<Vec<_>>();
    let deleted = removed
        .api_key_ids
        .iter()
        .filter(|key_id| !is_still_referenced(key_id))
        .cloned()
        .collect::<Vec<_>>();
    config.api_key.retain(|key| {
        key.id
            .as_ref()
            .is_none_or(|key_id| !deleted.contains(key_id))
    });
    (deleted, retained)
}

fn generate_api_key(format: ApiKeyFormat) -> String {
    let mut bytes = [0_u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    let random = bytes.iter().fold(String::new(), |mut output, byte| {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
        output
    });
    match format {
        ApiKeyFormat::Sk => format!("sk-{random}"),
        ApiKeyFormat::Token => format!("token_{random}"),
        ApiKeyFormat::Simple => random,
    }
}

fn api_key_id(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    digest[..8]
        .iter()
        .fold(String::from("ak_"), |mut output, byte| {
            use std::fmt::Write as _;
            let _ = write!(output, "{byte:02x}");
            output
        })
}

async fn handle_apikey_reset(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: AccountRefParams = parse_params(params)?;
    match state.meter.reset_usage(&params.id).await {
        Ok(true) => to_value(serde_json::json!({"id":params.id,"reset":true})),
        Ok(false) => Err(RpcError::bad_params(format!(
            "API key not found: {}",
            params.id
        ))),
        Err(error) => Err(RpcError::internal(error.to_string())),
    }
}

async fn persist_pool_snapshot(state: &Arc<AppState>) -> Result<(), RpcError> {
    state
        .persist_runtime_accounts()
        .await
        .map_err(|error| RpcError::internal(error.to_string()))
}

#[cfg(test)]
mod tests;
