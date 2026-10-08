//! Periodic refresh and persistence scheduler.

use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::state::AppState;

const PROXY_SERVICE_RECONCILE_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct TaskRun {
    pub last_run_at: Option<i64>,
    pub last_result: Option<String>,
    pub run_count: u64,
}

#[derive(Clone, Default)]
pub struct TaskRegistry {
    state: Arc<Mutex<BTreeMap<String, TaskRun>>>,
}

impl TaskRegistry {
    pub fn record(&self, name: &str, result: impl Into<String>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let run = state.entry(name.into()).or_default();
        run.last_run_at = Some(crate::meter::now_secs());
        run.last_result = Some(result.into());
        run.run_count += 1;
    }

    pub fn snapshot(&self, state: &AppState) -> serde_json::Value {
        let config = state.config.current();
        let runs = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        serde_json::json!({
            "account_file_watcher":{"interval_ms":1_000u64,"run":runs.get("account_file_watcher")},
            "token_refresh":{
                "interval_ms":config.tasks.token_refresh_interval_ms,
                "before_expiry_secs":config.effective_token_refresh_before_expiry(),
                "run":runs.get("token_refresh")
            },
            "status_check":{"interval_ms":config.tasks.status_check_interval_ms,"run":runs.get("status_check")},
            "adaptive_admission":{"interval_ms":config.server.adaptive.check_interval_ms,"run":runs.get("adaptive_admission")},
            "stats_persist":{"interval_ms":config.tasks.stats_persist_interval_ms,"run":runs.get("stats_persist")},
            "daily_reset":{"interval_ms":86_400_000u64,"run":runs.get("daily_reset")},
            "model_cache_refresh":{"interval_ms":model_refresh_interval_ms(&config),"run":runs.get("model_cache_refresh")},
            "proxy_service_reconcile":{"interval_ms":PROXY_SERVICE_RECONCILE_INTERVAL.as_millis() as u64,"run":runs.get("proxy_service_reconcile")},
            "health_recheck":{"interval_ms":config.pool.cooldown.quota_reset_ms,"run":runs.get("health_recheck")}
        })
    }

    pub fn readiness_issues(&self, state: &AppState) -> Vec<String> {
        let config = state.config.current();
        let runs = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let intervals = [
            ("account_file_watcher", 1_000u64),
            (
                "token_refresh",
                config.tasks.token_refresh_interval_ms.max(1_000),
            ),
            (
                "adaptive_admission",
                config.server.adaptive.check_interval_ms.max(1_000),
            ),
            ("model_cache_refresh", model_refresh_interval_ms(&config)),
            (
                "status_check",
                config.tasks.status_check_interval_ms.max(10_000),
            ),
            (
                "proxy_service_reconcile",
                PROXY_SERVICE_RECONCILE_INTERVAL.as_millis() as u64,
            ),
            (
                "health_recheck",
                config.pool.cooldown.quota_reset_ms.max(10_000),
            ),
            (
                "stats_persist",
                config.tasks.stats_persist_interval_ms.max(1_000),
            ),
        ];
        let now = crate::meter::now_secs();
        let uptime_ms = state.uptime_secs().saturating_mul(1_000);
        let mut issues = Vec::new();
        for (name, interval_ms) in intervals {
            let stale_after_ms = interval_ms.saturating_mul(3).max(60_000);
            let Some(run) = runs.get(name) else {
                if uptime_ms > stale_after_ms {
                    issues.push(format!(
                        "background task {name} has not reported a heartbeat"
                    ));
                }
                continue;
            };
            if run
                .last_result
                .as_deref()
                .is_some_and(|result| result.starts_with("supervisor failure:"))
            {
                issues.push(format!("background task {name} is restarting"));
                continue;
            }
            let stale_after = i64::try_from(stale_after_ms / 1_000)
                .unwrap_or(i64::MAX)
                .max(60);
            if run
                .last_run_at
                .is_some_and(|last_run| now.saturating_sub(last_run) > stale_after)
            {
                issues.push(format!("background task {name} heartbeat is stale"));
            }
        }
        issues
    }
}

