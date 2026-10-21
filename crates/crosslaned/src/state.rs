//! 服务运行态共享句柄。

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use crosslane_core::account::{Account, AuthMethod, Usage};
use crosslane_core::config::Config;
use crosslane_core::paths::Paths;
use crosslane_kiro::endpoint::EndpointOverrides;
use crosslane_kiro::{KiroClient, KiroError, KiroResponse, ModelInfo};
use crosslane_notify::Notifier;
use crosslane_pool::{
    AccountCreditState, AccountPool, RefreshError, RefreshOutcome, RefreshedCredentials,
    ReloadedCredentials, TokenRefresher,
};
use crosslane_store::accounts::AccountStore;
use crosslane_store::atomic::{lock_file_exclusive, ExclusiveFileLock};
use crosslane_store::config_loader::ConfigHandle;
use crosslane_translate::{
    model::{
        apply_adaptive_thinking, resolve_dynamic_model, thinking_enabled_for_model,
        ThinkingDecision,
    },
    KiroPayload, TokenCountCache, WebSearchReplayCodec,
};
use rand::RngCore;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::http::compaction_replay::CompactionReplayTracker;
use crate::http::prompt_cache::PromptCacheTracker;
use crate::http::responses::ResponsesSessionStore;
use crate::http::stream::KeepaliveHub;
use crate::meter::Meter;
use crate::stats::{ModelCache, StatsStore};
use crate::tasks::TaskRegistry;

pub struct TokenRefreshReport {
    pub checked: usize,
    pub eligible: usize,
    pub refreshed: Vec<(String, String)>,
    pub failures: Vec<(String, String, RefreshError)>,
}

const MAX_ADAPTIVE_FEEDBACK_SAMPLES: usize = 10_000;

#[derive(Debug, Clone, Copy, Default)]
pub struct AdaptiveFeedbackSnapshot {
    pub attempts: usize,
    pub successful_samples: usize,
    pub overloads: usize,
    pub p99_stream_slot_wait_ms: u64,
}

#[derive(Default)]
struct AdaptiveFeedbackWindow {
    attempts: usize,
    overloads: usize,
    stream_slot_wait_ms: VecDeque<u64>,
}

#[derive(Debug, Clone)]
pub(crate) struct AuthoritativeUsageObservation {
    pub usage: Usage,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy)]
struct PendingCreditObservation {
    generation: u64,
    state: AccountCreditState,
    consecutive: u8,
}

#[derive(Default)]
pub struct AdaptiveFeedback {
    inner: Mutex<AdaptiveFeedbackWindow>,
}

impl AdaptiveFeedback {
    fn clear(&self) {
        *lock(&self.inner) = AdaptiveFeedbackWindow::default();
    }

    fn record_success(&self, stream_slot_wait_ms: u64) {
        let mut window = lock(&self.inner);
        window.attempts = window.attempts.saturating_add(1);
        window.stream_slot_wait_ms.push_back(stream_slot_wait_ms);
        while window.stream_slot_wait_ms.len() > MAX_ADAPTIVE_FEEDBACK_SAMPLES {
            window.stream_slot_wait_ms.pop_front();
        }
    }

    fn record_failure(&self, overloaded: bool) {
        let mut window = lock(&self.inner);
        window.attempts = window.attempts.saturating_add(1);
        window.overloads = window.overloads.saturating_add(usize::from(overloaded));
    }

    pub fn take_if_ready(&self, minimum_samples: usize) -> Option<AdaptiveFeedbackSnapshot> {
        let mut window = lock(&self.inner);
        if window.attempts < minimum_samples.max(1) {
            return None;
        }
        let window = std::mem::take(&mut *window);
        let mut waits = window.stream_slot_wait_ms.into_iter().collect::<Vec<_>>();
        waits.sort_unstable();
        let p99_stream_slot_wait_ms = waits
            .get(((waits.len().saturating_sub(1)) as f64 * 0.99).ceil() as usize)
            .copied()
            .unwrap_or_default();
        Some(AdaptiveFeedbackSnapshot {
            attempts: window.attempts,
            successful_samples: waits.len(),
            overloads: window.overloads,
            p99_stream_slot_wait_ms,
        })
    }
}

impl TokenRefreshReport {
    pub fn summary(&self) -> String {
        format!(
            "ok: {} checked, {} eligible, {} refreshed, {} failures",
            self.checked,
            self.eligible,
            self.refreshed.len(),
            self.failures.len()
        )
    }
}

/// 进程内共享状态。
pub struct AppState {
    /// 当前生效配置。
    pub config: ConfigHandle,
    /// 账号库快照。
    pub accounts: Arc<RwLock<AccountStore>>,
    /// 解析后的文件路径。
    pub paths: Paths,
    pool: RwLock<AccountPool>,
    kiro: RwLock<KiroClient>,
    model_policy: Arc<crosslane_provider::ModelPolicy>,
    pub meter: Arc<Meter>,
    notifier: RwLock<Notifier>,
    pub tokenizer: TokenCountCache,
    pub web_search_replay: WebSearchReplayCodec,
    pub stats: Arc<StatsStore>,
    pub models: Arc<ModelCache>,
    /// Registered Kiro, Copilot, and future provider runtimes.
    pub providers: Arc<crate::providers::ProviderManager>,
    pub(crate) provider_scheduler: crate::provider_routing::ProviderScheduler,
    model_refresh: tokio::sync::Notify,
    refresher: RwLock<TokenRefresher>,
    tls_config: RwLock<Option<axum_server::tls_rustls::RustlsConfig>>,
    runtime_handle: Option<tokio::runtime::Handle>,
    runtime_config: RwLock<Config>,
    pub admission: Arc<AdmissionGate>,
    pub adaptive_feedback: AdaptiveFeedback,
    pub connections: Arc<AdmissionGate>,
    pub body_budget: Arc<BodyBudget>,
    pub keepalive: KeepaliveHub,
    pub(crate) compaction_replay: CompactionReplayTracker,
    #[cfg(feature = "crs")]
    pub(crate) context_pressure: crate::http::crs::ContextPressure,
    pub prompt_cache: PromptCacheTracker,
    /// Ephemeral, credential-scoped state for Responses `store=true` turns.
    pub responses_sessions: ResponsesSessionStore,
    pub response_client_contexts: crate::http::responses::ResponseClientContexts,
    pub task_registry: TaskRegistry,
    /// Dynamically managed API proxy listeners.
    pub proxy_services: Arc<crate::http::ProxyServiceManager>,
    /// Shared graceful-shutdown token for process lifecycle events.
    pub shutdown: CancellationToken,
    /// 启动时刻。
    pub started_at: Instant,
    config_reloaded_at: AtomicI64,
    authoritative_usage: RwLock<HashMap<String, AuthoritativeUsageObservation>>,
    authoritative_usage_generation: AtomicU64,
    pending_credit_observations: Mutex<HashMap<String, PendingCreditObservation>>,
    quota_alert_sync: tokio::sync::Mutex<()>,
    account_mutation: tokio::sync::Mutex<()>,
    config_mutation: tokio::sync::Mutex<()>,
}

/// Holds both layers of the account storage transaction lock.
///
/// The Tokio guard serializes every writer and the file guard coordinates
/// multiple crosslane processes that share the same account file.
pub(crate) struct AccountStorageGuard<'a> {
    _mutation: tokio::sync::MutexGuard<'a, ()>,
    _file: ExclusiveFileLock,
}

impl AppState {
    /// 构造共享状态。
    #[cfg(test)]
    pub fn new(paths: Paths, config: ConfigHandle, accounts: AccountStore) -> Self {
        let current = config.current();
        let meter = Meter::empty(&paths.daily_file, &current.api_key);
        let stats = Arc::new(StatsStore::empty(&paths.stats_file));
        Self::build(
            paths,
            config,
            accounts,
            meter,
            stats,
            WebSearchReplayCodec::from_key([0xA5; 32]),
        )
        .expect("default runtime components must initialize")
    }

    pub async fn load(
        paths: Paths,
        config: ConfigHandle,
        mut accounts: AccountStore,
    ) -> anyhow::Result<Self> {
        let current = config.current();
        accounts.set_compression_threshold(current.storage.compression_threshold);
        accounts.set_incremental_write(current.storage.incremental_write);
        let meter = match Meter::load(&paths.daily_file, &current.api_key).await {
            Ok(meter) => meter,
            Err(error) => {
                quarantine_corrupt_state(&paths.daily_file, "metering", &error.to_string()).await;
                warn!(
                    error = %error,
                    path = %paths.daily_file.display(),
                    "metering state is unavailable; starting management plane in fail-closed recovery mode"
                );
                Meter::fail_closed(&paths.daily_file, &current.api_key, error.to_string())
            }
        };
        let stats = match StatsStore::load(&paths.stats_file).await {
            Ok(stats) => Arc::new(stats),
            Err(error) => {
                quarantine_corrupt_state(&paths.stats_file, "statistics", &error.to_string()).await;
                warn!(
                    error = %error,
                    path = %paths.stats_file.display(),
                    "statistics state is unavailable; starting with empty statistics"
                );
                Arc::new(StatsStore::empty(&paths.stats_file))
            }
        };
        let web_search_replay =
            load_or_regenerate_replay_key(&paths.web_search_replay_key_file).await;
        let state = Self::build(paths, config, accounts, meter, stats, web_search_replay)?;
        let current = state.config.current();
        state
            .providers
            .reconcile(&current, &state.paths.data_dir)
            .await
            .map_err(anyhow::Error::msg)?;
        Ok(state)
    }