pub fn spawn(state: Arc<AppState>, shutdown: CancellationToken) {
    spawn_account_file_watcher(Arc::clone(&state), shutdown.clone());
    spawn_token_refresh(Arc::clone(&state), shutdown.clone());
    spawn_adaptive_admission(Arc::clone(&state), shutdown.clone());
    spawn_model_refresh(Arc::clone(&state), shutdown.clone());
    spawn_status_check(Arc::clone(&state), shutdown.clone());
    spawn_proxy_service_reconcile(Arc::clone(&state), shutdown.clone());
    spawn_health_recheck(Arc::clone(&state), shutdown.clone());
    spawn_daily_reset(Arc::clone(&state), shutdown.clone());
    spawn_persistence(state, shutdown);
}

fn spawn_account_file_watcher(state: Arc<AppState>, shutdown: CancellationToken) {
    spawn_supervised(
        "account_file_watcher",
        state,
        shutdown,
        |state, shutdown| async move {
            let path = state.paths.accounts_file.clone();
            let mut previous = account_storage_signature(&path).await;
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.tick().await;
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = interval.tick() => {
                        let mut current = account_storage_signature(&path).await;
                        state.task_registry.record("account_file_watcher", "ok");
                        if current == previous {
                            continue;
                        }
                        // Account tag/removal validation depends on a stable
                        // service-binding snapshot. Keep the global lock order
                        // config -> accounts, matching admin mutations.
                        let _config_mutation = state.lock_config_mutation().await;
                        let _transaction = match state.lock_account_storage().await {
                            Ok(transaction) => transaction,
                            Err(error) => {
                                warn!(%error, path = %path.display(), "accounts file lock failed; keeping current accounts");
                                continue;
                            }
                        };
                        current = account_storage_signature(&path).await;
                        if current == previous {
                            continue;
                        }
                        previous = current;
                        match crosslane_store::accounts::AccountStore::load(&path).await {
                            Ok(next) => {
                                let disk = serde_json::to_value(next.all()).ok();
                                let memory = state.with_accounts(|accounts| {
                                    serde_json::to_value(accounts.all()).ok()
                                });
                                if disk != memory {
                                    let count = next.len();
                                    match state.apply_account_file_reload(next).await {
                                        Ok(()) => {
                                            info!(count, path = %path.display(), "accounts file reloaded");
                                        }
                                        Err(error) => {
                                            warn!(%error, path = %path.display(), "accounts file reload rejected; keeping current accounts");
                                        }
                                    }
                                }
                            }
                            Err(error) => {
                                warn!(%error, path = %path.display(), "accounts file reload failed; keeping current accounts");
                            }
                        }
                    }
                }
            }
        },
    );
}

async fn account_storage_signature(path: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    if let Ok(bytes) = tokio::fs::read(path).await {
        files.push((path.display().to_string(), bytes));
    }
    let mut directory_name = path.as_os_str().to_os_string();
    directory_name.push(".d");
    let directory = std::path::PathBuf::from(directory_name);
    let mut entries = match tokio::fs::read_dir(&directory).await {
        Ok(entries) => entries,
        Err(_) => return files,
    };
    let mut paths = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry.file_type().await.is_ok_and(|kind| kind.is_file()) {
            paths.push(entry.path());
        }
    }
    paths.sort();
    for delta in paths {
        if let Ok(bytes) = tokio::fs::read(&delta).await {
            files.push((delta.display().to_string(), bytes));
        }
    }
    files
}

fn spawn_status_check(state: Arc<AppState>, shutdown: CancellationToken) {
    spawn_supervised(
        "status_check",
        state,
        shutdown,
        |state, shutdown| async move {
            loop {
                let delay = Duration::from_millis(
                    state
                        .config
                        .current()
                        .tasks
                        .status_check_interval_ms
                        .max(10_000),
                );
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tokio::time::sleep(delay) => {
                        let result = status_check(&state).await;
                        state.task_registry.record(
                            "status_check",
                            result.unwrap_or_else(|error| error.to_string()),
                        );
                    }
                }
            }
        },
    );
}

fn spawn_health_recheck(state: Arc<AppState>, shutdown: CancellationToken) {
    spawn_supervised(
        "health_recheck",
        state,
        shutdown,
        |state, shutdown| async move {
            loop {
                let delay = Duration::from_millis(
                    state
                        .config
                        .current()
                        .pool
                        .cooldown
                        .quota_reset_ms
                        .max(10_000),
                );
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tokio::time::sleep(delay) => {
                        let result = health_recheck(&state).await;
                        state.task_registry.record(
                            "health_recheck",
                            result.unwrap_or_else(|error| error.to_string()),
                        );
                    }
                }
            }
        },
    );
}

fn spawn_model_refresh(state: Arc<AppState>, shutdown: CancellationToken) {
    spawn_supervised(
        "model_cache_refresh",
        state,
        shutdown,
        |state, shutdown| async move {
            loop {
                let result = refresh_models(&state).await;
                state.task_registry.record(
                    "model_cache_refresh",
                    result.unwrap_or_else(|error| error.to_string()),
                );
                let delay =
                    Duration::from_millis(model_refresh_interval_ms(&state.config.current()));
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tokio::time::sleep(delay) => {}
                    _ = state.wait_for_model_refresh() => {}
                }
            }
        },
    );
}

fn model_refresh_interval_ms(config: &crosslane_core::config::Config) -> u64 {
    let mut intervals = config
        .effective_providers()
        .into_iter()
        .filter(|provider| provider.enabled && provider.kind != "kiro")
        .map(|provider| provider.models.cache_ttl_ms)
        .collect::<Vec<_>>();
    if config
        .effective_providers()
        .iter()
        .any(|provider| provider.enabled && provider.kind == "kiro")
        && config.models.dynamic_discovery
    {
        intervals.push(config.models.cache_ttl_ms);
    }
    intervals
        .into_iter()
        .min()
        .unwrap_or(config.models.cache_ttl_ms)
        .max(60_000)
}

fn spawn_proxy_service_reconcile(state: Arc<AppState>, shutdown: CancellationToken) {
    spawn_supervised(
        "proxy_service_reconcile",
        state,
        shutdown,
        |state, shutdown| async move {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tokio::time::sleep(PROXY_SERVICE_RECONCILE_INTERVAL) => {
                        let _mutation = state.lock_config_mutation().await;
                        let config = state.config.current();
                        let failures = state.reconcile_proxy_services(&config).await;
                        let result = if failures.is_empty() {
                            "ok".to_string()
                        } else {
                            for (service_id, error) in &failures {
                                warn!(%service_id, %error, "proxy service supervisor restart failed");
                            }
                            format!("{} restart failures", failures.len())
                        };
                        state.task_registry.record("proxy_service_reconcile", result);
                    }
                }
            }
        },
    );
}

fn spawn_daily_reset(state: Arc<AppState>, shutdown: CancellationToken) {
    spawn_supervised(
        "daily_reset",
        state,
        shutdown,
        |state, shutdown| async move {
            loop {
                let now = crate::meter::now_secs();
                let wait = 86_400 - now.rem_euclid(86_400);
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(wait as u64)) => {
                        let result = match state.meter.reset_daily().await {
                            Ok(()) => "ok".to_string(),
                            Err(error) => {
                                warn!(%error, "failed to persist daily credit reset");
                                error.to_string()
                            }
                        };
                        state.task_registry.record("daily_reset", result);
                    }
                }
            }
        },
    );
}

fn spawn_adaptive_admission(state: Arc<AppState>, shutdown: CancellationToken) {
    spawn_supervised(
        "adaptive_admission",
        state,
        shutdown,
        |state, shutdown| async move {
            loop {
                let delay = Duration::from_millis(
                    state
                        .config
                        .current()
                        .server
                        .adaptive
                        .check_interval_ms
                        .max(1_000),
                );
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tokio::time::sleep(delay) => {
                        let adaptive = state.config.current().server.adaptive.clone();
                        let feedback = state
                            .adaptive_feedback
                            .take_if_ready(adaptive.minimum_samples);
                        let queued_requests = state.pool().queued();
                        let limit = state.adjust_admission(feedback, queued_requests).await;
                        let feedback = feedback.unwrap_or_default();
                        let overload_rate = if feedback.attempts == 0 {
                            0.0
                        } else {
                            feedback.overloads as f64 / feedback.attempts as f64
                        };
                        debug!(
                            p99_stream_slot_wait_ms = feedback.p99_stream_slot_wait_ms,
                            adaptive_samples = feedback.attempts,
                            upstream_overloads = feedback.overloads,
                            overload_rate,
                            queued_requests,
                            admission_limit = limit,
                            "adaptive admission check"
                        );
                        state.task_registry.record("adaptive_admission", "ok");
                    }
                }
            }
        },
    );
}