    fn build(
        paths: Paths,
        config: ConfigHandle,
        mut accounts: AccountStore,
        meter: Arc<Meter>,
        stats: Arc<StatsStore>,
        web_search_replay: WebSearchReplayCodec,
    ) -> anyhow::Result<Self> {
        let current = config.current();
        validate_service_account_references(&current, accounts.all(), &paths.data_dir)
            .map_err(anyhow::Error::msg)?;
        meter.set_daily_limit(current.pool.daily_credit_limit);
        accounts.set_compression_threshold(current.storage.compression_threshold);
        accounts.set_incremental_write(current.storage.incremental_write);
        let overrides = EndpointOverrides {
            codewhisperer_url: std::env::var("CROSSLANE_CODEWHISPERER_URL").ok(),
            amazonq_url: std::env::var("CROSSLANE_AMAZONQ_URL").ok(),
            mcp_url: std::env::var("CROSSLANE_MCP_URL").ok(),
            runtime_url: std::env::var("CROSSLANE_RUNTIME_URL").ok(),
            management_url: std::env::var("CROSSLANE_MANAGEMENT_URL").ok(),
        };
        let model_policy = Arc::new(crosslane_provider::ModelPolicy::new(current.models.clone()));
        let kiro = KiroClient::new(current.upstream.clone(), overrides)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?
            .with_model_policy(Arc::clone(&model_policy));
        let refresher = TokenRefresher::new(current.effective_token_refresh_before_expiry())
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let pool = AccountPool::new(accounts.all().to_vec(), current.pool.clone());
        let notifier = Notifier::persistent(
            current.webhook.clone(),
            current.notify.clone(),
            1_024,
            paths.alert_incidents_file.clone(),
        );
        let admission_limit = current.server.max_concurrent_requests.max(1);
        let connection_limit = current.server.max_connections.max(1);
        let models = Arc::new(ModelCache::default());
        let providers = Arc::new(
            crate::providers::ProviderManager::new(Arc::clone(&models))
                .with_model_policy(Arc::clone(&model_policy)),
        );
        Ok(Self {
            config,
            accounts: Arc::new(RwLock::new(accounts)),
            paths,
            pool: RwLock::new(pool),
            kiro: RwLock::new(kiro),
            model_policy,
            meter,
            notifier: RwLock::new(notifier),
            tokenizer: TokenCountCache::new(512).map_err(anyhow::Error::msg)?,
            web_search_replay,
            stats,
            models,
            providers,
            provider_scheduler: crate::provider_routing::ProviderScheduler::default(),
            model_refresh: tokio::sync::Notify::new(),
            refresher: RwLock::new(refresher),
            tls_config: RwLock::new(None),
            runtime_handle: tokio::runtime::Handle::try_current().ok(),
            runtime_config: RwLock::new(current.as_ref().clone()),
            admission: Arc::new(AdmissionGate::new(admission_limit)),
            adaptive_feedback: AdaptiveFeedback::default(),
            connections: Arc::new(AdmissionGate::new(connection_limit)),
            body_budget: Arc::new(BodyBudget::new(128 * 1024 * 1024)),
            keepalive: KeepaliveHub::new(),
            compaction_replay: CompactionReplayTracker::default(),
            #[cfg(feature = "crs")]
            context_pressure: crate::http::crs::ContextPressure::default(),
            prompt_cache: PromptCacheTracker::default(),
            responses_sessions: ResponsesSessionStore::default(),
            response_client_contexts: crate::http::responses::ResponseClientContexts::default(),
            task_registry: TaskRegistry::default(),
            proxy_services: Arc::new(crate::http::ProxyServiceManager::default()),
            shutdown: CancellationToken::new(),
            started_at: Instant::now(),
            config_reloaded_at: AtomicI64::new(0),
            authoritative_usage: RwLock::new(HashMap::new()),
            authoritative_usage_generation: AtomicU64::new(0),
            pending_credit_observations: Mutex::new(HashMap::new()),
            quota_alert_sync: tokio::sync::Mutex::new(()),
            account_mutation: tokio::sync::Mutex::new(()),
            config_mutation: tokio::sync::Mutex::new(()),
        })
    }

    /// 运行时长。
    pub fn uptime_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    /// 记录配置重载时间。
    pub fn mark_config_reloaded(&self, at: i64) {
        self.config_reloaded_at.store(at, Ordering::Relaxed);
    }

    /// 上次配置重载时间。
    pub fn config_reloaded_at(&self) -> Option<i64> {
        let value = self.config_reloaded_at.load(Ordering::Relaxed);
        (value != 0).then_some(value)
    }

    /// 只读访问账号库。
    pub fn with_accounts<R>(&self, inspect: impl FnOnce(&AccountStore) -> R) -> R {
        match self.accounts.read() {
            Ok(guard) => inspect(&guard),
            Err(poisoned) => inspect(&poisoned.into_inner()),
        }
    }

    /// 整体替换账号库快照。
    pub fn replace_accounts(&self, next: AccountStore) {
        match self.accounts.write() {
            Ok(mut guard) => *guard = next,
            Err(poisoned) => *poisoned.into_inner() = next,
        }
    }

    /// 应用账号文件的外部变更，并用新快照重建调度池。
    pub async fn apply_account_file_reload(&self, mut next: AccountStore) -> Result<(), String> {
        self.validate_or_restore_account_store(&next).await?;
        let config = self.config.current();
        next.set_compression_threshold(config.storage.compression_threshold);
        next.set_incremental_write(config.storage.incremental_write);
        self.install_account_store(next).await;
        self.request_model_refresh();
        Ok(())
    }

    /// Return every configured service whose effective pool contains the
    /// account. Disabled services still own their bindings.
    pub(crate) fn account_service_bindings(&self, account: &Account) -> Vec<(String, String)> {
        self.config
            .current()
            .proxy_service
            .iter()
            .filter(|service| service.includes_account(account))
            .map(|service| (service.id.clone(), service.name.clone()))
            .collect()
    }