fn spawn_token_refresh(state: Arc<AppState>, shutdown: CancellationToken) {
    spawn_supervised(
        "token_refresh",
        state,
        shutdown,
        |state, shutdown| async move {
            loop {
                let delay = Duration::from_millis(
                    state
                        .config
                        .current()
                        .tasks
                        .token_refresh_interval_ms
                        .max(1_000),
                );
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tokio::time::sleep(delay) => {
                        let pool = state.pool();
                        let report = state.refresh_expiring_tokens(&pool).await;
                        for (account_id, account_name) in &report.refreshed {
                            info!(%account_id, %account_name, "background token refresh succeeded");
                        }
                        for (account_id, account_name, error) in &report.failures {
                            warn!(%account_id, %account_name, %error, "background token refresh failed");
                        }
                        info!(
                            checked = report.checked,
                            eligible = report.eligible,
                            refreshed = report.refreshed.len(),
                            failures = report.failures.len(),
                            "background token refresh check completed"
                        );
                        state.task_registry.record("token_refresh", report.summary());
                    }
                }
            }
        },
    );
}

fn spawn_persistence(state: Arc<AppState>, shutdown: CancellationToken) {
    spawn_supervised(
        "stats_persist",
        state,
        shutdown,
        |state, shutdown| async move {
            loop {
                let delay = Duration::from_millis(
                    state
                        .config
                        .current()
                        .tasks
                        .stats_persist_interval_ms
                        .max(1_000),
                );
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tokio::time::sleep(delay) => persist_usage(&state).await,
                }
            }
        },
    );
}

fn spawn_supervised<F, Fut>(
    name: &'static str,
    state: Arc<AppState>,
    shutdown: CancellationToken,
    task: F,
) where
    F: Fn(Arc<AppState>, CancellationToken) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let mut restart_attempt = 0u32;
        loop {
            let outcome = AssertUnwindSafe(task(Arc::clone(&state), shutdown.clone()))
                .catch_unwind()
                .await;
            if shutdown.is_cancelled() {
                break;
            }
            restart_attempt = restart_attempt.saturating_add(1);
            let reason = match outcome {
                Ok(()) => "task exited unexpectedly".to_string(),
                Err(payload) => format!("task panicked: {}", panic_message(payload)),
            };
            state.task_registry.record(
                name,
                format!("supervisor failure: {reason}; restart {restart_attempt}"),
            );
            warn!(task = name, %reason, restart_attempt, "background task will be restarted");
            let backoff = Duration::from_secs(1u64 << restart_attempt.saturating_sub(1).min(6));
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(backoff) => {}
            }
        }
    });
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic payload".to_string())
}

async fn persist_usage(state: &Arc<AppState>) {
    let mut errors = Vec::new();
    if let Err(error) = state.meter.persist().await {
        warn!(%error, "failed to persist API key usage");
        errors.push(error.to_string());
    }
    if let Err(error) = state.stats.persist().await {
        warn!(%error, "failed to persist proxy stats");
        errors.push(error.to_string());
    }
    let (p50, p95, p99) = state.stats.latency_percentiles();
    debug!(
        p50_ms = p50,
        p95_ms = p95,
        p99_ms = p99,
        "request latency percentiles"
    );
    state.task_registry.record(
        "stats_persist",
        if errors.is_empty() {
            "ok".to_string()
        } else {
            format!("failed: {}", errors.join("; "))
        },
    );
}

/// Completes the final metering/statistics checkpoint before the daemon exits.
/// A bounded wait prevents a broken filesystem from hanging service shutdown.
pub(crate) async fn flush_before_shutdown(state: &Arc<AppState>) {
    const FINAL_PERSIST_TIMEOUT: Duration = Duration::from_secs(30);
    if tokio::time::timeout(FINAL_PERSIST_TIMEOUT, persist_usage(state))
        .await
        .is_err()
    {
        warn!(
            timeout_secs = FINAL_PERSIST_TIMEOUT.as_secs(),
            "timed out waiting for final usage persistence during shutdown"
        );
        state.task_registry.record(
            "stats_persist",
            "failed: final shutdown persistence timed out".to_string(),
        );
    }
}

pub(crate) async fn refresh_models(state: &Arc<AppState>) -> anyhow::Result<String> {
    refresh_models_for(state, None).await
}

pub(crate) async fn refresh_models_for(
    state: &Arc<AppState>,
    provider_filter: Option<&str>,
) -> anyhow::Result<String> {
    refresh_models_in(state, provider_filter, None).await
}

/// Refresh only the Kiro models reachable through one proxy service's account
/// pool. Scoped refreshes merge into the catalog instead of replacing it, so a
/// service-scoped discovery never drops models found via other accounts.
pub(crate) async fn refresh_models_scoped(
    state: &Arc<AppState>,
    account_ids: &HashSet<String>,
) -> anyhow::Result<String> {
    refresh_models_in(state, Some("kiro"), Some(account_ids)).await
}

async fn refresh_models_in(
    state: &Arc<AppState>,
    provider_filter: Option<&str>,
    account_ids: Option<&HashSet<String>>,
) -> anyhow::Result<String> {
    let config = state.config.current();
    let provider_filter = provider_filter.filter(|provider| *provider != "all");
    let selected = config
        .effective_providers()
        .into_iter()
        .filter(|provider| provider_filter.is_none_or(|id| provider.id == id))
        .collect::<Vec<_>>();
    if selected.is_empty() {
        anyhow::bail!(
            "unknown provider {}",
            provider_filter.unwrap_or("<empty selection>")
        );
    }

    let mut refreshed = Vec::new();
    let mut failures = Vec::new();
    let mut adapter_ids = Vec::new();
    for provider in &selected {
        if !provider.enabled {
            failures.push(format!("{}: provider is disabled", provider.id));
        } else if provider.kind == "kiro" {
            if !config.models.dynamic_discovery {
                refreshed.push("kiro: discovery disabled".into());
            } else {
                match refresh_kiro_models(state, account_ids).await {
                    Ok(result) => refreshed.push(format!("kiro: {result}")),
                    Err(error) => failures.push(format!("kiro: {error}")),
                }
            }
        } else {
            adapter_ids.push(crosslane_core::provider::ProviderId::parse(
                provider.id.clone(),
            )?);
        }
    }
    if !adapter_ids.is_empty() {
        let (models, errors) = state.providers.models(&adapter_ids, true).await;
        let mut counts = BTreeMap::<String, usize>::new();
        for model in models {
            *counts.entry(model.provider_id.to_string()).or_default() += 1;
        }
        for provider_id in &adapter_ids {
            if let Some(error) = errors.get(provider_id.as_str()) {
                failures.push(format!("{provider_id}: {error}"));
            } else {
                refreshed.push(format!(
                    "{provider_id}: {} models",
                    counts
                        .get(provider_id.as_str())
                        .copied()
                        .unwrap_or_default()
                ));
            }
        }
    }
    if refreshed.is_empty() {
        anyhow::bail!("model discovery failed: {}", failures.join("; "));
    }
    let mut summary = format!("ok: {}", refreshed.join(", "));
    if !failures.is_empty() {
        summary.push_str(&format!(
            "; {} failures: {}",
            failures.len(),
            failures.join("; ")
        ));
    }
    Ok(summary)
}

async fn refresh_kiro_models(
    state: &Arc<AppState>,
    account_ids: Option<&HashSet<String>>,
) -> anyhow::Result<String> {
    let pool = state.pool();
    let accounts = pool
        .snapshot()
        .await
        .into_iter()
        .filter(|account| {
            account.enabled
                && account_ids.is_none_or(|allowed_ids| allowed_ids.contains(&account.id))
        })
        .collect::<Vec<_>>();
    if accounts.is_empty() {
        state.models.finish_refresh(Vec::new());
        anyhow::bail!("no enabled account");
    }
    let mut seen = HashSet::new();
    let mut discovered = Vec::new();
    let mut failures = Vec::new();
    for account in accounts {
        match state.kiro().list_models(&account).await {
            Ok(models) => {
                if let Some(runtime) = pool.get(&account.id).await {
                    runtime
                        .set_supported_models(models.iter().map(|model| model.model_id.clone()))
                        .await;
                }
                extend_discovered_models(&mut discovered, &mut seen, models);
            }
            Err(error) => failures.push(format!("{}: {error}", account.id)),
        }
    }
    if discovered.is_empty() && !failures.is_empty() {
        state.models.finish_refresh(Vec::new());
        anyhow::bail!("model discovery failed: {}", failures.join("; "));
    }
    let count = discovered.len();
    // A scoped refresh only saw one service's accounts, so merge to keep models
    // discovered through accounts outside that scope.
    if account_ids.is_some() {
        state.models.merge_refresh(discovered);
    } else {
        state.models.finish_refresh(discovered);
    }
    Ok(format!("ok: {count} models, {} failures", failures.len()))
}