    pub(crate) fn validate_account_store_transition(
        &self,
        next: &AccountStore,
    ) -> Result<(), String> {
        self.with_accounts(|current| {
            for account in current.all() {
                let tags_changed_or_removed = next
                    .find(&account.id)
                    .is_none_or(|replacement| !same_account_tags(&replacement.tags, &account.tags));
                if !tags_changed_or_removed {
                    continue;
                }
                let bindings = self.account_service_bindings(account);
                if !bindings.is_empty() {
                    return Err(format!(
                        "account {} is bound to proxy service(s) {}; unbind it before changing tags or removing it",
                        account.id,
                        bindings
                            .iter()
                            .map(|(id, name)| format!("{name} ({id})"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
            Ok(())
        })
    }

    pub(crate) async fn validate_or_restore_account_store(
        &self,
        next: &AccountStore,
    ) -> Result<(), String> {
        let Err(error) = self.validate_account_store_transition(next) else {
            return Ok(());
        };
        let mut current = self.with_accounts(Clone::clone);
        self.configure_account_store(&mut current);
        // A rejected direct file edit must not remain as the next startup
        // generation or leak back through a later runtime persistence pass.
        current.set_incremental_write(false);
        current.save().await.map_err(|rollback_error| {
            format!("{error}; restoring the previous account file failed: {rollback_error}")
        })?;
        Err(format!(
            "{error}; the previous account file has been restored"
        ))
    }

    /// Installs one durable account generation into both in-memory views.
    /// Callers must hold the account storage transaction when this is part of
    /// a write operation.
    async fn install_account_store(&self, next: AccountStore) {
        let invalidated_ids = self.with_accounts(|current| {
            current
                .all()
                .iter()
                .filter_map(|account| match next.find(&account.id) {
                    None => Some(account.id.clone()),
                    Some(replacement)
                        if replacement.credentials.access_token
                            != account.credentials.access_token
                            || replacement.credentials.auth_method
                                != account.credentials.auth_method =>
                    {
                        Some(account.id.clone())
                    }
                    Some(_) => None,
                })
                .collect::<Vec<_>>()
        });
        let snapshot = next.all().to_vec();
        self.replace_accounts(next);
        self.pool().replace_accounts(snapshot).await;
        let endpoint_cache = self.kiro().endpoint_cache();
        for account_id in invalidated_ids {
            endpoint_cache.clear_account(&account_id);
        }
    }

    /// 请求模型发现任务尽快刷新，不绕过任务自身的 singleflight 保护。
    pub fn request_model_refresh(&self) {
        self.model_refresh.notify_one();
    }

    /// 等待账号或运维操作触发一次模型目录刷新。
    pub async fn wait_for_model_refresh(&self) {
        self.model_refresh.notified().await;
    }

    /// Resolve a client/model alias against the current discovered Kiro catalog.
    ///
    /// Model metadata is shared runtime state, so callers outside the HTTP
    /// handler module must not depend on a private handler helper to read it.
    pub fn resolved_model_info(&self, model: &str) -> Option<ModelInfo> {
        let config = self.config.current();
        let (models, _) = self.models.get(config.models.cache_ttl_ms);
        let available = models
            .iter()
            .map(|candidate| candidate.model_id.clone())
            .collect::<Vec<_>>();
        let resolved = resolve_dynamic_model(model, &available)?;
        models
            .into_iter()
            .find(|candidate| candidate.model_id.eq_ignore_ascii_case(&resolved))
    }

    /// Serializes the complete account read-modify-write transaction both
    /// inside this daemon and across cooperating crosslane processes.
    pub(crate) async fn lock_account_storage(&self) -> anyhow::Result<AccountStorageGuard<'_>> {
        // Always acquire these locks in this order. Holding the in-process
        // mutex while waiting for the file lock also prevents a later local
        // writer from overtaking this transaction.
        let mutation = self.account_mutation.lock().await;
        let file = lock_file_exclusive(&self.paths.accounts_file).await?;
        Ok(AccountStorageGuard {
            _mutation: mutation,
            _file: file,
        })
    }

    /// Serializes config mutations initiated through the admin API.
    pub async fn lock_config_mutation(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.config_mutation.lock().await
    }

    /// Starts, stops, or replaces proxy listeners to match a config snapshot.
    pub async fn reconcile_proxy_services(
        self: &Arc<Self>,
        config: &Config,
    ) -> Vec<(String, String)> {
        self.proxy_services
            .reconcile(Arc::clone(self), &config.proxy_service)
            .await
    }

    /// 事务化应用热配置；任一代理监听启动失败时恢复上一份运行配置。
    pub async fn apply_config_transaction(self: &Arc<Self>, next: &Config) -> Result<(), String> {
        let _mutation = self.lock_config_mutation().await;
        self.apply_config_transaction_locked(next).await
    }

    /// Applies a configuration while the caller holds `config_mutation`.
    ///
    /// Keeping the lock across both loading and applying a file snapshot avoids
    /// an older reload overtaking a newer one.
    pub(crate) async fn apply_config_transaction_locked(
        self: &Arc<Self>,
        next: &Config,
    ) -> Result<(), String> {
        let previous = self.runtime_config_snapshot();
        validate_service_account_tag_transitions(&previous, next)?;
        let accounts = self.with_accounts(|store| store.all().to_vec());
        validate_service_account_references(next, &accounts, &self.paths.data_dir)?;
        self.config.replace(next.clone());
        self.apply_runtime_config(next).await;
        if let Err(error) = self.providers.reconcile(next, &self.paths.data_dir).await {
            self.config.replace(previous.clone());
            self.apply_runtime_config(&previous).await;
            let rollback_error = self
                .providers
                .reconcile(&previous, &self.paths.data_dir)
                .await
                .err();
            return Err(match rollback_error {
                Some(rollback_error) => format!(
                    "provider apply failed ({error}); provider rollback also failed: {rollback_error}"
                ),
                None => format!("provider apply failed; previous config restored: {error}"),
            });
        }
        let failures = self.reconcile_proxy_services(next).await;
        if failures.is_empty() {
            crate::alerts::sync_quota_incidents(self).await;
            return Ok(());
        }

        let apply_error = format_service_failures(&failures);
        self.config.replace(previous.clone());
        self.apply_runtime_config(&previous).await;
        let provider_rollback_error = self
            .providers
            .reconcile(&previous, &self.paths.data_dir)
            .await
            .err();
        let rollback_failures = self.reconcile_proxy_services(&previous).await;
        crate::alerts::sync_quota_incidents(self).await;
        if rollback_failures.is_empty() && provider_rollback_error.is_none() {
            Err(format!(
                "proxy service apply failed; previous config restored: {apply_error}"
            ))
        } else {
            Err(format!(
                "proxy service apply failed ({apply_error}); rollback also failed: services={}, providers={}",
                format_service_failures(&rollback_failures),
                provider_rollback_error.unwrap_or_else(|| "ok".into())
            ))
        }
    }

    /// 获取最后一次完整应用到运行时组件的配置快照。
    pub fn runtime_config_snapshot(&self) -> Config {
        read_lock(&self.runtime_config).clone()
    }

    /// 管理面 socket 路径。
    pub fn admin_socket(&self) -> PathBuf {
        PathBuf::from(&self.config.current().admin.socket)
    }

    pub fn pool(&self) -> AccountPool {
        read_lock(&self.pool).clone()
    }

    pub fn kiro(&self) -> KiroClient {
        read_lock(&self.kiro).clone()
    }

    /// Rebuild model controls for the actual target, including continuations
    /// and stream-time fallbacks that do not pass through initial dispatch.
    pub fn prepare_model_request(&self, payload: &mut KiroPayload) -> ThinkingDecision {
        let config = self.config.current();
        let model = &payload
            .conversation_state
            .current_message
            .user_input_message
            .model_id;
        let requested_model = payload
            .model_request_intent
            .as_ref()
            .map_or(model.as_str(), |intent| intent.requested_model.as_str());
        let enabled = thinking_enabled_for_model(model, &config.model_thinking_mode)
            && thinking_enabled_for_model(requested_model, &config.model_thinking_mode);
        let schema = self
            .resolved_model_info(model)
            // Alias/family resolution is useful for routing, but another
            // version's metadata is not the selected model's wire contract.
            .filter(|info| info.model_id.eq_ignore_ascii_case(model))
            .and_then(|info| info.additional_model_request_fields_schema);
        apply_adaptive_thinking(payload, schema.as_ref(), enabled)
    }

    /// Dispatches one generation request and records only proxy-side queueing
    /// and explicit upstream overload feedback for adaptive admission.
    pub async fn generate(
        &self,
        account: &Account,
        payload: &KiroPayload,
    ) -> Result<KiroResponse, KiroError> {
        let prepared = self.prepare_generation_payload(payload);
        self.generate_prepared(account, &prepared).await
    }

    /// Freezes attempt-local model controls without changing the caller's payload.
    pub(crate) fn prepare_generation_payload(&self, payload: &KiroPayload) -> KiroPayload {
        // Work on an attempt-local copy so routing to another model cannot
        // permanently erase the original request's effort or thinking controls.
        let mut prepared = payload.clone();
        if let Some(intent) = prepared.model_request_intent.as_mut() {
            let config = self.config.current();
            intent.requested_model = config.unqualified_model(&intent.requested_model).to_owned();
        }
        self.prepare_model_request(&mut prepared);
        prepared
    }

    /// Sends an already prepared payload without rebuilding its model controls.
    pub(crate) async fn generate_prepared(
        &self,
        account: &Account,
        payload: &KiroPayload,
    ) -> Result<KiroResponse, KiroError> {
        self.generate_prepared_with_client(&self.kiro(), account, payload)
            .await
    }

    /// Sends a prepared payload through the selected client without rebuilding either.
    pub(crate) async fn generate_prepared_with_client(
        &self,
        client: &KiroClient,
        account: &Account,
        payload: &KiroPayload,
    ) -> Result<KiroResponse, KiroError> {
        let result = client.generate(account, payload, None).await;
        self.record_generation_feedback(&result);
        result
    }

    #[cfg(feature = "crs")]
    pub(crate) async fn generate_at_endpoint_isolated_with_client(
        &self,
        client: &KiroClient,
        account: &Account,
        payload: &KiroPayload,
        endpoint: crosslane_kiro::endpoint::EndpointKey,
    ) -> Result<KiroResponse, KiroError> {
        let result = client
            .generate_at_endpoint_isolated(account, payload, endpoint)
            .await;
        self.record_generation_feedback(&result);
        result
    }

    #[cfg(feature = "crs")]
    pub(crate) async fn generate_runtime_isolated_with_client(
        &self,
        client: &KiroClient,
        account: &Account,
        payload: &KiroPayload,
        previous_endpoint: crosslane_kiro::endpoint::EndpointKey,
    ) -> Option<Result<KiroResponse, KiroError>> {
        let result = client
            .generate_runtime_isolated(account, payload, previous_endpoint)
            .await;
        if let Some(result) = &result {
            self.record_generation_feedback(result);
        }
        result
    }

    fn record_generation_feedback(&self, result: &Result<KiroResponse, KiroError>) {
        match result {
            Ok(response) => self
                .adaptive_feedback
                .record_success(response.stream_slot_wait_ms()),
            Err(error) => self.adaptive_feedback.record_failure(
                error.is_model_capacity_error() || matches!(error.status, Some(429 | 503)),
            ),
        }
    }

    /// Records an overload delivered inside an already accepted event stream.
    pub fn record_stream_overload(&self) {
        self.adaptive_feedback.record_failure(true);
    }

    pub fn notifier(&self) -> Notifier {
        read_lock(&self.notifier).clone()
    }

    /// Records usage returned by the upstream quota endpoint. Request-side
    /// optimistic settlement deliberately does not update this observation,
    /// so alerts never treat a scheduling estimate as authoritative quota.
    pub(crate) fn record_authoritative_usage(&self, account_id: &str, usage: Usage) {
        let generation = self
            .authoritative_usage_generation
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        write_lock(&self.authoritative_usage).insert(
            account_id.to_owned(),
            AuthoritativeUsageObservation { usage, generation },
        );
    }

    pub(crate) fn authoritative_usage(
        &self,
        account_id: &str,
    ) -> Option<AuthoritativeUsageObservation> {
        read_lock(&self.authoritative_usage)
            .get(account_id)
            .cloned()
    }

    /// Counts distinct upstream observations of a candidate credit state.
    /// Repeated config reloads or concurrent reconciliation of the same
    /// generation therefore cannot prematurely confirm a recovery.
    pub(crate) fn observe_credit_transition(
        &self,
        account_id: &str,
        generation: u64,
        state: AccountCreditState,
    ) -> u8 {
        let mut observations = lock(&self.pending_credit_observations);
        let observation =
            observations
                .entry(account_id.to_owned())
                .or_insert(PendingCreditObservation {
                    generation,
                    state,
                    consecutive: 1,
                });
        if observation.generation == generation {
            return observation.consecutive;
        }
        observation.consecutive = if observation.state == state {
            observation.consecutive.saturating_add(1)
        } else {
            1
        };
        observation.generation = generation;
        observation.state = state;
        observation.consecutive
    }

    pub(crate) fn clear_credit_transition(&self, account_id: &str) {
        lock(&self.pending_credit_observations).remove(account_id);
    }

    pub(crate) async fn lock_quota_alert_sync(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.quota_alert_sync.lock().await
    }

    pub fn refresher(&self) -> TokenRefresher {
        read_lock(&self.refresher).clone()
    }

    /// Refreshes one account and immediately re-enables endpoints rejected by
    /// the previous token. Successful endpoint preferences are retained.
    pub async fn refresh_account_token(
        &self,
        pool: &AccountPool,
        account_id: &str,
        force: bool,
    ) -> Result<RefreshOutcome, RefreshError> {
        if let Some(outcome) = Self::api_key_refresh_outcome(pool, account_id, force, None).await {
            return outcome;
        }
        let (account_name, account_enabled) = self.account_refresh_identity(pool, account_id).await;
        let result = match self.lock_account_refresh(account_id).await {
            Ok(_coordination) => {
                self.refresher()
                    .refresh_account_and_persist_with_reload(
                        pool,
                        account_id,
                        force,
                        || async move { self.reload_account_credentials(pool, account_id).await },
                        |refreshed| async move {
                            self.persist_refreshed_credentials(pool, refreshed).await
                        },
                    )
                    .await
            }
            Err(error) => Err(RefreshError::CredentialReload(format!(
                "failed to coordinate token refresh: {error}"
            ))),
        };
        self.finish_token_refresh_with_identity(account_id, &account_name, account_enabled, result)
    }

    /// Refreshes credentials for an authentication failure tied to the exact
    /// access token used by that failed upstream request.
    pub async fn refresh_account_token_after_auth_failure(
        &self,
        pool: &AccountPool,
        account_id: &str,
        rejected_access_token: &str,
    ) -> Result<RefreshOutcome, RefreshError> {
        if let Some(outcome) =
            Self::api_key_refresh_outcome(pool, account_id, true, Some(rejected_access_token)).await
        {
            return outcome;
        }
        let (account_name, account_enabled) = self.account_refresh_identity(pool, account_id).await;
        let result = match self.lock_account_refresh(account_id).await {
            Ok(_coordination) => {
                self.refresher()
                    .refresh_after_auth_failure_and_persist(
                        pool,
                        account_id,
                        rejected_access_token,
                        || async move { self.reload_account_credentials(pool, account_id).await },
                        |refreshed| async move {
                            self.persist_refreshed_credentials(pool, refreshed).await
                        },
                    )
                    .await
            }
            Err(error) => Err(RefreshError::CredentialReload(format!(
                "failed to coordinate token refresh: {error}"
            ))),
        };
        self.finish_token_refresh_with_identity(account_id, &account_name, account_enabled, result)
    }

    async fn api_key_refresh_outcome(
        pool: &AccountPool,
        account_id: &str,
        force: bool,
        rejected_access_token: Option<&str>,
    ) -> Option<Result<RefreshOutcome, RefreshError>> {
        let runtime = pool.get(account_id).await?;
        let account = runtime.account.read().await;
        if account.credentials.auth_method != AuthMethod::ApiKey {
            return None;
        }
        // Static keys have no OAuth refresh lifecycle: do not acquire refresh
        // coordination locks or raise token-refresh incidents. A concurrent key
        // rotation can still satisfy recovery of a request using the old key.
        Some(
            if !force
                || rejected_access_token
                    .is_some_and(|token| token != account.credentials.access_token)
            {
                Ok(RefreshOutcome {
                    changed: false,
                    persistence_error: None,
                })
            } else {
                Err(RefreshError::NotRefreshable)
            },
        )
    }

    async fn account_refresh_identity(
        &self,
        pool: &AccountPool,
        account_id: &str,
    ) -> (String, bool) {
        if let Some(runtime) = pool.get(account_id).await {
            let account = runtime.account.read().await;
            (account.display_name().to_owned(), account.enabled)
        } else {
            (account_id.to_owned(), false)
        }
    }

    /// Serializes refresh-token consumption for one account across daemon
    /// instances. The upstream request and durable commit are both inside this
    /// guard, so another process cannot consume the previous rotating token in
    /// the gap before its replacement reaches disk.
    async fn lock_account_refresh(&self, account_id: &str) -> anyhow::Result<ExclusiveFileLock> {
        let digest = Sha256::digest(account_id.as_bytes());
        let suffix = digest[..16]
            .iter()
            .fold(String::with_capacity(32), |mut output, byte| {
                use std::fmt::Write as _;
                let _ = write!(output, "{byte:02x}");
                output
            });
        let mut path = self.paths.accounts_file.as_os_str().to_os_string();
        path.push(format!(".refresh.{suffix}"));
        lock_file_exclusive(&PathBuf::from(path)).await
    }

    fn finish_token_refresh_with_identity(
        &self,
        account_id: &str,
        account_name: &str,
        account_enabled: bool,
        result: Result<RefreshOutcome, RefreshError>,
    ) -> Result<RefreshOutcome, RefreshError> {
        match result {
            Ok(outcome) => {
                if outcome.changed {
                    self.kiro().endpoint_cache().clear_failures(account_id);
                }
                if let Some(error) = outcome.persistence_error.as_deref() {
                    if account_enabled {
                        crate::alerts::emit_token_refresh_failure(
                            self,
                            account_id,
                            account_name,
                            &crosslane_translate::sanitize_error_message(&format!(
                                "refreshed credentials could not be persisted: {error}"
                            )),
                        );
                    }
                } else if outcome.changed {
                    crate::alerts::resolve_token_refresh_failure(self, account_id);
                }
                Ok(outcome)
            }
            Err(error) => {
                if account_enabled {
                    crate::alerts::emit_token_refresh_failure(
                        self,
                        account_id,
                        account_name,
                        &crosslane_translate::sanitize_error_message(&error.to_string()),
                    );
                } else {
                    crate::alerts::resolve_token_refresh_failure(self, account_id);
                }
                Err(error)
            }
        }
    }

    async fn reload_account_credentials(
        &self,
        pool: &AccountPool,
        account_id: &str,
    ) -> Result<Option<ReloadedCredentials>, String> {
        let _config_mutation = self.lock_config_mutation().await;
        let _transaction = self
            .lock_account_storage()
            .await
            .map_err(|error| error.to_string())?;
        let mut disk = AccountStore::load(&self.paths.accounts_file)
            .await
            .map_err(|error| error.to_string())?;
        self.configure_account_store(&mut disk);
        self.validate_or_restore_account_store(&disk).await?;
        let candidate = disk
            .find(account_id)
            .cloned()
            .ok_or_else(|| format!("account not found in credential store: {account_id}"))?;
        let runtime = pool
            .get(account_id)
            .await
            .ok_or_else(|| format!("account not found: {account_id}"))?;
        let current = runtime.account.read().await.clone();
        let credentials_changed = serde_json::to_value(&candidate.credentials).ok()
            != serde_json::to_value(&current.credentials).ok()
            || candidate.profile_arn != current.profile_arn;
        if !credentials_changed {
            return Ok(None);
        }

        self.install_account_store(disk).await;
        Ok(Some(ReloadedCredentials {
            credentials: candidate.credentials,
            profile_arn: candidate.profile_arn,
        }))
    }

    /// Commits only the rotated credential fields for one account. Updating a
    /// single account avoids replaying a stale whole-pool snapshot over tokens
    /// refreshed concurrently by other accounts.
    async fn persist_refreshed_credentials(
        &self,
        pool: &AccountPool,
        refreshed: RefreshedCredentials,
    ) -> Result<(), String> {
        let _config_mutation = self.lock_config_mutation().await;
        let _transaction = self
            .lock_account_storage()
            .await
            .map_err(|error| error.to_string())?;
        let runtime = pool
            .get(&refreshed.account_id)
            .await
            .ok_or_else(|| format!("account not found: {}", refreshed.account_id))?;

        // Reassert the freshly rotated fields while holding the same mutation
        // lock as the file watcher. This closes the window where a delayed
        // disk reload could replace them between refresh and persistence.
        {
            let mut account = runtime.account.write().await;
            account.credentials = refreshed.credentials.clone();
            if let Some(profile_arn) = refreshed.profile_arn.as_ref() {
                account.profile_arn = Some(profile_arn.clone());
            }
        }

        // Always merge into the latest durable snapshot. This mirrors the
        // reference implementation's read-merge-write behavior and prevents
        // an external credential update from being overwritten by a stale
        // in-memory whole-store clone.
        let mut next = AccountStore::load(&self.paths.accounts_file)
            .await
            .map_err(|error| error.to_string())?;
        self.configure_account_store(&mut next);
        self.validate_or_restore_account_store(&next).await?;
        let credentials = refreshed.credentials;
        let profile_arn = refreshed.profile_arn;
        if !next.update(&refreshed.account_id, move |account| {
            account.credentials = credentials;
            if let Some(profile_arn) = profile_arn {
                account.profile_arn = Some(profile_arn);
            }
        }) {
            return Err(format!("account not found: {}", refreshed.account_id));
        }
        next.save().await.map_err(|error| error.to_string())?;
        self.install_account_store(next).await;
        Ok(())
    }

    pub(crate) fn configure_account_store(&self, store: &mut AccountStore) {
        let config = self.config.current();
        store.set_compression_threshold(config.storage.compression_threshold);
        store.set_incremental_write(config.storage.incremental_write);
    }

    /// Persists mutable runtime state without granting a whole-pool snapshot
    /// permission to overwrite durable credentials or operator-managed fields.
    /// The lock is acquired before taking the snapshot, and the snapshot is
    /// merged into the latest on-disk store.
    pub(crate) async fn persist_runtime_accounts(&self) -> anyhow::Result<()> {
        let _config_mutation = self.lock_config_mutation().await;
        let _transaction = self.lock_account_storage().await?;
        let snapshot = self.pool().snapshot().await;
        let mut next = AccountStore::load(&self.paths.accounts_file).await?;
        self.configure_account_store(&mut next);
        self.validate_or_restore_account_store(&next)
            .await
            .map_err(anyhow::Error::msg)?;
        for account in snapshot {
            merge_runtime_account(&mut next, &account);
        }
        next.save().await?;
        self.install_account_store(next).await;
        Ok(())
    }

    /// Refreshes every expiring account through the same cache-invalidation
    /// path as request-triggered and manual refreshes.
    pub async fn refresh_expiring_tokens(&self, pool: &AccountPool) -> TokenRefreshReport {
        let before_expiry = self
            .config
            .current()
            .effective_token_refresh_before_expiry();
        let accounts = pool
            .snapshot()
            .await
            .into_iter()
            .filter(|account| account.enabled)
            .collect::<Vec<_>>();
        let mut report = TokenRefreshReport {
            checked: accounts.len(),
            eligible: 0,
            refreshed: Vec::new(),
            failures: Vec::new(),
        };
        for account in accounts {
            if account.is_token_expiring(crate::meter::now_secs(), before_expiry) {
                report.eligible += 1;
                let account_id = account.id.clone();
                let account_name = account.display_name().to_owned();
                match self.refresh_account_token(pool, &account.id, false).await {
                    Ok(outcome) if outcome.changed && outcome.persisted() => {
                        report.refreshed.push((account_id, account_name));
                    }
                    Ok(outcome) if outcome.changed => {
                        report.failures.push((
                            account_id,
                            account_name,
                            RefreshError::Persistence(outcome.persistence_error.unwrap_or_else(
                                || "refreshed credentials were not persisted".into(),
                            )),
                        ))
                    }
                    Ok(_) => {}
                    Err(error) => report.failures.push((account_id, account_name, error)),
                }
            }
        }
        report
    }

    pub fn install_tls_config(&self, config: axum_server::tls_rustls::RustlsConfig) {
        *write_lock(&self.tls_config) = Some(config);
    }

    /// 将文件热重载同步到依赖配置构造的运行时组件。
    pub async fn apply_runtime_config(&self, next: &Config) {
        let previous = read_lock(&self.runtime_config).clone();
        if previous.models.disabled != next.models.disabled {
            self.model_policy.replace(&next.models).await;
        }
        if serde_json::to_string(&previous.log).ok() != serde_json::to_string(&next.log).ok() {
            if let Err(error) = crate::logging::reload_config(&next.log) {
                tracing::warn!(%error, "failed to hot-reload log configuration");
            }
        }
        self.meter.replace_configs(&next.api_key);
        self.meter.set_daily_limit(next.pool.daily_credit_limit);
        self.admission
            .set_maximum(next.server.max_concurrent_requests.max(1));
        self.adaptive_feedback.clear();
        self.connections
            .set_maximum(next.server.max_connections.max(1));
        match self.accounts.write() {
            Ok(mut accounts) => {
                accounts.set_compression_threshold(next.storage.compression_threshold);
                accounts.set_incremental_write(next.storage.incremental_write);
            }
            Err(poisoned) => {
                let mut accounts = poisoned.into_inner();
                accounts.set_compression_threshold(next.storage.compression_threshold);
                accounts.set_incremental_write(next.storage.incremental_write);
            }
        }
        if serde_json::to_string(&previous.pool).ok() != serde_json::to_string(&next.pool).ok() {
            self.pool().update_config(next.pool.clone());
        }
        let upstream_changed = serde_json::to_string(&previous.upstream).ok()
            != serde_json::to_string(&next.upstream).ok();
        if upstream_changed {
            let overrides = EndpointOverrides {
                codewhisperer_url: std::env::var("CROSSLANE_CODEWHISPERER_URL").ok(),
                amazonq_url: std::env::var("CROSSLANE_AMAZONQ_URL").ok(),
                mcp_url: std::env::var("CROSSLANE_MCP_URL").ok(),
                runtime_url: std::env::var("CROSSLANE_RUNTIME_URL").ok(),
                management_url: std::env::var("CROSSLANE_MANAGEMENT_URL").ok(),
            };
            if let Ok(client) = KiroClient::new(next.upstream.clone(), overrides) {
                *write_lock(&self.kiro) = client.with_model_policy(Arc::clone(&self.model_policy));
            }
        }
        if previous.upstream.token_refresh_before_expiry
            != next.upstream.token_refresh_before_expiry
            || previous.tasks.token_refresh_interval_ms != next.tasks.token_refresh_interval_ms
        {
            if let Ok(refresher) = TokenRefresher::new(next.effective_token_refresh_before_expiry())
            {
                *write_lock(&self.refresher) = refresher;
            }
        }
        if previous.server.tls.enabled
            && next.server.tls.enabled
            && serde_json::to_string(&previous.server.tls).ok()
                != serde_json::to_string(&next.server.tls).ok()
        {
            if let (Some(tls), Some(runtime)) = (
                read_lock(&self.tls_config).clone(),
                self.runtime_handle.as_ref(),
            ) {
                let next_tls = next.server.tls.clone();
                runtime.spawn(async move {
                    let result = if let (Some(cert), Some(key)) =
                        (next_tls.cert.as_ref(), next_tls.key.as_ref())
                    {
                        tls.reload_from_pem(cert.as_bytes().to_vec(), key.as_bytes().to_vec())
                            .await
                    } else if let (Some(cert), Some(key)) =
                        (next_tls.cert_path.as_ref(), next_tls.key_path.as_ref())
                    {
                        tls.reload_from_pem_file(cert, key).await
                    } else {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "TLS certificate and key are required",
                        ))
                    };
                    if let Err(error) = result {
                        tracing::error!(%error, "failed to hot-reload TLS certificate");
                    }
                });
            }
        }
        if serde_json::to_string(&previous.webhook).ok()
            != serde_json::to_string(&next.webhook).ok()
            || serde_json::to_string(&previous.notify).ok()
                != serde_json::to_string(&next.notify).ok()
        {
            let current = self.notifier();
            *write_lock(&self.notifier) =
                current.reconfigured(next.webhook.clone(), next.notify.clone(), 1_024);
        }
        *write_lock(&self.runtime_config) = next.clone();
    }

    /// Adjusts global admission from proxy-side queueing and explicit upstream
    /// overloads. Model generation time is deliberately excluded.
    pub async fn adjust_admission(
        &self,
        feedback: Option<AdaptiveFeedbackSnapshot>,
        queued_requests: usize,
    ) -> usize {
        let adaptive = self.config.current().server.adaptive.clone();
        let configured = self.config.current().server.max_concurrent_requests.max(1);
        if !adaptive.enabled {
            self.admission.set_maximum(configured);
            return configured;
        }
        let current = self.admission.maximum();
        let decrease_step = (current / 10).max(1);
        let increase_step = (configured / 100).max(1);
        let feedback = feedback.unwrap_or_default();
        let overload_rate = if feedback.attempts == 0 {
            0.0
        } else {
            feedback.overloads as f64 / feedback.attempts as f64
        };
        let enough_latency_samples = feedback.successful_samples >= adaptive.minimum_samples.max(1);
        let pressure = queued_requests > 0
            || (feedback.overloads > 0
                && feedback.attempts >= adaptive.minimum_samples.max(1)
                && overload_rate >= adaptive.overload_error_rate)
            || (enough_latency_samples
                && feedback.p99_stream_slot_wait_ms > adaptive.p99_degrade_ms);
        let healthy = queued_requests == 0
            && feedback.overloads == 0
            && enough_latency_samples
            && feedback.p99_stream_slot_wait_ms < adaptive.p99_recover_ms;
        let next = if pressure {
            current.saturating_sub(decrease_step).max(1)
        } else if healthy {
            current.saturating_add(increase_step).min(configured)
        } else {
            current.min(configured)
        };
        self.admission.set_maximum(next);
        next
    }
}

async fn quarantine_corrupt_state(path: &std::path::Path, kind: &str, error: &str) {
    if !tokio::fs::try_exists(path).await.unwrap_or(false) {
        return;
    }
    let timestamp = crate::meter::now_secs();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state");
    for suffix in 0..100u32 {
        let unique = if suffix == 0 {
            timestamp.to_string()
        } else {
            format!("{timestamp}-{suffix}")
        };
        let quarantine = path.with_file_name(format!("{file_name}.corrupt-{unique}"));
        if tokio::fs::try_exists(&quarantine).await.unwrap_or(true) {
            continue;
        }
        match tokio::fs::rename(path, &quarantine).await {
            Ok(()) => {
                warn!(
                    state_kind = kind,
                    source = %path.display(),
                    quarantine = %quarantine.display(),
                    reason = error,
                    "quarantined unreadable state file"
                );
                return;
            }
            Err(rename_error) => {
                warn!(
                    state_kind = kind,
                    path = %path.display(),
                    reason = error,
                    error = %rename_error,
                    "failed to quarantine unreadable state file"
                );
                return;
            }
        }
    }
    warn!(
        state_kind = kind,
        path = %path.display(),
        reason = error,
        "failed to quarantine unreadable state file because all backup names are occupied"
    );
}

async fn load_or_regenerate_replay_key(path: &std::path::Path) -> WebSearchReplayCodec {
    match tokio::fs::read_to_string(path).await {
        Ok(encoded) => match WebSearchReplayCodec::from_base64(&encoded) {
            Ok(codec) => return codec,
            Err(error) => quarantine_corrupt_state(path, "web-search replay key", &error).await,
        },
        Err(error) => {
            quarantine_corrupt_state(path, "web-search replay key", &error.to_string()).await
        }
    }

    let mut key = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut key);
    let encoded = format!("{}\n", URL_SAFE_NO_PAD.encode(key));
    if let Err(error) =
        crosslane_store::atomic::write_bytes_atomically(path, encoded.as_bytes(), Some(0o600)).await
    {
        warn!(
            path = %path.display(),
            %error,
            "failed to persist regenerated web-search replay key; using an ephemeral key"
        );
    } else {
        warn!(
            path = %path.display(),
            "regenerated web-search replay key; replay tokens issued before recovery are invalid"
        );
    }
    WebSearchReplayCodec::from_key(key)
}

/// Merges only fields owned by runtime probes and health tracking. In
/// particular, credentials and operator-managed metadata are never copied from
/// a pool snapshot. A resolved profile ARN is accepted only when the snapshot
/// still describes the same credential generation as durable storage.
fn merge_runtime_account(store: &mut AccountStore, runtime: &Account) -> bool {
    let Some(mut merged) = store.find(&runtime.id).cloned() else {
        return false;
    };
    let same_credentials = serde_json::to_value(&merged.credentials).ok()
        == serde_json::to_value(&runtime.credentials).ok();
    if same_credentials && runtime.profile_arn.is_some() {
        merged.profile_arn.clone_from(&runtime.profile_arn);
    }
    if merged.upstream_user_id.is_none() && runtime.upstream_user_id.is_some() {
        merged
            .upstream_user_id
            .clone_from(&runtime.upstream_user_id);
    }
    if runtime.usage.is_some() {
        merged.usage.clone_from(&runtime.usage);
    }
    if runtime.subscription.is_some() {
        merged.subscription.clone_from(&runtime.subscription);
    }
    merged.credit_exhausted = runtime.credit_exhausted;
    store.replace_if_changed(merged)
}

fn same_account_tags(left: &[String], right: &[String]) -> bool {
    left.iter().collect::<HashSet<_>>() == right.iter().collect::<HashSet<_>>()
}

fn validate_service_account_references(
    config: &Config,
    accounts: &[Account],
    data_dir: &std::path::Path,
) -> Result<(), String> {
    let known_ids = accounts
        .iter()
        .map(|account| account.id.as_str())
        .collect::<HashSet<_>>();
    for service in &config.proxy_service {
        // Manual inclusions must resolve to a usable account. Exclusions may
        // intentionally remain as tombstones after an unbound account is
        // deleted, preventing an account re-imported with the same ID from
        // silently rejoining this service.
        for account_id in &service.account_ids {
            let (provider_id, local_id) = account_id
                .split_once('/')
                .unwrap_or(("kiro", account_id.as_str()));
            let exists = if provider_id == "kiro" {
                known_ids.contains(local_id)
            } else {
                let provider = config
                    .effective_providers()
                    .into_iter()
                    .find(|p| p.id == provider_id && p.kind == "copilot");
                if provider.is_none() {
                    return Err(format!("unknown account provider {provider_id}"));
                }
                crosslane_core::provider::ProviderId::parse(provider_id)
                    .map_err(|error| error.to_string())?;
                let file = data_dir
                    .join("providers")
                    .join(provider_id)
                    .join("accounts.json");
                let value: serde_json::Value = std::fs::read(file)
                    .ok()
                    .and_then(|raw| serde_json::from_slice(&raw).ok())
                    .unwrap_or_default();
                // Legacy arrays and the provider-scoped persistence envelope.
                let items = value.get("accounts").unwrap_or(&value);
                let owner_matches = value.get("provider_id").is_none_or(|id| id == provider_id);
                owner_matches
                    && items
                        .as_array()
                        .is_some_and(|items| items.iter().any(|account| account["id"] == local_id))
            };
            if !exists {
                return Err(format!(
                    "proxy service {} ({}) references unknown account {account_id}",
                    service.name, service.id
                ));
            }
        }
    }
    Ok(())
}

fn validate_service_account_tag_transitions(
    previous: &Config,
    next: &Config,
) -> Result<(), String> {
    for current in &previous.proxy_service {
        let Some(updated) = next
            .proxy_service
            .iter()
            .find(|candidate| candidate.id == current.id)
        else {
            continue;
        };
        if !same_account_tags(
            current.selected_account_tags(),
            updated.selected_account_tags(),
        ) && current.enabled
        {
            return Err(format!(
                "proxy service {} ({}) must be disabled before changing account_tag",
                current.name, current.id
            ));
        }
    }
    Ok(())
}

fn format_service_failures(failures: &[(String, String)]) -> String {
    failures
        .iter()
        .map(|(service_id, error)| format!("{service_id}: {error}"))
        .collect::<Vec<_>>()
        .join("; ")
}

pub struct AdmissionGate {
    maximum: AtomicUsize,
    current: AtomicUsize,
}

impl AdmissionGate {
    fn new(maximum: usize) -> Self {
        Self {
            maximum: AtomicUsize::new(maximum.max(1)),
            current: AtomicUsize::new(0),
        }
    }

    pub fn maximum(&self) -> usize {
        self.maximum.load(Ordering::Acquire)
    }

    pub fn current(&self) -> usize {
        self.current.load(Ordering::Acquire)
    }

    pub fn set_maximum(&self, maximum: usize) {
        self.maximum.store(maximum.max(1), Ordering::Release);
    }

    pub fn try_acquire(self: &Arc<Self>) -> Option<AdmissionGuard> {
        self.current
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |current| {
                (current < self.maximum()).then_some(current + 1)
            })
            .ok()
            .map(|_| AdmissionGuard {
                _permit: Arc::new(AdmissionPermit {
                    gate: Arc::clone(self),
                }),
            })
    }
}

#[derive(Clone)]
pub struct AdmissionGuard {
    _permit: Arc<AdmissionPermit>,
}

struct AdmissionPermit {
    gate: Arc<AdmissionGate>,
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        self.gate.current.fetch_sub(1, Ordering::AcqRel);
    }
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct BodyBudget {
    maximum: usize,
    current: AtomicUsize,
}

impl BodyBudget {
    fn new(maximum: usize) -> Self {
        Self {
            maximum,
            current: AtomicUsize::new(0),
        }
    }