fn extend_discovered_models(
    discovered: &mut Vec<crosslane_kiro::ModelInfo>,
    seen: &mut HashSet<String>,
    models: Vec<crosslane_kiro::ModelInfo>,
) {
    for model in models {
        if seen.insert(model.model_id.clone()) {
            discovered.push(model);
        }
    }
}

async fn status_check(state: &Arc<AppState>) -> anyhow::Result<String> {
    let pool = state.pool();
    let accounts = pool.snapshot().await;
    let mut healthy = 0;
    let mut failed = 0;
    for account in accounts.into_iter().filter(|account| account.enabled) {
        let status_ok = match refresh_account_usage(state, &pool, &account.id).await {
            Ok(_) => true,
            Err(error) => {
                debug!(
                    account_id = %account.id,
                    account_name = account.display_name(),
                    %error,
                    "usage status check failed"
                );
                false
            }
        };
        if status_ok {
            healthy += 1;
        } else {
            failed += 1;
        }
    }
    crate::alerts::sync_quota_incidents(state).await;
    persist_pool_accounts(state).await?;
    Ok(format!("ok: {healthy} healthy, {failed} failed"))
}

pub(crate) async fn refresh_account_usage(
    state: &Arc<AppState>,
    pool: &crosslane_pool::AccountPool,
    account_id: &str,
) -> anyhow::Result<bool> {
    let runtime = pool
        .get(account_id)
        .await
        .ok_or_else(|| anyhow::anyhow!("account not found: {account_id}"))?;
    let account = runtime.account.read().await.clone();
    let limits = state.kiro().get_usage_limits(&account).await?;
    let usage = limits.normalized_usage(crate::meter::now_secs());
    let subscription = limits.normalized_subscription();
    let upstream_user_id = limits
        .user_info
        .as_ref()
        .map(|identity| identity.user_id.trim())
        .filter(|user_id| !user_id.is_empty())
        .map(str::to_owned);
    if let (Some(bound), Some(received)) = (
        account.upstream_user_id.as_deref(),
        upstream_user_id.as_deref(),
    ) {
        anyhow::ensure!(
            bound == received,
            "Kiro identity changed for account {account_id}; refusing to replace the stored binding"
        );
    }
    let duplicate_identity = if let Some(user_id) = upstream_user_id.as_deref() {
        pool.snapshot().await.into_iter().find(|existing| {
            existing.id != account.id
                && existing.upstream_user_id.as_deref().map(str::trim) == Some(user_id)
        })
    } else {
        None
    };
    let upstream_user_id = if let Some(existing) = duplicate_identity {
        warn!(
            account_id,
            existing_account_id = %existing.id,
            existing_account_name = %existing.display_name(),
            "Kiro user ID is already bound to another account; not updating identity binding"
        );
        None
    } else {
        upstream_user_id
    };
    if usage.is_none() && subscription.is_none() && upstream_user_id.is_none() {
        return Ok(false);
    }
    let authoritative_usage = usage.clone();
    let mut account = runtime.account.write().await;
    if let Some(user_id) = upstream_user_id {
        account.upstream_user_id = Some(user_id);
    }
    if let Some(usage) = usage {
        account.credit_exhausted =
            crosslane_pool::usage_credit_exhausted(&usage, &state.runtime_config_snapshot().pool);
        account.usage = Some(usage);
    }
    if let Some(subscription) = subscription {
        account.subscription = Some(subscription);
    }
    drop(account);
    if let Some(usage) = authoritative_usage {
        state.record_authoritative_usage(account_id, usage);
    }
    Ok(true)
}

async fn health_recheck(state: &Arc<AppState>) -> anyhow::Result<String> {
    let pool = state.pool();
    let accounts = pool.snapshot().await;
    let mut recovered = 0;
    for account in accounts.into_iter().filter(|account| account.enabled) {
        let exhausted = pool
            .get(&account.id)
            .await
            .is_some_and(|runtime| runtime.health() == crosslane_pool::AccountHealth::Exhausted);
        if !account.credit_exhausted && !exhausted {
            continue;
        }
        let usage_recovered = refresh_account_usage(state, &pool, &account.id)
            .await
            .ok()
            .filter(|updated| *updated)
            .is_some()
            && pool.get(&account.id).await.is_some_and(|runtime| {
                runtime
                    .account
                    .try_read()
                    .map(|account| !account.credit_exhausted)
                    .unwrap_or(false)
            });
        if !usage_recovered {
            continue;
        }
        if state.config.current().models.dynamic_discovery {
            let Ok(models) = state.kiro().list_models(&account).await else {
                continue;
            };
            if let Some(runtime) = pool.get(&account.id).await {
                runtime
                    .set_supported_models(models.into_iter().map(|model| model.model_id))
                    .await;
            }
        }
        if pool.reset_health(&account.id).await {
            recovered += 1;
        }
    }
    if recovered > 0 {
        persist_pool_accounts(state).await?;
    }
    Ok(format!("ok: recovered {recovered}"))
}

pub async fn run_named(state: &Arc<AppState>, name: &str) -> anyhow::Result<serde_json::Value> {
    let result = match name {
        "token_refresh" => {
            let pool = state.pool();
            let report = state.refresh_expiring_tokens(&pool).await;
            report.summary()
        }
        "stats_persist" => {
            state.meter.persist().await?;
            state.stats.persist().await?;
            "ok".into()
        }
        "model_cache_refresh" => refresh_models(state).await?,
        "status_check" => status_check(state).await?,
        "proxy_service_reconcile" => {
            let _mutation = state.lock_config_mutation().await;
            let config = state.config.current();
            let failures = state.reconcile_proxy_services(&config).await;
            if !failures.is_empty() {
                anyhow::bail!(
                    "proxy service restart failed: {}",
                    failures
                        .into_iter()
                        .map(|(service_id, error)| format!("{service_id}: {error}"))
                        .collect::<Vec<_>>()
                        .join("; ")
                );
            }
            "ok".into()
        }
        "daily_reset" => {
            state.meter.reset_daily().await?;
            "ok".into()
        }
        "health_recheck" => health_recheck(state).await?,
        _ => anyhow::bail!("unknown task {name}"),
    };
    state.task_registry.record(name, result.clone());
    Ok(serde_json::json!({"name":name,"result":result}))
}