    pub fn reserve(self: &Arc<Self>, bytes: usize) -> Option<BodyGuard> {
        let result = self
            .current
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |current| {
                current
                    .checked_add(bytes)
                    .filter(|next| *next <= self.maximum)
            });
        result.ok().map(|_| BodyGuard {
            budget: Arc::clone(self),
            bytes,
        })
    }
}

pub struct BodyGuard {
    budget: Arc<BodyBudget>,
    bytes: usize,
}

impl BodyGuard {
    pub fn reserve_more(&mut self, bytes: usize) -> bool {
        if bytes == 0 {
            return true;
        }
        let reserved = self
            .budget
            .current
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |current| {
                current
                    .checked_add(bytes)
                    .filter(|next| *next <= self.budget.maximum)
            })
            .is_ok();
        if reserved {
            self.bytes = self.bytes.saturating_add(bytes);
        }
        reserved
    }
}

impl Drop for BodyGuard {
    fn drop(&mut self) {
        self.budget.current.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crosslane_core::account::{AuthMethod, Credentials, Usage};
    use serde_json::json;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[test]
    fn admission_clones_hold_one_slot_until_the_last_attempt_or_stream_finishes() {
        let gate = Arc::new(AdmissionGate::new(1));
        let request = gate.try_acquire().unwrap();
        let attempt = request.clone();
        let stream = attempt.clone();
        drop(attempt);
        assert_eq!(gate.current(), 1);
        assert!(gate.try_acquire().is_none());
        drop(request);
        assert_eq!(gate.current(), 1);
        drop(stream);
        assert_eq!(gate.current(), 0);
        assert!(gate.try_acquire().is_some());
    }

    fn refreshable_account(id: &str, email: &str, token: &str) -> Account {
        Account {
            id: id.into(),
            email: email.into(),
            label: None,
            enabled: true,
            machine_id: "a".repeat(64),
            profile_arn: None,
            upstream_user_id: None,
            credentials: Credentials {
                access_token: token.into(),
                refresh_token: Some(format!("{token}-refresh")),
                client_id: Some("client-id".into()),
                client_secret: Some("client-secret".into()),
                region: "us-east-1".into(),
                expires_at: 0,
                auth_method: AuthMethod::Idc,
            },
            usage: None,
            subscription: None,
            tags: Vec::new(),
            created_at: 0,
            credit_exhausted: false,
        }
    }

    #[test]
    fn summary_distinguishes_checks_from_actual_refreshes() {
        let report = TokenRefreshReport {
            checked: 3,
            eligible: 1,
            refreshed: vec![("acc_one".into(), "Team account".into())],
            failures: Vec::new(),
        };
        assert_eq!(
            report.summary(),
            "ok: 3 checked, 1 eligible, 1 refreshed, 0 failures"
        );
    }

    #[tokio::test]
    async fn runtime_persistence_cannot_apply_a_bound_accounts_external_tag_change() {
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
        let mut accounts = AccountStore::load(&paths.accounts_file)
            .await
            .expect("accounts");
        let mut account = refreshable_account("acc_00000001", "one@example.com", "token");
        account.tags = vec!["team-a".into()];
        accounts.insert(account).expect("account");
        accounts.save().await.expect("save account");
        let mut config = Config::default();
        config
            .proxy_service
            .push(crosslane_core::config::ProxyServiceConfig {
                web_search_provider: None,
                crs_relay: None,
                id: "svc_team_a".into(),
                name: "team-a".into(),
                host: "127.0.0.1".into(),
                port: 5580,
                enabled: false,
                skip_user_agent_check: false,
                api_key_ids: Vec::new(),
                account_tag: Some("team-a".into()),
                account_ids: Vec::new(),
                excluded_account_ids: Vec::new(),
                created_at: 0,
                default_provider: String::new(),
                allowed_providers: Vec::new(),
            });
        let state = AppState::new(paths.clone(), ConfigHandle::new(config), accounts);

        let mut external = AccountStore::load(&paths.accounts_file)
            .await
            .expect("external store");
        assert!(external.update("acc_00000001", |account| {
            account.tags = vec!["team-b".into()];
        }));
        external.save().await.expect("external save");

        let error = state
            .persist_runtime_accounts()
            .await
            .expect_err("bound tag change must stay rejected");

        assert!(error.to_string().contains("bound to proxy service"));
        assert_eq!(
            state.with_accounts(|store| store
                .find("acc_00000001")
                .expect("runtime account")
                .tags
                .clone()),
            ["team-a"]
        );
        let restored = AccountStore::load(&paths.accounts_file)
            .await
            .expect("restored account store");
        assert_eq!(
            restored
                .find("acc_00000001")
                .expect("restored account")
                .tags,
            ["team-a"]
        );
    }

    #[tokio::test]
    async fn rejected_api_keys_do_not_start_oauth_refresh_or_raise_refresh_incidents() {
        let server = MockServer::start().await;
        let directory = tempfile::tempdir().unwrap();
        let paths =
            Paths::from_env_values(Some(directory.path().to_str().unwrap()), None, None, None);
        crosslane_store::bootstrap::ensure_layout(&paths)
            .await
            .unwrap();
        let mut accounts = AccountStore::load(&paths.accounts_file).await.unwrap();
        let mut account =
            refreshable_account("acc_00000009", "headless@example.com", "ksk_synthetic");
        account.credentials.auth_method = AuthMethod::ApiKey;
        account.credentials.refresh_token = None;
        account.credentials.client_id = None;
        account.credentials.client_secret = None;
        accounts.insert(account).unwrap();
        accounts.save().await.unwrap();
        let mut config = Config::default();
        config.webhook.push(
            serde_json::from_value(json!({
                "name":"test", "kind":"custom", "url":server.uri(),
                "events":[crosslane_notify::WebhookEventKind::TokenRefreshFailed.as_str()]
            }))
            .unwrap(),
        );
        let state = AppState::new(paths, ConfigHandle::new(config), accounts);
        let pool = state.pool();
        assert!(matches!(
            state
                .refresh_account_token_after_auth_failure(&pool, "acc_00000009", "ksk_synthetic")
                .await,
            Err(RefreshError::NotRefreshable)
        ));
        assert!(matches!(
            state
                .refresh_account_token(&pool, "acc_00000009", true)
                .await,
            Err(RefreshError::NotRefreshable)
        ));
        assert!(!state.notifier().incident_active(
            crosslane_notify::WebhookEventKind::TokenRefreshFailed,
            Some("acc_00000009")
        ));
        assert!(
            !state
                .refresh_account_token(&pool, "acc_00000009", false)
                .await
                .unwrap()
                .changed
        );
        let runtime = pool.get("acc_00000009").await.unwrap();
        runtime.account.write().await.credentials.access_token = "ksk_rotated".into();
        let outcome = state
            .refresh_account_token_after_auth_failure(&pool, "acc_00000009", "ksk_synthetic")
            .await
            .unwrap();
        assert!(!outcome.changed);
        let mut entries = tokio::fs::read_dir(directory.path()).await.unwrap();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            assert!(!entry.file_name().to_string_lossy().contains(".refresh."));
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn refreshed_credentials_for_concurrent_accounts_are_committed_without_lost_updates() {
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
        let mut accounts = AccountStore::load(&paths.accounts_file)
            .await
            .expect("accounts");
        accounts
            .insert(refreshable_account(
                "acc_00000001",
                "one@example.com",
                "old-one",
            ))
            .expect("first account");
        accounts
            .insert(refreshable_account(
                "acc_00000002",
                "two@example.com",
                "old-two",
            ))
            .expect("second account");
        accounts.save().await.expect("save accounts");
        let state = AppState::new(
            paths.clone(),
            ConfigHandle::new(Config::default()),
            accounts,
        );
        let pool = state.pool();

        let first = RefreshedCredentials {
            account_id: "acc_00000001".into(),
            credentials: Credentials {
                access_token: "new-one".into(),
                refresh_token: Some("new-one-refresh".into()),
                client_id: Some("client-id".into()),
                client_secret: Some("client-secret".into()),
                region: "us-east-1".into(),
                expires_at: 3_000_000_000,
                auth_method: AuthMethod::Idc,
            },
            profile_arn: None,
        };
        let second = RefreshedCredentials {
            account_id: "acc_00000002".into(),
            credentials: Credentials {
                access_token: "new-two".into(),
                refresh_token: Some("new-two-refresh".into()),
                client_id: Some("client-id".into()),
                client_secret: Some("client-secret".into()),
                region: "us-east-1".into(),
                expires_at: 3_000_000_001,
                auth_method: AuthMethod::Idc,
            },
            profile_arn: None,
        };

        let (first_result, second_result) = tokio::join!(
            state.persist_refreshed_credentials(&pool, first),
            state.persist_refreshed_credentials(&pool, second),
        );
        first_result.expect("persist first account");
        second_result.expect("persist second account");

        let persisted = AccountStore::load(&paths.accounts_file)
            .await
            .expect("reload accounts");
        assert_eq!(
            persisted
                .find("acc_00000001")
                .expect("first persisted account")
                .credentials
                .refresh_token
                .as_deref(),
            Some("new-one-refresh")
        );
        assert_eq!(
            persisted
                .find("acc_00000002")
                .expect("second persisted account")
                .credentials
                .refresh_token
                .as_deref(),
            Some("new-two-refresh")
        );
    }

    #[tokio::test]
    async fn independent_daemons_serialize_rotating_refresh_token_consumption() {
        let server = MockServer::start().await;
        let attempts = Arc::new(AtomicUsize::new(0));
        let responder_attempts = Arc::clone(&attempts);
        Mock::given(method("POST"))
            .and(path("/refresh"))
            .respond_with(move |_request: &wiremock::Request| {
                if responder_attempts.fetch_add(1, Ordering::AcqRel) == 0 {
                    ResponseTemplate::new(200)
                        .set_delay(std::time::Duration::from_millis(150))
                        .set_body_raw(
                            r#"{"accessToken":"rotated-access","refreshToken":"rotated-refresh","expiresIn":3600}"#,
                            "application/json",
                        )
                } else {
                    ResponseTemplate::new(400).set_body_string("invalid_request")
                }
            })
            .expect(2)
            .mount(&server)
            .await;

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
        let mut initial = AccountStore::load(&paths.accounts_file)
            .await
            .expect("accounts");
        initial
            .insert(refreshable_account(
                "acc_00000001",
                "one@example.com",
                "old-access",
            ))
            .expect("account");
        initial.save().await.expect("save account");
        let first_store = AccountStore::load(&paths.accounts_file)
            .await
            .expect("first store");
        let second_store = AccountStore::load(&paths.accounts_file)
            .await
            .expect("second store");
        let first = AppState::new(
            paths.clone(),
            ConfigHandle::new(Config::default()),
            first_store,
        );
        let second = AppState::new(
            paths.clone(),
            ConfigHandle::new(Config::default()),
            second_store,
        );
        let endpoint = format!("{}/refresh", server.uri());
        *write_lock(&first.refresher) = TokenRefresher::new(300)
            .expect("first refresher")
            .with_endpoint(endpoint.clone());
        *write_lock(&second.refresher) = TokenRefresher::new(300)
            .expect("second refresher")
            .with_endpoint(endpoint);
        let first_pool = first.pool();
        let second_pool = second.pool();

        let (first_result, second_result) = tokio::join!(
            first.refresh_account_token_after_auth_failure(
                &first_pool,
                "acc_00000001",
                "old-access"
            ),
            second.refresh_account_token_after_auth_failure(
                &second_pool,
                "acc_00000001",
                "old-access"
            ),
        );
        assert!(first_result.expect("first refresh").changed);
        assert!(second_result.expect("second refresh").changed);
        assert_eq!(attempts.load(Ordering::Acquire), 2);

        let persisted = AccountStore::load(&paths.accounts_file)
            .await
            .expect("persisted store");
        let account = persisted.find("acc_00000001").expect("account");
        assert_eq!(account.credentials.access_token, "rotated-access");
        assert_eq!(
            account.credentials.refresh_token.as_deref(),
            Some("rotated-refresh")
        );
    }

    #[tokio::test]
    async fn stale_runtime_snapshot_cannot_roll_back_rotated_durable_credentials() {
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
        let mut accounts = AccountStore::load(&paths.accounts_file)
            .await
            .expect("accounts");
        let mut original = refreshable_account("acc_00000001", "one@example.com", "old-access");
        original.profile_arn = Some("old-profile".into());
        accounts.insert(original).expect("account");
        accounts.save().await.expect("save account");
        let state = AppState::new(
            paths.clone(),
            ConfigHandle::new(Config::default()),
            accounts,
        );

        // Simulate a credential rotation that reached durable storage while a
        // stale pool snapshot still contains the previous token generation.
        let mut durable = AccountStore::load(&paths.accounts_file)
            .await
            .expect("durable store");
        assert!(durable.update("acc_00000001", |account| {
            account.label = Some("operator-label".into());
            account.credentials.access_token = "rotated-access".into();
            account.credentials.refresh_token = Some("rotated-refresh".into());
            account.credentials.expires_at = 3_000_000_000;
            account.profile_arn = Some("rotated-profile".into());
        }));
        durable.save().await.expect("persist rotation");

        let runtime = state
            .pool()
            .get("acc_00000001")
            .await
            .expect("runtime account");
        {
            let mut stale = runtime.account.write().await;
            stale.usage = Some(Usage {
                current: 42.0,
                limit: 100.0,
                overage_cap: None,
                percent_used: 42.0,
                next_reset_date: None,
                updated_at: 123,
            });
        }
        state
            .persist_runtime_accounts()
            .await
            .expect("persist runtime fields");

        let persisted = AccountStore::load(&paths.accounts_file)
            .await
            .expect("reload persisted store");
        let account = persisted.find("acc_00000001").expect("account");
        assert_eq!(account.credentials.access_token, "rotated-access");
        assert_eq!(
            account.credentials.refresh_token.as_deref(),
            Some("rotated-refresh")
        );
        assert_eq!(account.profile_arn.as_deref(), Some("rotated-profile"));
        assert_eq!(account.label.as_deref(), Some("operator-label"));
        assert_eq!(
            account.usage.as_ref().map(|usage| usage.current),
            Some(42.0)
        );
        let runtime = state
            .pool()
            .get("acc_00000001")
            .await
            .expect("runtime account")
            .account
            .read()
            .await
            .clone();
        assert_eq!(runtime.credentials.access_token, "rotated-access");
        assert_eq!(
            runtime.credentials.refresh_token.as_deref(),
            Some("rotated-refresh")
        );
    }

    #[tokio::test]
    async fn proactive_idc_400_reloads_new_credentials_from_disk_before_retrying() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/refresh"))
            .and(body_json(json!({
                "clientId": "client-id",
                "clientSecret": "client-secret",
                "grantType": "refresh_token",
                "refreshToken": "old-access-refresh"
            })))
            .respond_with(ResponseTemplate::new(400).set_body_string("invalid_request"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/refresh"))
            .and(body_json(json!({
                "clientId": "new-client-id",
                "clientSecret": "new-client-secret",
                "grantType": "refresh_token",
                "refreshToken": "external-refresh"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                r#"{"accessToken":"recovered-access","refreshToken":"recovered-refresh","expiresIn":3600}"#,
                "application/json",
            ))
            .expect(1)
            .mount(&server)
            .await;

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
        let mut accounts = AccountStore::load(&paths.accounts_file)
            .await
            .expect("accounts");
        accounts
            .insert(refreshable_account(
                "acc_00000001",
                "one@example.com",
                "old-access",
            ))
            .expect("account");
        accounts.save().await.expect("save account");
        let state = AppState::new(
            paths.clone(),
            ConfigHandle::new(Config::default()),
            accounts,
        );
        let pool = state.pool();
        *write_lock(&state.refresher) = TokenRefresher::new(300)
            .expect("refresher")
            .with_endpoint(format!("{}/refresh", server.uri()));

        let mut external = AccountStore::load(&paths.accounts_file)
            .await
            .expect("external store");
        assert!(external.update("acc_00000001", |account| {
            account.credentials.refresh_token = Some("external-refresh".into());
            account.credentials.client_id = Some("new-client-id".into());
            account.credentials.client_secret = Some("new-client-secret".into());
        }));
        external.save().await.expect("external credential update");

        let outcome = state
            .refresh_account_token(&pool, "acc_00000001", false)
            .await
            .expect("refresh recovers");

        assert!(outcome.changed);
        assert!(outcome.persisted());
        let persisted = AccountStore::load(&paths.accounts_file)
            .await
            .expect("persisted store");
        let credentials = &persisted
            .find("acc_00000001")
            .expect("persisted account")
            .credentials;
        assert_eq!(credentials.access_token, "recovered-access");
        assert_eq!(
            credentials.refresh_token.as_deref(),
            Some("recovered-refresh")
        );
    }

    #[test]
    fn adaptive_feedback_uses_stream_slot_wait_and_overload_samples() {
        let feedback = AdaptiveFeedback::default();
        for wait_ms in [5, 10, 20, 50, 250] {
            feedback.record_success(wait_ms);
        }
        feedback.record_failure(true);

        let snapshot = feedback.take_if_ready(5).expect("feedback");
        assert_eq!(snapshot.attempts, 6);
        assert_eq!(snapshot.successful_samples, 5);
        assert_eq!(snapshot.overloads, 1);
        assert_eq!(snapshot.p99_stream_slot_wait_ms, 250);
        assert!(feedback.take_if_ready(5).is_none());
    }

    #[tokio::test]
    async fn adaptive_admission_ignores_generation_duration_and_tracks_pressure() {
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
        let mut config = Config::default();
        config.server.adaptive.enabled = true;
        let state = AppState::new(paths, ConfigHandle::new(config), accounts);

        state.admission.set_maximum(300);
        let healthy = AdaptiveFeedbackSnapshot {
            attempts: 5,
            successful_samples: 5,
            overloads: 0,
            p99_stream_slot_wait_ms: 20,
        };
        assert_eq!(state.adjust_admission(Some(healthy), 0).await, 305);

        let slot_pressure = AdaptiveFeedbackSnapshot {
            p99_stream_slot_wait_ms: 250,
            ..healthy
        };
        assert_eq!(state.adjust_admission(Some(slot_pressure), 0).await, 275);

        let upstream_pressure = AdaptiveFeedbackSnapshot {
            attempts: 5,
            successful_samples: 4,
            overloads: 1,
            p99_stream_slot_wait_ms: 20,
        };
        assert_eq!(
            state.adjust_admission(Some(upstream_pressure), 0).await,
            248
        );
        assert_eq!(state.adjust_admission(None, 1).await, 224);
        assert_eq!(state.adjust_admission(None, 0).await, 224);
    }

    #[tokio::test]
    async fn corrupt_auxiliary_state_starts_in_recoverable_mode() {
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
        tokio::fs::write(&paths.daily_file, "{not-json")
            .await
            .expect("corrupt daily");
        tokio::fs::write(&paths.stats_file, r#"{"total":{}}"#)
            .await
            .expect("corrupt stats");
        tokio::fs::write(&paths.web_search_replay_key_file, "not-a-valid-key")
            .await
            .expect("corrupt replay key");
        let accounts = AccountStore::load(&paths.accounts_file)
            .await
            .expect("accounts");

        let state = AppState::load(
            paths.clone(),
            ConfigHandle::new(Config::default()),
            accounts,
        )
        .await
        .expect("management state remains available");

        assert!(state.meter.recovery_error().is_some());
        assert_eq!(state.stats.snapshot(None).total.requests, 0);
        let mut entries = tokio::fs::read_dir(&paths.data_dir)
            .await
            .expect("data directory");
        let mut quarantined = Vec::new();
        while let Some(entry) = entries.next_entry().await.expect("directory entry") {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.contains(".corrupt-") {
                quarantined.push(name);
            }
        }
        assert_eq!(quarantined.len(), 3);
        let replay_key = tokio::fs::read_to_string(&paths.web_search_replay_key_file)
            .await
            .expect("regenerated replay key");
        WebSearchReplayCodec::from_base64(&replay_key).expect("valid regenerated replay key");
    }
}