pub(crate) async fn persist_pool_accounts(state: &Arc<AppState>) -> anyhow::Result<()> {
    state.persist_runtime_accounts().await
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crosslane_core::config::{Config, ProviderConfig};
    use crosslane_core::paths::Paths;
    use crosslane_store::accounts::AccountStore;
    use crosslane_store::config_loader::ConfigHandle;

    use super::*;

    fn model(id: &str, name: &str) -> crosslane_kiro::ModelInfo {
        crosslane_kiro::ModelInfo {
            model_id: id.into(),
            model_name: name.into(),
            description: String::new(),
            rate_multiplier: None,
            token_limits: None,
            additional_model_request_fields_schema: None,
        }
    }

    #[test]
    fn discovered_models_keep_upstream_order_and_first_metadata() {
        let mut discovered = Vec::new();
        let mut seen = HashSet::new();
        extend_discovered_models(
            &mut discovered,
            &mut seen,
            vec![model("model-b", "B first"), model("model-a", "A")],
        );
        extend_discovered_models(
            &mut discovered,
            &mut seen,
            vec![model("model-b", "B duplicate"), model("model-c", "C")],
        );

        assert_eq!(
            discovered
                .iter()
                .map(|model| model.model_id.as_str())
                .collect::<Vec<_>>(),
            ["model-b", "model-a", "model-c"]
        );
        assert_eq!(discovered[0].model_name, "B first");
    }

    #[tokio::test]
    async fn provider_scoped_kiro_refresh_does_not_touch_other_providers() {
        let directory = tempfile::tempdir().expect("tempdir");
        let paths = Paths::from_env_values(
            Some(directory.path().to_str().expect("utf8")),
            None,
            None,
            None,
        );
        crosslane_store::bootstrap::ensure_layout(&paths)
            .await
            .expect("layout");
        let accounts = AccountStore::load(&paths.accounts_file)
            .await
            .expect("accounts");
        let mut config = Config {
            provider: vec![
                ProviderConfig::default(),
                ProviderConfig {
                    id: "copilot".into(),
                    kind: "copilot".into(),
                    ..ProviderConfig::default()
                },
            ],
            ..Config::default()
        };
        config.models.dynamic_discovery = false;
        let state = Arc::new(AppState::new(paths, ConfigHandle::new(config), accounts));

        let result = refresh_models_for(&state, Some("kiro"))
            .await
            .expect("disabled Kiro discovery is a successful no-op");

        assert_eq!(result, "ok: kiro: discovery disabled");
    }

    fn recorded_request() -> crate::stats::RequestLog {
        crate::stats::RequestLog {
            timestamp: crate::now_secs(),
            trace_id: "trace-shutdown".into(),
            request_id: "request-shutdown".into(),
            path: "/v1/messages".into(),
            provider_id: "kiro".into(),
            model: "claude-sonnet-4.6".into(),
            original_model: "claude-sonnet-4.6".into(),
            kiro_model: "claude-sonnet-4.6".into(),
            account_id: "acc_test".into(),
            account_name: "test".into(),
            endpoint: "AmazonQ".into(),
            model_path: Vec::new(),
            model_mapping_rule: None,
            attempts: Vec::new(),
            duration_ms: 10,
            status: 200,
            input_tokens: 1,
            output_tokens: 1,
            credits: 0.1,
            error: None,
            diagnostics: crate::stats::RequestDiagnostics::default(),
        }
    }

    #[tokio::test]
    async fn final_shutdown_flush_is_durable_before_returning() {
        let directory = tempfile::tempdir().expect("tempdir");
        let paths = Paths::from_env_values(
            Some(directory.path().to_str().expect("utf8")),
            None,
            None,
            None,
        );
        crosslane_store::bootstrap::ensure_layout(&paths)
            .await
            .expect("layout");
        let accounts = AccountStore::load(&paths.accounts_file)
            .await
            .expect("accounts");
        let state = Arc::new(AppState::new(
            paths.clone(),
            ConfigHandle::new(Config::default()),
            accounts,
        ));
        state.stats.record(recorded_request());

        flush_before_shutdown(&state).await;

        let persisted: crate::stats::ProxyStats = serde_json::from_str(
            &tokio::fs::read_to_string(&paths.stats_file)
                .await
                .expect("read final checkpoint"),
        )
        .expect("parse final checkpoint");
        assert_eq!(persisted.total.requests, 1);
        assert!(persisted.minute_buckets.is_empty());
    }

    #[tokio::test]
    async fn supervisor_restarts_a_panicked_background_task() {
        let directory = tempfile::tempdir().expect("tempdir");
        let paths = Paths::from_env_values(
            Some(directory.path().to_str().expect("utf8")),
            None,
            None,
            None,
        );
        crosslane_store::bootstrap::ensure_layout(&paths)
            .await
            .expect("layout");
        let accounts = AccountStore::load(&paths.accounts_file)
            .await
            .expect("accounts");
        let state = Arc::new(AppState::new(
            paths,
            ConfigHandle::new(Config::default()),
            accounts,
        ));
        let shutdown = CancellationToken::new();
        let attempts = Arc::new(AtomicUsize::new(0));
        let task_attempts = Arc::clone(&attempts);
        spawn_supervised(
            "panic_probe",
            state,
            shutdown.clone(),
            move |_state, shutdown| {
                let attempts = Arc::clone(&task_attempts);
                async move {
                    if attempts.fetch_add(1, Ordering::AcqRel) == 0 {
                        panic!("injected task failure");
                    }
                    shutdown.cancelled().await;
                }
            },
        );

        tokio::time::timeout(Duration::from_secs(3), async {
            while attempts.load(Ordering::Acquire) < 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("task restarted");
        shutdown.cancel();
    }
}
