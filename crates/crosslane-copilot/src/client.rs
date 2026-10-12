use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock as StdRwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use crosslane_core::config::{
    ProviderBalanceConfig, ProviderConfig, DEFAULT_COPILOT_OAUTH_CLIENT_ID,
};
use crosslane_core::provider::{
    CapabilitySupport, ProviderCapabilities, ProviderDescriptor, ProviderId, ProviderModel,
    ProviderProtocol,
};
use crosslane_provider::{
    ProviderAdapter, ProviderError, ProviderErrorKind, ProviderRequest, ProviderResponse,
    ProviderResponseBody,
};
use futures::{stream, StreamExt};
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex, RwLock, Semaphore};
use url::Url;
use uuid::Uuid;

use crate::account::{
    CopilotAccount, CopilotAccountInfo, CopilotAccountSummary, CopilotCredentials,
    CopilotQuotaSnapshot,
};
use crate::auth::{DeviceLoginState, DeviceLoginStatus, DeviceLoginTask};

const ACCOUNTS_FILE_MODE: u32 = 0o600;
const ACCOUNTS_SCHEMA_VERSION: u32 = 1;
const MODEL_CACHE_SCHEMA_VERSION: u32 = 1;
const DEFAULT_COPILOT_API: &str = "https://api.githubcopilot.com";
const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const ACCOUNT_INFO_TTL_SECS: i64 = 300;
const FORWARDABLE_ANTHROPIC_BETAS: &[&str] = &[
    "claude-code-20250219",
    "context-1m-2025-08-07",
    "context-management-2025-06-27",
    "effort-2025-11-24",
    "fallback-credit-2026-06-01",
    "interleaved-thinking-2025-05-14",
    "mid-conversation-system-2026-04-07",
    "prompt-caching-scope-2026-01-05",
    "thinking-token-count-2026-05-13",
];

#[derive(Clone)]
pub struct CopilotSettings {
    pub github_host: String,
    pub github_api_base: String,
    pub oauth_base: String,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub refresh_before_secs: i64,
    pub max_concurrent_per_account: usize,
    pub balance: ProviderBalanceConfig,
    pub model_cache_ttl_secs: i64,
    pub model_max_stale_secs: i64,
    pub model_refresh_concurrency: usize,
    pub allow_insecure_http: bool,
    pub allowed_endpoint_hosts: Vec<String>,
    pub api_endpoint_fallback: Option<String>,
    pub editor_version: String,
    pub editor_plugin_version: String,
    pub integration_id: String,
    pub user_agent: String,
}

impl CopilotSettings {
    pub fn from_config(config: &ProviderConfig) -> Self {
        let string = |name: &str| {
            config
                .settings
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let number = |name: &str| config.settings.get(name).and_then(Value::as_u64);
        let boolean = |name: &str| config.settings.get(name).and_then(Value::as_bool);
        let allowed_endpoint_hosts = config
            .settings
            .get("allowed_endpoint_hosts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_ascii_lowercase)
            .collect();
        Self {
            github_host: string("github_host").unwrap_or_else(|| "github.com".into()),
            github_api_base: string("github_api_base")
                .unwrap_or_else(|| "https://api.github.com".into()),
            oauth_base: string("oauth_base").unwrap_or_else(|| "https://github.com".into()),
            client_id: config
                .settings
                .get("client_id")
                .map(|value| value.as_str().unwrap_or_default().to_owned()),
            client_secret: string("client_secret").filter(|value| !value.trim().is_empty()),
            refresh_before_secs: number("api_token_refresh_before_secs")
                .and_then(|value| i64::try_from(value).ok())
                .unwrap_or(300),
            max_concurrent_per_account: if config.pool.max_concurrent_per_account == 0 {
                2
            } else {
                config.pool.max_concurrent_per_account
            },
            balance: config.pool.balance.clone(),
            model_cache_ttl_secs: i64::try_from(config.models.cache_ttl_ms.div_ceil(1_000))
                .unwrap_or(i64::MAX),
            model_max_stale_secs: i64::try_from(config.models.max_stale_ms.div_ceil(1_000))
                .unwrap_or(i64::MAX),
            model_refresh_concurrency: config.settings.get("model_refresh_concurrency").map_or(
                32,
                |value| {
                    value
                        .as_u64()
                        .and_then(|value| usize::try_from(value).ok())
                        .unwrap_or(0)
                },
            ),
            allow_insecure_http: boolean("allow_insecure_http").unwrap_or(false),
            allowed_endpoint_hosts,
            api_endpoint_fallback: string("api_endpoint_fallback"),
            editor_version: string("editor_version").unwrap_or_else(|| "vscode/1.104.0".into()),
            editor_plugin_version: string("editor_plugin_version")
                .unwrap_or_else(|| "copilot-chat/0.31.0".into()),
            integration_id: string("integration_id").unwrap_or_else(|| "vscode-chat".into()),
            user_agent: string("user_agent").unwrap_or_else(|| "GitHubCopilotChat/0.31.0".into()),
        }
    }

    fn oauth_client_id(&self) -> &str {
        self.client_id
            .as_deref()
            .unwrap_or(DEFAULT_COPILOT_OAUTH_CLIENT_ID)
    }

    fn oauth_client_secret(&self) -> Option<&str> {
        if self.oauth_client_id() == DEFAULT_COPILOT_OAUTH_CLIENT_ID {
            None
        } else {
            self.client_secret.as_deref()
        }
    }

    fn validate(&self) -> Result<(), ProviderError> {
        let balance_weight_sum = self.balance.weight_active + self.balance.weight_idle;
        if !self.balance.weight_active.is_finite()
            || self.balance.weight_active < 0.0
            || !self.balance.weight_idle.is_finite()
            || self.balance.weight_idle < 0.0
            || !balance_weight_sum.is_finite()
            || balance_weight_sum <= 0.0
            || self.balance.idle_window_ms == 0
        {
            return Err(internal_error("invalid Copilot account balance settings"));
        }
        if self
            .client_id
            .as_ref()
            .is_some_and(|value| value.trim().is_empty() || value.contains(char::is_whitespace))
        {
            return Err(internal_error(
                "Copilot settings.client_id must be a non-empty string without whitespace",
            ));
        }
        if self.github_host.trim().is_empty()
            || self.github_host.contains('/')
            || self.github_host.contains(char::is_whitespace)
        {
            return Err(internal_error(
                "Copilot settings.github_host must be a non-empty hostname",
            ));
        }
        for (name, base) in [
            ("github_api_base", self.github_api_base.as_str()),
            ("oauth_base", self.oauth_base.as_str()),
        ] {
            let url = Url::parse(base)
                .map_err(|error| internal_error(format!("invalid Copilot {name}: {error}")))?;
            let allowed_scheme =
                url.scheme() == "https" || (self.allow_insecure_http && url.scheme() == "http");
            if !allowed_scheme || url.host_str().is_none() {
                return Err(internal_error(format!(
                    "Copilot {name} must be an absolute HTTPS URL"
                )));
            }
        }
        if self.model_cache_ttl_secs <= 0 || self.model_max_stale_secs < self.model_cache_ttl_secs {
            return Err(internal_error(
                "Copilot model max_stale must be greater than or equal to cache_ttl",
            ));
        }
        if self.model_refresh_concurrency == 0 {
            return Err(internal_error(
                "Copilot settings.model_refresh_concurrency must be greater than zero",
            ));
        }
        for host in &self.allowed_endpoint_hosts {
            if host.trim().is_empty() || host.contains('/') || host.contains(char::is_whitespace) {
                return Err(internal_error(
                    "Copilot allowed_endpoint_hosts entries must be hostnames",
                ));
            }
        }
        if let Some(endpoint) = &self.api_endpoint_fallback {
            self.validate_copilot_endpoint(endpoint)?;
        }
        for (name, value) in [
            ("editor_version", self.editor_version.as_str()),
            ("editor_plugin_version", self.editor_plugin_version.as_str()),
            ("integration_id", self.integration_id.as_str()),
            ("user_agent", self.user_agent.as_str()),
        ] {
            HeaderValue::try_from(value).map_err(|_| {
                internal_error(format!(
                    "Copilot settings.{name} is not a valid HTTP header"
                ))
            })?;
        }
        Ok(())
    }

    fn validate_copilot_endpoint(&self, endpoint: &str) -> Result<(), ProviderError> {
        let url = Url::parse(endpoint)
            .map_err(|error| internal_error(format!("invalid Copilot API endpoint: {error}")))?;
        if url.scheme() != "https" && !(self.allow_insecure_http && url.scheme() == "http") {
            return Err(internal_error(
                "Copilot API endpoint must use HTTPS (or an explicit test-only HTTP override)",
            ));
        }
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        let default_allowed = host == "github.com"
            || host.ends_with(".github.com")
            || host.ends_with(".githubcopilot.com")
            || host.ends_with(".githubusercontent.com");
        let configured_allowed = self
            .allowed_endpoint_hosts
            .iter()
            .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")));
        if !default_allowed && !configured_allowed && !self.allow_insecure_http {
            return Err(internal_error(format!(
                "Copilot API endpoint host {host} is not allowed"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct CachedApiToken {
    token: String,
    endpoint: String,
    expires_at: i64,
    refresh_in_secs: Option<u64>,
    sku: Option<String>,
}

impl CachedApiToken {
    fn update_info(&self, info: &mut CopilotAccountInfo) {
        info.api_token_expires_at = Some(self.expires_at);
        info.api_token_refresh_in_secs = self.refresh_in_secs;
        if self.sku.is_some() {
            info.sku.clone_from(&self.sku);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedAccounts {
    schema_version: u32,
    provider_id: String,
    accounts: Vec<CopilotAccount>,
}

#[derive(Debug, Deserialize)]
struct GithubUser {
    id: u64,
    login: String,
    email: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GithubEmail {
    email: String,
    verified: bool,
}

#[derive(Debug, Deserialize)]
struct CopilotTokenResponse {
    token: String,
    expires_at: i64,
    refresh_in: Option<u64>,
    sku: Option<String>,
    #[serde(default)]
    endpoints: CopilotEndpoints,
}

#[derive(Debug, Deserialize)]
struct CopilotQuotaResponse {
    copilot_plan: Option<String>,
    access_type_sku: Option<String>,
    token_based_billing: Option<bool>,
    quota_reset_date: Option<String>,
    quota_reset_date_utc: Option<String>,
    #[serde(default)]
    quota_snapshots: BTreeMap<String, CopilotQuotaSnapshot>,
}

impl CopilotQuotaResponse {
    fn update_info(self, info: &mut CopilotAccountInfo) {
        info.plan = self.copilot_plan;
        if self.access_type_sku.is_some() {
            info.sku = self.access_type_sku;
        }
        info.token_based_billing = self.token_based_billing;
        info.quota_reset_date = self.quota_reset_date_utc.or(self.quota_reset_date);
        info.quota_snapshots = self.quota_snapshots;
        info.quota_error = None;
    }
}

#[derive(Debug, Default, Deserialize)]
struct CopilotEndpoints {
    api: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DeviceTokenResponse {
    access_token: Option<String>,
    token_type: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    refresh_token_expires_in: Option<u64>,
    error: Option<String>,
    error_description: Option<String>,
    interval: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ModelCache {
    #[serde(default)]
    accounts: BTreeMap<String, AccountModelCache>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AccountModelCache {
    models: Vec<ProviderModel>,
    updated_at: i64,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedModelCache {
    schema_version: u32,
    provider_id: String,
    #[serde(default)]
    accounts: BTreeMap<String, AccountModelCache>,
}

struct AccountSlot {
    semaphore: Arc<Semaphore>,
    last_selected: StdMutex<Option<Instant>>,
}

impl AccountSlot {
    fn new(max_concurrent: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            last_selected: StdMutex::new(None),
        }
    }

    fn score(&self, max_concurrent: usize, balance: &ProviderBalanceConfig, now: Instant) -> f64 {
        let active = max_concurrent.saturating_sub(self.semaphore.available_permits());
        let active_factor = active as f64 / max_concurrent.max(1) as f64;
        let last_selected = *self
            .last_selected
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let idle_factor = last_selected.map_or(0.0, |last| {
            let idle_ms = now.saturating_duration_since(last).as_millis() as f64;
            1.0 - (idle_ms / balance.idle_window_ms.max(1) as f64).min(1.0)
        });
        balance.weight_active * active_factor + balance.weight_idle * idle_factor
    }

    fn mark_selected(&self) {
        *self
            .last_selected
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now());
    }
}

/// One configured Copilot provider instance with isolated accounts and token
/// cache.
pub struct CopilotProvider {
    model_policy: Arc<crosslane_provider::ModelPolicy>,
    id: ProviderId,
    enabled: bool,
    settings: CopilotSettings,
    http: reqwest::Client,
    accounts_path: PathBuf,
    model_cache_path: PathBuf,
    accounts: RwLock<Vec<CopilotAccount>>,
    mutation: Mutex<()>,
    account_info_refresh: Mutex<()>,
    token_cache: RwLock<HashMap<String, CachedApiToken>>,
    refresh_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    account_limits: StdMutex<HashMap<String, Arc<AccountSlot>>>,
    next_account: AtomicUsize,
    model_cache: RwLock<ModelCache>,
    model_refresh: Mutex<()>,
    device_logins: Mutex<HashMap<String, DeviceLoginTask>>,
    last_error: StdRwLock<Option<String>>,
    tokenizer: std::sync::OnceLock<Result<crosslane_translate::TokenCountCache, String>>,
}

impl CopilotProvider {
    pub async fn load(
        id: ProviderId,
        enabled: bool,
        settings: CopilotSettings,
        accounts_path: PathBuf,
    ) -> Result<Self, ProviderError> {
        settings.validate()?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| internal_error(format!("build Copilot HTTP client: {error}")))?;
        let accounts = load_accounts(&accounts_path, &id).await?;
        let model_cache_path = accounts_path.with_file_name("models.json");
        let model_cache = load_model_cache(&model_cache_path, &id).await?;
        let account_limits = accounts
            .iter()
            .map(|account| {
                (
                    account.id.clone(),
                    Arc::new(AccountSlot::new(settings.max_concurrent_per_account)),
                )
            })
            .collect();
        Ok(Self {
            model_policy: Arc::new(crosslane_provider::ModelPolicy::default()),
            tokenizer: std::sync::OnceLock::new(),
            id,
            enabled,
            settings,
            http,
            accounts_path,
            model_cache_path,
            accounts: RwLock::new(accounts),
            mutation: Mutex::new(()),
            account_info_refresh: Mutex::new(()),
            token_cache: RwLock::new(HashMap::new()),
            refresh_locks: Mutex::new(HashMap::new()),
            account_limits: StdMutex::new(account_limits),
            next_account: AtomicUsize::new(0),
            model_cache: RwLock::new(model_cache),
            model_refresh: Mutex::new(()),
            device_logins: Mutex::new(HashMap::new()),
            last_error: StdRwLock::new(None),
        })
    }

    pub fn id(&self) -> &ProviderId {
        &self.id
    }

    pub fn with_model_policy(mut self, policy: Arc<crosslane_provider::ModelPolicy>) -> Self {
        self.model_policy = policy;
        self
    }

    /// Trusted OAuth origin for browser-side credential entry.
    pub fn oauth_base(&self) -> &str {
        &self.settings.oauth_base
    }

    pub async fn account_summaries(&self) -> Vec<CopilotAccountSummary> {
        self.accounts
            .read()
            .await
            .iter()
            .map(|account| self.account_summary(account))
            .collect()
    }

    /// Cached, service-scoped capacity for an equivalent model and native
    /// protocol. Routing never refreshes an unrelated account's catalog.
    pub async fn routing_snapshot(
        &self,
        pool: &crosslane_core::config::ProxyServiceConfig,
        requested: &str,
        protocol: ProviderProtocol,
    ) -> Option<(String, usize, usize)> {
        let accounts = self.accounts.read().await;
        let cache = self.model_cache.read().await;
        let now = now_secs();
        let matching = accounts
            .iter()
            .filter(|account| {
                account.enabled
                    && pool.includes_provider_account(self.id.as_str(), &account.id, &account.tags)
            })
            .filter_map(|account| {
                let entry = cache.accounts.get(&account.id)?;
                cache_entry_is_usable(entry, now, self.settings.model_max_stale_secs)
                    .then_some((account, entry))
            })
            .collect::<Vec<_>>();
        let available = matching
            .iter()
            .flat_map(|(_, entry)| entry.models.iter())
            .filter(|model| !is_embedding_model(model) && model.protocols.contains(&protocol))
            .map(|model| model.id.clone())
            .collect::<Vec<_>>();
        let model = crosslane_translate::model::resolve_model_alias(requested, &available)?;
        let slots = self
            .account_limits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut capacity = 0usize;
        let mut active = 0usize;
        for (account, entry) in matching {
            if !account.supported_models.contains(&model)
                || !entry.models.iter().any(|candidate| {
                    candidate.id == model && candidate.protocols.contains(&protocol)
                })
            {
                continue;
            }
            capacity += self.settings.max_concurrent_per_account;
            if let Some(slot) = slots.get(&account.id) {
                active += self
                    .settings
                    .max_concurrent_per_account
                    .saturating_sub(slot.semaphore.available_permits());
            }
        }
        (capacity > 0).then_some((model, capacity, active))
    }

    /// Refresh optional metadata without changing account health. Bound the
    /// entire list query and reuse recent successes and failures for five minutes.
    pub async fn refresh_account_info(&self, account_id: Option<&str>, force: bool) {
        if !self.enabled {
            return;
        }
        let Ok(_refresh) = self.account_info_refresh.try_lock() else {
            return;
        };
        let now = now_secs();
        let accounts = self
            .accounts
            .read()
            .await
            .iter()
            .filter(|account| {
                account.enabled
                    && account_id.is_none_or(|id| account.id == id)
                    && (force
                        || account.info.quota_checked_at.is_none_or(|checked| {
                            now.saturating_sub(checked) >= ACCOUNT_INFO_TTL_SECS
                        })
                        || (account.info.quota_error.is_none()
                            && account
                                .info
                                .api_token_expires_at
                                .is_none_or(|expires| expires <= now)))
            })
            .cloned()
            .collect::<Vec<_>>();
        let refreshes =
            futures::stream::iter(accounts).for_each_concurrent(4, |account| async move {
                let result = tokio::time::timeout(Duration::from_secs(5), async {
                    self.api_token(&account).await?;
                    let current = self
                        .find_account(&account.id)
                        .await
                        .ok_or_else(|| not_found_account(&account.id))?;
                    self.github_get::<CopilotQuotaResponse>(
                        &current.credentials.github_access_token,
                        "/copilot_internal/user",
                        &[],
                    )
                    .await
                })
                .await;
                let _mutation = self.mutation.lock().await;
                let mut accounts = self.accounts.write().await;
                let mut next = accounts.clone();
                let Some(current) = next.iter_mut().find(|current| current.id == account.id) else {
                    return;
                };
                current.info.quota_checked_at = Some(now_secs());
                match result {
                    Ok(Ok(quota)) => quota.update_info(&mut current.info),
                    Ok(Err(error)) => {
                        current.info.quota_error = Some(format!(
                            "Copilot account metadata is unavailable (HTTP {})",
                            error.status
                        ));
                    }
                    Err(_) => {
                        current.info.quota_error =
                            Some("Copilot account metadata refresh timed out".into());
                    }
                }
                if let Err(error) = save_accounts(&self.accounts_path, &self.id, &next).await {
                    tracing::warn!(provider_id = %self.id, account_id = %account.id, %error,
                    "failed to persist Copilot account metadata");
                }
                *accounts = next;
            });
        // Large pools return their latest cached information within the CLI's
        // response window, instead of waiting for every upstream request.
        let _ = tokio::time::timeout(Duration::from_secs(8), refreshes).await;
    }

    pub async fn account(&self, id_or_login: &str) -> Option<CopilotAccountSummary> {
        self.accounts
            .read()
            .await
            .iter()
            .find(|account| account.id == id_or_login || account.login == id_or_login)
            .map(|account| self.account_summary(account))
    }

    pub async fn export_accounts(&self, redact: bool) -> Value {
        let accounts = self.accounts.read().await.clone();
        let mut value = serde_json::to_value(accounts).unwrap_or_else(|_| Value::Array(Vec::new()));
        if redact {
            for account in value.as_array_mut().into_iter().flatten() {
                if let Some(credentials) = account
                    .get_mut("credentials")
                    .and_then(Value::as_object_mut)
                {
                    for field in ["github_access_token", "github_refresh_token"] {
                        if credentials.contains_key(field) {
                            credentials.insert(field.into(), Value::String("[REDACTED]".into()));
                        }
                    }
                }
            }
        }
        value
    }

    pub async fn import_token(
        &self,
        token: String,
        label: Option<String>,
    ) -> Result<CopilotAccountSummary, ProviderError> {
        self.import_credentials(
            CopilotCredentials {
                github_access_token: token,
                github_refresh_token: None,
                github_expires_at: None,
                github_refresh_token_expires_at: None,
                token_type: "bearer".into(),
            },
            label,
            None,
        )
        .await
    }

    async fn import_credentials(
        &self,
        credentials: CopilotCredentials,
        label: Option<String>,
        expected_login: Option<&str>,
    ) -> Result<CopilotAccountSummary, ProviderError> {
        let user = self.github_user(&credentials.github_access_token).await?;
        if let Some(expected) = expected_login {
            let matches = if expected.contains('@') {
                self.github_verified_email(&credentials.github_access_token, expected)
                    .await?
            } else {
                user.login.eq_ignore_ascii_case(expected)
            };
            if !matches {
                return Err(ProviderError::new(
                    ProviderErrorKind::Authentication,
                    StatusCode::UNAUTHORIZED,
                    "authorized GitHub user does not match the requested browser-login username or verified email",
                ));
            }
        }
        let exchanged = self
            .exchange_token_value(&credentials.github_access_token)
            .await?;
        let models = self.fetch_models_with_token(&exchanged, None).await?;
        if models.is_empty() {
            return Err(ProviderError::unavailable(
                "Copilot token is valid but its model catalog is empty",
            ));
        }
        let model_ids = models
            .iter()
            .map(|model| model.id.clone())
            .collect::<Vec<_>>();
        let now = now_secs();
        let mut info = CopilotAccountInfo::default();
        exchanged.update_info(&mut info);
        let account = CopilotAccount {
            id: format!("acc_{}", &Uuid::new_v4().simple().to_string()[..8]),
            github_host: self.settings.github_host.clone(),
            github_user_id: user.id,
            login: user.login,
            email: user.email,
            label,
            tags: Vec::new(),
            enabled: true,
            credentials,
            created_at: now,
            updated_at: None,
            supported_models: model_ids,
            endpoint: Some(exchanged.endpoint.clone()),
            last_error: None,
            info,
        };
        let _mutation = self.mutation.lock().await;
        let mut accounts = self.accounts.write().await;
        let mut next = accounts.clone();
        if next.iter().any(|current| {
            current
                .github_host
                .eq_ignore_ascii_case(&account.github_host)
                && current.github_user_id == account.github_user_id
        }) {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                StatusCode::CONFLICT,
                format!("GitHub user {} is already configured", account.login),
            ));
        }
        next.push(account.clone());
        save_accounts(&self.accounts_path, &self.id, &next).await?;
        *accounts = next;
        drop(accounts);
        self.token_cache
            .write()
            .await
            .insert(account.id.clone(), exchanged);
        self.account_limits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                account.id.clone(),
                Arc::new(AccountSlot::new(self.settings.max_concurrent_per_account)),
            );
        let cache = {
            let mut cache = self.model_cache.write().await;
            cache.accounts.insert(
                account.id.clone(),
                AccountModelCache {
                    models,
                    updated_at: now,
                },
            );
            cache.clone()
        };
        if let Err(error) = save_model_cache(&self.model_cache_path, &self.id, &cache).await {
            tracing::warn!(
                provider_id = %self.id,
                account_id = %account.id,
                %error,
                "failed to persist initial Copilot model cache"
            );
        }
        Ok(self.account_summary(&account))
    }

    pub async fn set_account_enabled(
        &self,
        id_or_login: &str,
        enabled: bool,
    ) -> Result<CopilotAccountSummary, ProviderError> {
        let _mutation = self.mutation.lock().await;
        let mut accounts = self.accounts.write().await;
        let mut next = accounts.clone();
        let account = next
            .iter_mut()
            .find(|account| account.id == id_or_login || account.login == id_or_login)
            .ok_or_else(|| not_found_account(id_or_login))?;
        account.enabled = enabled;
        account.updated_at = Some(now_secs());
        let summary = self.account_summary(account);
        save_accounts(&self.accounts_path, &self.id, &next).await?;
        *accounts = next;
        if !enabled {
            self.token_cache.write().await.remove(&summary.id);
        }
        Ok(summary)
    }

    pub async fn update_account_tags(
        &self,
        id_or_login: &str,
        add: &[String],
        remove: &[String],
    ) -> Result<CopilotAccountSummary, ProviderError> {
        self.update_accounts_tags(&[id_or_login.to_owned()], add, remove)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| not_found_account(id_or_login))
    }

    /// Validate and persist a whole provider-local batch before publishing it.
    pub async fn update_accounts_tags(
        &self,
        ids: &[String],
        add: &[String],
        remove: &[String],
    ) -> Result<Vec<CopilotAccountSummary>, ProviderError> {
        let normalize = |tags: &[String]| -> Result<Vec<String>, ProviderError> {
            tags.iter()
                .map(|tag| {
                    let tag = tag.trim();
                    if tag.is_empty() {
                        Err(ProviderError::new(
                            ProviderErrorKind::InvalidRequest,
                            StatusCode::BAD_REQUEST,
                            "account tag must not be empty",
                        ))
                    } else {
                        Ok(tag.to_owned())
                    }
                })
                .collect()
        };
        let add = normalize(add)?;
        let remove = normalize(remove)?;
        let _mutation = self.mutation.lock().await;
        let mut accounts = self.accounts.write().await;
        let mut next = accounts.clone();
        let mut summaries = Vec::with_capacity(ids.len());
        let mut seen = BTreeSet::new();
        for id in ids {
            let account = next
                .iter_mut()
                .find(|account| account.id == *id || account.login == *id)
                .ok_or_else(|| not_found_account(id))?;
            if !seen.insert(account.id.clone()) {
                return Err(ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    StatusCode::BAD_REQUEST,
                    format!("account specified more than once: {}", account.id),
                ));
            }
            for tag in &add {
                if !account.tags.contains(tag) {
                    account.tags.push(tag.clone());
                }
            }
            account.tags.retain(|tag| !remove.contains(tag));
            account.tags.sort();
            account.tags.dedup();
            account.updated_at = Some(now_secs());
            summaries.push(self.account_summary(account));
        }
        save_accounts(&self.accounts_path, &self.id, &next).await?;
        *accounts = next;
        Ok(summaries)
    }

    pub async fn reset_account_health(
        &self,
        id_or_login: &str,
    ) -> Result<CopilotAccountSummary, ProviderError> {
        let _mutation = self.mutation.lock().await;
        let mut accounts = self.accounts.write().await;
        let mut next = accounts.clone();
        let account = next
            .iter_mut()
            .find(|account| account.id == id_or_login || account.login == id_or_login)
            .ok_or_else(|| not_found_account(id_or_login))?;
        account.last_error = None;
        account.updated_at = Some(now_secs());
        let summary = self.account_summary(account);
        save_accounts(&self.accounts_path, &self.id, &next).await?;
        *accounts = next;
        *self
            .last_error
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        Ok(summary)
    }

    pub async fn remove_account(
        &self,
        id_or_login: &str,
    ) -> Result<CopilotAccountSummary, ProviderError> {
        let _mutation = self.mutation.lock().await;
        let mut accounts = self.accounts.write().await;
        let mut next = accounts.clone();
        let index = next
            .iter()
            .position(|account| account.id == id_or_login || account.login == id_or_login)
            .ok_or_else(|| not_found_account(id_or_login))?;
        let account = next.remove(index);
        save_accounts(&self.accounts_path, &self.id, &next).await?;
        *accounts = next;
        self.token_cache.write().await.remove(&account.id);
        self.account_limits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&account.id);
        let cache = {
            let mut cache = self.model_cache.write().await;
            cache.accounts.remove(&account.id);
            cache.clone()
        };
        if let Err(error) = save_model_cache(&self.model_cache_path, &self.id, &cache).await {
            tracing::warn!(
                provider_id = %self.id,
                account_id = %account.id,
                %error,
                "failed to persist Copilot model-cache removal"
            );
        }
        Ok(self.account_summary(&account))
    }

    pub async fn refresh_account(
        &self,
        id_or_login: &str,
    ) -> Result<CopilotAccountSummary, ProviderError> {
        let account = self
            .find_account(id_or_login)
            .await
            .ok_or_else(|| not_found_account(id_or_login))?;
        self.token_cache.write().await.remove(&account.id);
        let token = self.api_token(&account).await?;
        let models = self.fetch_models_with_token(&token, None).await?;
        let ids = models
            .iter()
            .map(|model| model.id.clone())
            .collect::<Vec<_>>();
        let now = now_secs();
        let _mutation = self.mutation.lock().await;
        let mut accounts = self.accounts.write().await;
        let mut next = accounts.clone();
        let Some(current) = next.iter_mut().find(|candidate| candidate.id == account.id) else {
            drop(accounts);
            self.token_cache.write().await.remove(&account.id);
            return Err(not_found_account(id_or_login));
        };
        current.endpoint = Some(token.endpoint);
        current.supported_models = ids;
        current.last_error = None;
        current.updated_at = Some(now);
        let summary = self.account_summary(current);
        save_accounts(&self.accounts_path, &self.id, &next).await?;
        *accounts = next;
        drop(accounts);
        let cache = {
            let mut cache = self.model_cache.write().await;
            cache.accounts.insert(
                account.id.clone(),
                AccountModelCache {
                    models,
                    updated_at: now,
                },
            );
            cache.clone()
        };
        if let Err(error) = save_model_cache(&self.model_cache_path, &self.id, &cache).await {
            tracing::warn!(
                provider_id = %self.id,
                account_id = %account.id,
                %error,
                "failed to persist refreshed Copilot model cache"
            );
        }
        drop(_mutation);
        self.refresh_account_info(Some(&account.id), true).await;
        Ok(self.account(&account.id).await.unwrap_or(summary))
    }

    pub async fn start_device_login(
        &self,
        label: Option<String>,
    ) -> Result<DeviceLoginState, ProviderError> {
        self.start_device_login_for_user(label, None).await
    }

    pub async fn start_device_login_for_user(
        &self,
        label: Option<String>,
        expected_login: Option<String>,
    ) -> Result<DeviceLoginState, ProviderError> {
        let client_id = self.settings.oauth_client_id();
        let url = endpoint_url(&self.settings.oauth_base, "/login/device/code")?;
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", client_id)
            .append_pair("scope", "read:user user:email")
            .finish();
        let response = self
            .http
            .post(url)
            .timeout(Duration::from_secs(30))
            .header("accept", "application/json")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("user-agent", &self.settings.user_agent)
            .body(body)
            .send()
            .await
            .map_err(transport_error)?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(transport_error)?;
        if !status.is_success() {
            return Err(upstream_error(status, &bytes, "GitHub Device Flow start"));
        }
        let issued: DeviceCodeResponse = serde_json::from_slice(&bytes).map_err(|error| {
            internal_error(format!("decode GitHub device-code response: {error}"))
        })?;
        let now = now_secs();
        let interval = issued.interval.unwrap_or(5).max(1);
        let state = DeviceLoginState {
            id: format!("login_{}", Uuid::new_v4().simple()),
            provider_id: self.id.to_string(),
            user_code: issued.user_code,
            verification_uri: issued.verification_uri,
            expires_at: now.saturating_add(i64::try_from(issued.expires_in).unwrap_or(i64::MAX)),
            interval_secs: interval,
            status: DeviceLoginStatus::Pending,
            account_id: None,
            error: None,
        };
        self.device_logins.lock().await.insert(
            state.id.clone(),
            DeviceLoginTask {
                state: state.clone(),
                device_code: issued.device_code,
                next_poll_at: now,
                label,
                expected_login,
            },
        );
        Ok(state)
    }

    pub async fn poll_device_login(
        &self,
        task_id: &str,
    ) -> Result<DeviceLoginState, ProviderError> {
        let now = now_secs();
        let snapshot = {
            let mut tasks = self.device_logins.lock().await;
            let task = tasks.get_mut(task_id).ok_or_else(|| {
                ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    StatusCode::NOT_FOUND,
                    format!("unknown device login task {task_id}"),
                )
            })?;
            if task.state.status != DeviceLoginStatus::Pending {
                return Ok(task.state.clone());
            }
            if now >= task.state.expires_at {
                task.state.status = DeviceLoginStatus::Expired;
                task.state.error = Some("GitHub device code expired".into());
                return Ok(task.state.clone());
            }
            if now < task.next_poll_at {
                return Ok(task.state.clone());
            }
            task.next_poll_at = now.saturating_add(task.state.interval_secs as i64);
            (
                task.device_code.clone(),
                task.label.clone(),
                task.state.interval_secs,
                task.expected_login.clone(),
            )
        };
        let client_id = self.settings.oauth_client_id();
        let url = endpoint_url(&self.settings.oauth_base, "/login/oauth/access_token")?;
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", client_id)
            .append_pair("device_code", &snapshot.0)
            .append_pair("grant_type", "urn:ietf:params:oauth:grant-type:device_code")
            .finish();
        let response = self
            .http
            .post(url)
            .timeout(Duration::from_secs(30))
            .header("accept", "application/json")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("user-agent", &self.settings.user_agent)
            .body(body)
            .send()
            .await
            .map_err(transport_error)?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(transport_error)?;
        if !status.is_success() {
            return Err(upstream_error(status, &bytes, "GitHub Device Flow poll"));
        }
        let token: DeviceTokenResponse = serde_json::from_slice(&bytes).map_err(|error| {
            internal_error(format!("decode GitHub device-token response: {error}"))
        })?;
        if let Some(access_token) = token.access_token {
            let now = now_secs();
            let credentials = CopilotCredentials {
                github_access_token: access_token,
                github_refresh_token: token.refresh_token,
                github_expires_at: token
                    .expires_in
                    .map(|seconds| now.saturating_add(i64::try_from(seconds).unwrap_or(i64::MAX))),
                github_refresh_token_expires_at: token
                    .refresh_token_expires_in
                    .map(|seconds| now.saturating_add(i64::try_from(seconds).unwrap_or(i64::MAX))),
                token_type: token.token_type.unwrap_or_else(|| "bearer".into()),
            };
            match self
                .import_credentials(credentials, snapshot.1, snapshot.3.as_deref())
                .await
            {
                Ok(account) => {
                    let mut tasks = self.device_logins.lock().await;
                    let task = tasks.get_mut(task_id).ok_or_else(|| {
                        internal_error("device login task disappeared during authorization")
                    })?;
                    task.state.status = DeviceLoginStatus::Authorized;
                    task.state.account_id = Some(account.id);
                    return Ok(task.state.clone());
                }
                Err(error) => {
                    let mut tasks = self.device_logins.lock().await;
                    let task = tasks.get_mut(task_id).ok_or_else(|| {
                        internal_error("device login task disappeared during authorization")
                    })?;
                    task.state.status = DeviceLoginStatus::Failed;
                    task.state.error = Some(error.to_string());
                    return Ok(task.state.clone());
                }
            }
        }
        let error_code = token.error.as_deref().unwrap_or("authorization_pending");
        let mut tasks = self.device_logins.lock().await;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| internal_error("device login task disappeared during poll"))?;
        match error_code {
            "authorization_pending" => {}
            "slow_down" => {
                task.state.interval_secs = token
                    .interval
                    .unwrap_or(task.state.interval_secs.saturating_add(5))
                    .max(task.state.interval_secs.saturating_add(5));
                task.next_poll_at = now.saturating_add(task.state.interval_secs as i64);
            }
            "access_denied" => {
                task.state.status = DeviceLoginStatus::Denied;
                task.state.error = token
                    .error_description
                    .or_else(|| Some("GitHub device authorization was denied".into()));
            }
            "expired_token" => {
                task.state.status = DeviceLoginStatus::Expired;
                task.state.error = token
                    .error_description
                    .or_else(|| Some("GitHub device code expired".into()));
            }
            other => {
                task.state.status = DeviceLoginStatus::Failed;
                task.state.error = Some(
                    token
                        .error_description
                        .unwrap_or_else(|| format!("GitHub Device Flow failed: {other}")),
                );
            }
        }
        Ok(task.state.clone())
    }

    pub async fn cancel_device_login(
        &self,
        task_id: &str,
    ) -> Result<DeviceLoginState, ProviderError> {
        let mut tasks = self.device_logins.lock().await;
        let task = tasks.get_mut(task_id).ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                StatusCode::NOT_FOUND,
                format!("unknown device login task {task_id}"),
            )
        })?;
        if task.state.status == DeviceLoginStatus::Pending {
            task.state.status = DeviceLoginStatus::Cancelled;
            task.device_code.clear();
        }
        Ok(task.state.clone())
    }

    async fn find_account(&self, id_or_login: &str) -> Option<CopilotAccount> {
        self.accounts
            .read()
            .await
            .iter()
            .find(|account| account.id == id_or_login || account.login == id_or_login)
            .cloned()
    }

    fn account_summary(&self, account: &CopilotAccount) -> CopilotAccountSummary {
        let auth_state = if account
            .credentials
            .github_expires_at
            .is_some_and(|expires_at| expires_at <= now_secs())
            && account.credentials.github_refresh_token.is_none()
        {
            "reauth_required"
        } else {
            "ready"
        };
        let mut info = account.info.clone();
        info.github_token_expires_at = account.credentials.github_expires_at;
        info.github_refresh_token_expires_at = account.credentials.github_refresh_token_expires_at;
        CopilotAccountSummary {
            provider_id: self.id.to_string(),
            provider_kind: "copilot".into(),
            id: account.id.clone(),
            login: account.login.clone(),
            email: account.email.clone(),
            label: account.label.clone(),
            enabled: account.enabled,
            auth_state: auth_state.into(),
            tags: account.tags.clone(),
            supported_models: account.supported_models.clone(),
            endpoint: account.endpoint.clone(),
            created_at: account.created_at,
            last_error: account.last_error.clone(),
            info,
        }
    }

    async fn set_account_runtime_error(&self, account_id: &str, error: Option<String>) {
        if let Some(account) = self
            .accounts
            .write()
            .await
            .iter_mut()
            .find(|account| account.id == account_id)
        {
            account.last_error = error;
        }
    }

    fn set_provider_runtime_error(&self, error: Option<String>) {
        *self
            .last_error
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = error;
    }

    async fn github_user(&self, token: &str) -> Result<GithubUser, ProviderError> {
        self.github_get(token, "/user", &[]).await
    }

    async fn github_verified_email(
        &self,
        token: &str,
        expected: &str,
    ) -> Result<bool, ProviderError> {
        // Check private/secondary emails as well as the public /user email.
        // Device Flow already requests user:email; never accept an unverified address.
        for page in 1..=10 {
            let emails: Vec<GithubEmail> = self
                .github_get(token, "/user/emails", &[("per_page", 100), ("page", page)])
                .await?;
            if emails
                .iter()
                .any(|email| email.verified && email.email.eq_ignore_ascii_case(expected))
            {
                return Ok(true);
            }
            if emails.len() < 100 {
                break;
            }
        }
        Ok(false)
    }

    async fn github_get<T: serde::de::DeserializeOwned>(
        &self,
        token: &str,
        path: &str,
        query: &[(&str, u32)],
    ) -> Result<T, ProviderError> {
        let url = endpoint_url(&self.settings.github_api_base, path)?;
        let response = self
            .http
            .get(url)
            .query(query)
            .timeout(Duration::from_secs(30))
            .header("accept", "application/vnd.github+json")
            .header("authorization", format!("Bearer {token}"))
            .header("x-github-api-version", "2022-11-28")
            .header("user-agent", &self.settings.user_agent)
            .send()
            .await
            .map_err(transport_error)?;
        decode_json_response(response, &format!("GitHub {path}")).await
    }

    async fn api_token(&self, account: &CopilotAccount) -> Result<CachedApiToken, ProviderError> {
        let now = now_secs();
        if let Some(cached) = self.token_cache.read().await.get(&account.id).cloned() {
            if cached.expires_at > now.saturating_add(self.settings.refresh_before_secs) {
                return Ok(cached);
            }
        }
        let refresh_lock = {
            let mut locks = self.refresh_locks.lock().await;
            Arc::clone(
                locks
                    .entry(account.id.clone())
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };
        let _refresh = refresh_lock.lock().await;
        if let Some(cached) = self.token_cache.read().await.get(&account.id).cloned() {
            if cached.expires_at > now_secs().saturating_add(self.settings.refresh_before_secs) {
                return Ok(cached);
            }
        }
        let github_token = if account
            .credentials
            .github_expires_at
            .is_some_and(|expires_at| {
                expires_at <= now_secs().saturating_add(self.settings.refresh_before_secs)
            }) {
            if account.credentials.github_refresh_token.is_some() {
                self.refresh_github_credentials(account).await?
            } else if account
                .credentials
                .github_expires_at
                .is_some_and(|expires_at| expires_at <= now_secs())
            {
                return Err(ProviderError::new(
                    ProviderErrorKind::Authentication,
                    StatusCode::UNAUTHORIZED,
                    format!("Copilot account {} requires reauthorization", account.login),
                ));
            } else {
                account.credentials.github_access_token.clone()
            }
        } else {
            account.credentials.github_access_token.clone()
        };
        let token = self.exchange_token_value(&github_token).await?;
        self.token_cache
            .write()
            .await
            .insert(account.id.clone(), token.clone());
        let _mutation = self.mutation.lock().await;
        let mut accounts = self.accounts.write().await;
        let mut next = accounts.clone();
        if let Some(current) = next.iter_mut().find(|current| current.id == account.id) {
            token.update_info(&mut current.info);
            if let Err(error) = save_accounts(&self.accounts_path, &self.id, &next).await {
                tracing::warn!(provider_id = %self.id, account_id = %account.id, %error,
                    "failed to persist Copilot token deadline");
            }
            *accounts = next;
        }
        Ok(token)
    }

    async fn refresh_github_credentials(
        &self,
        account: &CopilotAccount,
    ) -> Result<String, ProviderError> {
        if account
            .credentials
            .github_refresh_token_expires_at
            .is_some_and(|expires_at| expires_at <= now_secs())
        {
            return Err(ProviderError::new(
                ProviderErrorKind::Authentication,
                StatusCode::UNAUTHORIZED,
                format!("Copilot account {} requires reauthorization", account.login),
            ));
        }
        let refresh_token = account
            .credentials
            .github_refresh_token
            .as_deref()
            .ok_or_else(|| internal_error("GitHub refresh token is missing"))?;
        let client_id = self.settings.oauth_client_id();
        let mut form = vec![
            ("client_id", client_id),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ];
        if let Some(client_secret) = self.settings.oauth_client_secret() {
            form.push(("client_secret", client_secret));
        }
        let url = endpoint_url(&self.settings.oauth_base, "/login/oauth/access_token")?;
        let response = self
            .http
            .post(url)
            .header("accept", "application/json")
            .header("user-agent", &self.settings.user_agent)
            .form(&form)
            .send()
            .await
            .map_err(transport_error)?;
        let refreshed: DeviceTokenResponse =
            decode_json_response(response, "GitHub token refresh").await?;
        if let Some(error) = refreshed.error {
            return Err(ProviderError::new(
                ProviderErrorKind::Authentication,
                StatusCode::UNAUTHORIZED,
                refreshed.error_description.unwrap_or(error),
            ));
        }
        let access_token = refreshed
            .access_token
            .ok_or_else(|| internal_error("GitHub token refresh returned no access_token"))?;
        let now = now_secs();
        let credentials = CopilotCredentials {
            github_access_token: access_token.clone(),
            github_refresh_token: refreshed
                .refresh_token
                .or_else(|| account.credentials.github_refresh_token.clone()),
            github_expires_at: refreshed
                .expires_in
                .map(|seconds| now.saturating_add(i64::try_from(seconds).unwrap_or(i64::MAX))),
            github_refresh_token_expires_at: refreshed
                .refresh_token_expires_in
                .map(|seconds| now.saturating_add(i64::try_from(seconds).unwrap_or(i64::MAX)))
                .or(account.credentials.github_refresh_token_expires_at),
            token_type: refreshed.token_type.unwrap_or_else(|| "bearer".into()),
        };
        let _mutation = self.mutation.lock().await;
        let mut accounts = self.accounts.write().await;
        let mut next = accounts.clone();
        let current = next
            .iter_mut()
            .find(|candidate| candidate.id == account.id)
            .ok_or_else(|| not_found_account(&account.id))?;
        current.credentials = credentials;
        current.updated_at = Some(now);
        save_accounts(&self.accounts_path, &self.id, &next).await?;
        *accounts = next;
        Ok(access_token)
    }

    async fn exchange_token_value(
        &self,
        github_token: &str,
    ) -> Result<CachedApiToken, ProviderError> {
        let url = endpoint_url(&self.settings.github_api_base, "/copilot_internal/v2/token")?;
        let mut headers = self.identity_headers();
        insert_header(&mut headers, "accept", "application/json");
        insert_header(
            &mut headers,
            "authorization",
            &format!("token {github_token}"),
        );
        let response = self
            .http
            .get(url)
            .timeout(Duration::from_secs(30))
            .headers(headers)
            .send()
            .await
            .map_err(transport_error)?;
        let token: CopilotTokenResponse =
            decode_json_response(response, "Copilot token exchange").await?;
        let endpoint = token
            .endpoints
            .api
            .or_else(|| self.settings.api_endpoint_fallback.clone())
            .unwrap_or_else(|| DEFAULT_COPILOT_API.into());
        self.settings.validate_copilot_endpoint(&endpoint)?;
        if token.token.trim().is_empty() || token.expires_at <= now_secs() {
            return Err(internal_error(
                "Copilot token exchange returned an empty or expired token",
            ));
        }
        Ok(CachedApiToken {
            token: token.token,
            endpoint,
            expires_at: token.expires_at,
            refresh_in_secs: token.refresh_in,
            sku: token.sku,
        })
    }

    async fn fetch_models_with_token(
        &self,
        token: &CachedApiToken,
        account_id: Option<&str>,
    ) -> Result<Vec<ProviderModel>, ProviderError> {
        let url = endpoint_url(&token.endpoint, "/models")?;
        let response = self
            .http
            .get(url)
            .timeout(Duration::from_secs(30))
            .headers(self.upstream_headers(
                &token.token,
                None,
                ProviderProtocol::OpenAiChat,
                false,
                false,
            ))
            .send()
            .await
            .map_err(transport_error)?;
        let value: Value = decode_json_response(response, "Copilot model discovery").await?;
        parse_models(&self.id, &value, account_id)
    }

    fn upstream_headers(
        &self,
        token: &str,
        incoming: Option<&HeaderMap>,
        protocol: ProviderProtocol,
        streaming: bool,
        vision: bool,
    ) -> HeaderMap {
        let mut headers = self.identity_headers();
        insert_header(&mut headers, "authorization", &format!("Bearer {token}"));
        insert_header(
            &mut headers,
            "accept",
            if streaming {
                "text/event-stream"
            } else {
                "application/json"
            },
        );
        insert_header(&mut headers, "content-type", "application/json");
        if let Some(incoming) = incoming {
            if protocol == ProviderProtocol::ClaudeMessages {
                let version = incoming
                    .get("anthropic-version")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or(DEFAULT_ANTHROPIC_VERSION);
                insert_header(&mut headers, "anthropic-version", version);
                let beta = incoming
                    .get("anthropic-beta")
                    .and_then(|value| value.to_str().ok())
                    .map(forwardable_anthropic_betas)
                    .unwrap_or_default();
                if !beta.is_empty() {
                    insert_header(&mut headers, "anthropic-beta", &beta);
                }
            }
            for name in [
                "openai-beta",
                "x-initiator",
                "x-copilot-agent",
                "x-interaction-type",
            ] {
                if let Some(value) = incoming.get(name) {
                    if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
                        headers.insert(name, value.clone());
                    }
                }
            }
        } else if protocol == ProviderProtocol::ClaudeMessages {
            insert_header(&mut headers, "anthropic-version", DEFAULT_ANTHROPIC_VERSION);
        }
        if vision {
            insert_header(&mut headers, "copilot-vision-request", "true");
        }
        headers
    }

    fn identity_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        insert_header(&mut headers, "user-agent", &self.settings.user_agent);
        insert_header(
            &mut headers,
            "editor-version",
            &self.settings.editor_version,
        );
        insert_header(
            &mut headers,
            "editor-plugin-version",
            &self.settings.editor_plugin_version,
        );
        insert_header(
            &mut headers,
            "copilot-integration-id",
            &self.settings.integration_id,
        );
        headers
    }

    async fn pool_account_ids(
        &self,
        pool: Option<&crosslane_core::config::ProxyServiceConfig>,
    ) -> Option<BTreeSet<String>> {
        let pool = pool?;
        Some(
            self.accounts
                .read()
                .await
                .iter()
                .filter(|account| {
                    pool.includes_provider_account(self.id.as_str(), &account.id, &account.tags)
                })
                .map(|account| account.id.clone())
                .collect(),
        )
    }

    async fn acquire_account(
        &self,
        excluded: &[String],
        model: &str,
        protocol: ProviderProtocol,
        native_only: bool,
        account_ids: Option<&BTreeSet<String>>,
    ) -> Result<
        (
            CopilotAccount,
            tokio::sync::OwnedSemaphorePermit,
            ProviderModel,
        ),
        ProviderError,
    > {
        let now = now_secs();
        let capable_accounts = self
            .model_cache
            .read()
            .await
            .accounts
            .iter()
            .filter_map(|(account_id, entry)| {
                if !cache_entry_is_usable(entry, now, self.settings.model_max_stale_secs) {
                    return None;
                }
                entry
                    .models
                    .iter()
                    .find(|candidate| {
                        candidate.id == model
                            && !is_embedding_model(candidate)
                            && if native_only {
                                candidate.protocols.contains(&protocol)
                            } else {
                                !candidate.protocols.is_empty()
                            }
                    })
                    .map(|candidate| (account_id.clone(), candidate.clone()))
            })
            .collect::<BTreeMap<_, _>>();
        let mut accounts = self
            .accounts
            .read()
            .await
            .iter()
            .filter(|account| {
                account.enabled
                    && account_ids.is_none_or(|ids| ids.contains(&account.id))
                    && !excluded.contains(&account.id)
                    && account
                        .supported_models
                        .iter()
                        .any(|candidate| candidate == model)
                    && capable_accounts.contains_key(&account.id)
            })
            .cloned()
            .collect::<Vec<_>>();
        // Native-capable accounts preserve fields with no portable equivalent
        // (for example Responses state); do not randomly adapt those requests.
        if accounts
            .iter()
            .any(|account| capable_accounts[&account.id].protocols.contains(&protocol))
        {
            accounts.retain(|account| capable_accounts[&account.id].protocols.contains(&protocol));
        }
        if accounts.is_empty() {
            let mut error = ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                StatusCode::BAD_REQUEST,
                format!(
                    "model {model} does not support {} on an enabled account for provider {}",
                    protocol.as_str(),
                    self.id
                ),
            );
            error.upstream_code = Some("model_not_available".into());
            return Err(error);
        }
        let start = self.next_account.fetch_add(1, Ordering::Relaxed) % accounts.len();
        let candidates = {
            let limits = self
                .account_limits
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (0..accounts.len())
                .filter_map(|offset| {
                    let account = accounts[(start + offset) % accounts.len()].clone();
                    limits
                        .get(&account.id)
                        .map(|limit| (offset, account, Arc::clone(limit)))
                })
                .collect::<Vec<_>>()
        };
        if candidates.is_empty() {
            return Err(ProviderError::unavailable(format!(
                "provider {} has no initialized Copilot account",
                self.id
            )));
        }
        // Prefer a less busy account, then one used less recently. The
        // rotating offset only breaks equal-score ties. Copilot does not
        // expose a reliable per-account remaining-credit balance.
        let now = Instant::now();
        // Snapshot each score once so concurrent requests cannot change the
        // comparator's ordering while this request is sorting candidates.
        let mut candidates = candidates
            .into_iter()
            .map(|(offset, account, slot)| {
                let score = slot.score(
                    self.settings.max_concurrent_per_account,
                    &self.settings.balance,
                    now,
                );
                (score, offset, account, slot)
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            left.0
                .total_cmp(&right.0)
                .then_with(|| left.1.cmp(&right.1))
        });
        for (_, _, account, slot) in &candidates {
            if let Ok(permit) = Arc::clone(&slot.semaphore).try_acquire_owned() {
                slot.mark_selected();
                return Ok((
                    account.clone(),
                    permit,
                    capable_accounts[&account.id].clone(),
                ));
            }
        }
        // When all slots are occupied, wake for whichever account frees first
        // instead of queuing behind one long-running stream.
        let waiters = candidates
            .iter()
            .map(|(_, _, _, slot)| Box::pin(Arc::clone(&slot.semaphore).acquire_owned()))
            .collect::<Vec<_>>();
        let (permit, index, _) = futures::future::select_all(waiters).await;
        let (_, _, account, slot) = candidates.swap_remove(index);
        let permit = permit.map_err(|_| {
            ProviderError::unavailable(format!(
                "account {} concurrency limiter is closed",
                account.id
            ))
        })?;
        slot.mark_selected();
        let model = capable_accounts[&account.id].clone();
        Ok((account, permit, model))
    }

    async fn execute_path(
        &self,
        request: ProviderRequest,
        fixed_path: Option<&str>,
    ) -> Result<ProviderResponse, ProviderError> {
        drop(
            self.model_policy
                .admit(&[
                    &request.model,
                    request.original_model.as_deref().unwrap_or(&request.model),
                ])
                .await?,
        );
        if !self.enabled {
            return Err(ProviderError::unavailable(format!(
                "provider {} is disabled",
                self.id
            )));
        }
        let account_count = self
            .accounts
            .read()
            .await
            .iter()
            .filter(|account| {
                account.enabled
                    && account
                        .supported_models
                        .iter()
                        .any(|candidate| candidate == &request.model)
            })
            .count();
        // Freeze membership for this request, including failover retries.
        let account_ids = self.pool_account_ids(request.account_pool.as_ref()).await;
        let mut excluded = Vec::new();
        let mut last_error = None;
        for _ in 0..account_count.max(1) {
            let (account, permit, model) = match self
                .acquire_account(
                    &excluded,
                    &request.model,
                    request.protocol,
                    fixed_path.is_some(),
                    account_ids.as_ref(),
                )
                .await
            {
                Ok(acquired) => acquired,
                Err(_) if last_error.is_some() => break,
                Err(error) => return Err(error),
            };
            excluded.push(account.id.clone());
            tracing::info!(
                event = "upstream.account.selected",
                trace_id = %request.trace_id,
                service_id = %request.service_id,
                provider_id = %self.id,
                account_id = %account.id,
                model = %request.model,
                attempt = excluded.len(),
                max_attempts = account_count.max(1),
                "upstream account selected"
            );
            let upstream_protocol = if model.protocols.contains(&request.protocol) {
                request.protocol
            } else {
                model.protocols[0]
            };
            let upstream_path = fixed_path.unwrap_or(match upstream_protocol {
                ProviderProtocol::ClaudeMessages => "/v1/messages",
                ProviderProtocol::OpenAiChat => "/chat/completions",
                ProviderProtocol::OpenAiResponses => "/responses",
            });
            let mut upstream_request = request.clone();
            upstream_request.protocol = upstream_protocol;
            if upstream_protocol != request.protocol {
                let body: Value = serde_json::from_slice(&request.body).map_err(|_| {
                    ProviderError::new(
                        ProviderErrorKind::InvalidRequest,
                        StatusCode::BAD_REQUEST,
                        "invalid request JSON",
                    )
                })?;
                let translated = crosslane_translate::bridge::translate_request(
                    &body,
                    request.protocol,
                    upstream_protocol,
                    model.max_output_tokens,
                )
                .map_err(|error| {
                    ProviderError::new(
                        ProviderErrorKind::InvalidRequest,
                        StatusCode::BAD_REQUEST,
                        error.to_string(),
                    )
                })?;
                upstream_request.body = bytes::Bytes::from(translated.to_string());
            }
            let prepared = prepare_provider_request(&upstream_request)?;
            let mut token = match self.api_token(&account).await {
                Ok(token) => token,
                Err(error) => {
                    tracing::warn!(
                        event = "upstream.account.failed",
                        trace_id = %request.trace_id,
                        provider_id = %self.id,
                        account_id = %account.id,
                        error_kind = ?error.kind,
                        "upstream account token unavailable"
                    );
                    self.set_account_runtime_error(&account.id, Some(error.to_string()))
                        .await;
                    last_error = Some(error);
                    continue;
                }
            };
            let mut retried_auth = false;
            loop {
                let url = endpoint_url(&token.endpoint, upstream_path)?;
                let model_permit = self
                    .model_policy
                    .admit(&[
                        &request.model,
                        request.original_model.as_deref().unwrap_or(&request.model),
                    ])
                    .await?;
                let sent = self
                    .http
                    .post(url)
                    .headers(self.upstream_headers(
                        &token.token,
                        Some(&request.headers),
                        upstream_protocol,
                        prepared.streaming,
                        prepared.vision,
                    ))
                    .header("x-request-id", &request.trace_id)
                    .body(prepared.body.clone())
                    .send()
                    .await;
                drop(model_permit);
                let response = match sent {
                    Ok(response) => response,
                    Err(error) => {
                        let error = transport_error(error);
                        tracing::warn!(
                            event = "upstream.account.failed",
                            trace_id = %request.trace_id,
                            provider_id = %self.id,
                            account_id = %account.id,
                            error_kind = ?error.kind,
                            "upstream account transport failed"
                        );
                        self.set_account_runtime_error(&account.id, Some(error.to_string()))
                            .await;
                        last_error = Some(error);
                        drop(permit);
                        break;
                    }
                };
                let status = response.status();
                if status == reqwest::StatusCode::UNAUTHORIZED && !retried_auth {
                    retried_auth = true;
                    self.token_cache.write().await.remove(&account.id);
                    token = match self.api_token(&account).await {
                        Ok(token) => token,
                        Err(error) => {
                            self.set_account_runtime_error(&account.id, Some(error.to_string()))
                                .await;
                            last_error = Some(error);
                            drop(permit);
                            break;
                        }
                    };
                    continue;
                }
                if provider_status_degrades_health(status) {
                    let error = provider_status_error(
                        status,
                        format!("Copilot upstream returned HTTP {}", status.as_u16()),
                    );
                    tracing::warn!(
                        event = "upstream.account.failed",
                        trace_id = %request.trace_id,
                        provider_id = %self.id,
                        account_id = %account.id,
                        upstream_status = status.as_u16(),
                        "upstream account request failed"
                    );
                    self.set_account_runtime_error(&account.id, Some(error.to_string()))
                        .await;
                    if provider_status_allows_failover(status) && excluded.len() < account_count {
                        last_error = Some(error);
                        drop(permit);
                        break;
                    }
                    self.set_provider_runtime_error(Some(error.to_string()));
                } else {
                    if status.is_success() {
                        self.set_account_runtime_error(&account.id, None).await;
                    }
                    self.set_provider_runtime_error(None);
                }
                let response = response_from_reqwest(response, account.id.clone(), permit);
                let response = if upstream_protocol == ProviderProtocol::OpenAiResponses {
                    super::responses::normalize_response(response, prepared.streaming)
                } else {
                    response
                };
                if upstream_protocol == request.protocol {
                    return Ok(response);
                }
                return super::bridge::adapt_response(
                    response,
                    upstream_protocol,
                    request.protocol,
                    prepared.streaming,
                    &request.model,
                    if request.protocol == ProviderProtocol::OpenAiResponses {
                        let body: Value = serde_json::from_slice(&request.body)
                            .map_err(|error| internal_error(error.to_string()))?;
                        crosslane_translate::bridge::ToolNames::for_responses(&body)
                            .map_err(|error| internal_error(error.to_string()))?
                    } else {
                        Default::default()
                    },
                )
                .await;
            }
        }
        let error = last_error.unwrap_or_else(|| {
            ProviderError::unavailable(format!("provider {} has no usable account", self.id))
        });
        self.set_provider_runtime_error(Some(error.to_string()));
        Err(error)
    }
}

#[async_trait]
impl ProviderAdapter for CopilotProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        let account_count = self.accounts.try_read().map_or_else(
            |_| {
                self.account_limits
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .len()
            },
            |accounts| accounts.iter().filter(|account| account.enabled).count(),
        );
        let error = self
            .last_error
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        ProviderDescriptor {
            id: self.id.clone(),
            kind: "copilot".into(),
            enabled: self.enabled,
            status: if !self.enabled {
                "disabled"
            } else if account_count == 0 {
                "needs_account"
            } else if error.is_some() {
                "degraded"
            } else {
                "ready"
            }
            .into(),
            error,
            capabilities: ProviderCapabilities {
                protocols: vec![
                    ProviderProtocol::ClaudeMessages,
                    ProviderProtocol::OpenAiChat,
                    ProviderProtocol::OpenAiResponses,
                ],
                device_flow: true,
                token_import: true,
                model_discovery: true,
                token_counting: CapabilitySupport::Adapted,
                usage: CapabilitySupport::Native,
            },
        }
    }

    async fn models(&self, refresh: bool) -> Result<Vec<ProviderModel>, ProviderError> {
        let accounts = self
            .accounts
            .read()
            .await
            .iter()
            .filter(|account| account.enabled)
            .cloned()
            .collect::<Vec<_>>();
        if accounts.is_empty() {
            return Err(ProviderError::unavailable(format!(
                "provider {} has no enabled Copilot account",
                self.id
            )));
        }
        let enabled_ids = accounts
            .iter()
            .map(|account| account.id.clone())
            .collect::<BTreeSet<_>>();
        if !refresh {
            let cache = self.model_cache.read().await;
            // Normal requests use the same bounded-stale snapshots as account
            // scheduling. Background/explicit refreshes enforce the fresh TTL;
            // a missing or unhealthy unrelated account must not stall traffic.
            if cache_has_usable_snapshot(&cache, &enabled_ids, self.settings.model_max_stale_secs) {
                return Ok(aggregate_model_cache(
                    &cache,
                    &enabled_ids,
                    self.settings.model_max_stale_secs,
                ));
            }
        }

        // Collapse background and CLI-triggered catalog refreshes. The token
        // exchange itself also has a per-account singleflight lock.
        let _refresh = self.model_refresh.lock().await;
        if !refresh {
            let cache = self.model_cache.read().await;
            if cache_has_usable_snapshot(&cache, &enabled_ids, self.settings.model_max_stale_secs) {
                return Ok(aggregate_model_cache(
                    &cache,
                    &enabled_ids,
                    self.settings.model_max_stale_secs,
                ));
            }
        }

        let mut discovered = BTreeMap::<String, (String, Vec<ProviderModel>)>::new();
        let mut errors = BTreeMap::<String, String>::new();
        let mut refreshes = stream::iter(accounts.into_iter().map(|account| async move {
            let result = match self.api_token(&account).await {
                Ok(token) => match self
                    .fetch_models_with_token(&token, Some(&account.id))
                    .await
                {
                    Ok(models) => Ok((token.endpoint, models)),
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            };
            (account.id.clone(), result)
        }))
        .buffer_unordered(self.settings.model_refresh_concurrency);
        while let Some((account_id, result)) = refreshes.next().await {
            match result {
                Ok(snapshot) => {
                    discovered.insert(account_id, snapshot);
                }
                Err(error) => {
                    errors.insert(account_id, error.to_string());
                }
            }
        }

        let now = now_secs();
        // Account management can run while discovery performs network I/O.
        // Reconcile against the current account set under the mutation lock so
        // a concurrent removal cannot be resurrected in the model cache.
        let _mutation = self.mutation.lock().await;
        let mut stored = self.accounts.write().await;
        let enabled_ids = stored
            .iter()
            .filter(|account| account.enabled)
            .map(|account| account.id.clone())
            .collect::<BTreeSet<_>>();
        discovered.retain(|account_id, _| enabled_ids.contains(account_id));
        errors.retain(|account_id, _| enabled_ids.contains(account_id));

        let mut cache_snapshot = self.model_cache.read().await.clone();
        for (account_id, (_, models)) in &discovered {
            cache_snapshot.accounts.insert(
                account_id.clone(),
                AccountModelCache {
                    models: models.clone(),
                    updated_at: now,
                },
            );
        }
        cache_snapshot.accounts.retain(|account_id, entry| {
            enabled_ids.contains(account_id)
                && now.saturating_sub(entry.updated_at) < self.settings.model_max_stale_secs
        });
        let has_usable_snapshot = enabled_ids
            .iter()
            .any(|account_id| cache_snapshot.accounts.contains_key(account_id));
        let models = aggregate_model_cache(
            &cache_snapshot,
            &enabled_ids,
            self.settings.model_max_stale_secs,
        );
        let mut next_accounts = stored.clone();
        for account in &mut next_accounts {
            if let Some((endpoint, models)) = discovered.get(&account.id) {
                account.endpoint = Some(endpoint.clone());
                account.supported_models = models.iter().map(|model| model.id.clone()).collect();
                account.last_error = None;
                account.updated_at = Some(now);
            } else if let Some(error) = errors.get(&account.id) {
                account.last_error = Some(error.clone());
                account.updated_at = Some(now);
            }
        }
        // Persist pruning as well as successful discovery so an expired cache
        // cannot become usable again after a daemon restart.
        save_model_cache(&self.model_cache_path, &self.id, &cache_snapshot).await?;
        if !discovered.is_empty() || !errors.is_empty() {
            save_accounts(&self.accounts_path, &self.id, &next_accounts).await?;
        }
        *self.model_cache.write().await = cache_snapshot;
        *stored = next_accounts;
        drop(stored);
        drop(_mutation);

        let combined_error = (!errors.is_empty()).then(|| {
            errors
                .iter()
                .map(|(account, error)| format!("{account}: {error}"))
                .collect::<Vec<_>>()
                .join("; ")
        });
        *self
            .last_error
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = combined_error.clone();

        if discovered.is_empty() && !has_usable_snapshot {
            return Err(ProviderError::unavailable(format!(
                "Copilot model discovery failed: {}",
                combined_error.unwrap_or_else(|| "no usable catalog snapshot".into())
            )));
        }
        Ok(models)
    }

    async fn execute(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        self.execute_path(request, None).await
    }

    async fn count_tokens(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        drop(
            self.model_policy
                .admit(&[
                    &request.model,
                    request.original_model.as_deref().unwrap_or(&request.model),
                ])
                .await?,
        );
        if !self.enabled {
            return Err(ProviderError::unavailable(format!(
                "provider {} is disabled",
                self.id
            )));
        }
        let account_ids = self.pool_account_ids(request.account_pool.as_ref()).await;
        let (account, permit, model) = self
            .acquire_account(
                &[],
                &request.model,
                ProviderProtocol::ClaudeMessages,
                false,
                account_ids.as_ref(),
            )
            .await?;
        if !model.protocols.contains(&ProviderProtocol::ClaudeMessages) {
            tracing::info!(
                event = "upstream.account.selected",
                trace_id = %request.trace_id,
                service_id = %request.service_id,
                provider_id = %self.id,
                account_id = %account.id,
                model = %request.model,
                operation = "count_tokens_estimate",
                "upstream account selected"
            );
            let body: Value = serde_json::from_slice(&request.body).map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    StatusCode::BAD_REQUEST,
                    "invalid request JSON",
                )
            })?;
            // Validate the same content shapes used by adapted generation.
            crosslane_translate::bridge::translate_request(
                &body,
                ProviderProtocol::ClaudeMessages,
                ProviderProtocol::OpenAiChat,
                model.max_output_tokens,
            )
            .map_err(|error| {
                ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    StatusCode::BAD_REQUEST,
                    error.to_string(),
                )
            })?;
            let mut input = serde_json::json!({"system":body["system"],"messages":body["messages"],"tools":body["tools"]});
            let mut attachment_tokens = 0;
            strip_counted_attachments(&mut input, &mut attachment_tokens);
            let tokenizer = self
                .tokenizer
                .get_or_init(|| crosslane_translate::TokenCountCache::new(128))
                .as_ref()
                .map_err(|error| internal_error(error.clone()))?;
            let tokens = tokenizer
                .count(input.to_string())
                .await
                .map_err(internal_error)?
                + attachment_tokens;
            drop(permit);
            let mut headers = HeaderMap::new();
            headers.insert("content-type", HeaderValue::from_static("application/json"));
            headers.insert(
                "x-crosslane-token-count-source",
                HeaderValue::from_static("estimate"),
            );
            return Ok(ProviderResponse {
                status: StatusCode::OK,
                headers,
                body: ProviderResponseBody::Full(bytes::Bytes::from(
                    serde_json::json!({"input_tokens":tokens,"estimated":true}).to_string(),
                )),
                account_id: Some(account.id),
                upstream_request_id: None,
            });
        }
        drop(permit);
        self.execute_path(request, Some("/v1/messages/count_tokens"))
            .await
    }
}

fn strip_counted_attachments(value: &mut Value, attachment_tokens: &mut usize) {
    if matches!(
        value["type"].as_str(),
        Some("image" | "image_url" | "input_image")
    ) {
        *attachment_tokens += 1024;
        *value = serde_json::json!({"type":"image"});
    } else if value["type"] == "document" && value["source"]["type"] != "text" {
        *attachment_tokens += value["source"]["data"]
            .as_str()
            .map_or(1024, |data| data.len() / 4 + 256);
        *value = serde_json::json!({"type":"document"});
    } else {
        match value {
            Value::Array(items) => {
                for item in items {
                    strip_counted_attachments(item, attachment_tokens);
                }
            }
            Value::Object(fields) => {
                for field in fields.values_mut() {
                    strip_counted_attachments(field, attachment_tokens);
                }
            }
            _ => {}
        }
    }
}

struct PreparedProviderRequest {
    body: bytes::Bytes,
    streaming: bool,
    vision: bool,
}

fn prepare_provider_request(
    request: &ProviderRequest,
) -> Result<PreparedProviderRequest, ProviderError> {
    let mut value: Value = serde_json::from_slice(&request.body)
        .map_err(|error| internal_error(format!("decode routed provider request: {error}")))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| internal_error("routed provider request must be a JSON object"))?;
    object.insert("model".into(), Value::String(request.model.clone()));
    let streaming = object
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let vision = object.values().any(contains_image_content);
    if request.model.starts_with("claude-") {
        if object
            .get("thinking")
            .and_then(|thinking| thinking.get("display"))
            .is_some_and(|value| value == "updates")
        {
            // Copilot does not implement the progress-summary beta. Preserve
            // thinking and expose its supported summarized representation.
            object.get_mut("thinking").unwrap()["display"] = Value::String("summarized".into());
        }
        if let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) {
            for tool in tools {
                let schema = if request.protocol == ProviderProtocol::ClaudeMessages {
                    tool.get_mut("input_schema")
                } else {
                    tool.pointer_mut("/function/parameters")
                };
                if let Some(schema) = schema {
                    *schema = crosslane_translate::tool_schema::compatible_tool_schema(schema);
                }
            }
        }
    }
    if request.protocol == ProviderProtocol::ClaudeMessages {
        // These first-party extensions have no matching Copilot beta support.
        // Keep their request envelopes consistent with the forwarded betas.
        for field in ["fallbacks", "container", "mcp_servers", "safeguards"] {
            object.remove(field);
        }
        let has_context_management_beta = request
            .headers
            .get("anthropic-beta")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .any(|beta| beta == "context-management-2025-06-27")
            });
        if !has_context_management_beta {
            object.remove("context_management");
        }
        if !streaming {
            object.remove("stream");
        }
    } else if request.protocol == ProviderProtocol::OpenAiChat && streaming {
        let stream_options = object
            .entry("stream_options")
            .or_insert_with(|| serde_json::json!({}));
        if let Some(options) = stream_options.as_object_mut() {
            options.entry("include_usage").or_insert(Value::Bool(true));
        }
    }
    let body = serde_json::to_vec(&value)
        .map(bytes::Bytes::from)
        .map_err(|error| internal_error(format!("encode routed provider request: {error}")))?;
    Ok(PreparedProviderRequest {
        body,
        streaming,
        vision,
    })
}

fn contains_image_content(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            object
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| matches!(kind, "image" | "image_url" | "input_image"))
                || object.contains_key("image_url")
                || object.values().any(contains_image_content)
        }
        Value::Array(items) => items.iter().any(contains_image_content),
        _ => false,
    }
}

fn forwardable_anthropic_betas(value: &str) -> String {
    value
        .split(',')
        .map(str::trim)
        .filter(|beta| {
            FORWARDABLE_ANTHROPIC_BETAS.contains(beta)
                // Preserve dated revisions of the native per-message control beta.
                || beta.strip_prefix("per-turn-control-").is_some_and(|date| {
                    date.len() == 10
                        && date.bytes().enumerate().all(|(index, byte)| match index {
                            4 | 7 => byte == b'-',
                            _ => byte.is_ascii_digit(),
                        })
                })
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(",")
}

async fn load_accounts(
    path: &Path,
    provider_id: &ProviderId,
) -> Result<Vec<CopilotAccount>, ProviderError> {
    let raw = match crosslane_store::atomic::read_to_string_with_retry(path).await {
        Ok(raw) => raw,
        Err(error) if crosslane_store::atomic::is_missing(&error) => return Ok(Vec::new()),
        Err(error) => return Err(internal_error(format!("read {}: {error}", path.display()))),
    };
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    if let Ok(envelope) = serde_json::from_str::<PersistedAccounts>(&raw) {
        if envelope.schema_version != ACCOUNTS_SCHEMA_VERSION {
            return Err(internal_error(format!(
                "unsupported Copilot account schema version {}",
                envelope.schema_version
            )));
        }
        if envelope.provider_id != provider_id.as_str() {
            return Err(internal_error(format!(
                "provider account file belongs to {}, expected {}",
                envelope.provider_id, provider_id
            )));
        }
        return Ok(envelope.accounts);
    }
    serde_json::from_str::<Vec<CopilotAccount>>(&raw)
        .map_err(|error| internal_error(format!("parse {}: {error}", path.display())))
}

async fn save_accounts(
    path: &Path,
    provider_id: &ProviderId,
    accounts: &[CopilotAccount],
) -> Result<(), ProviderError> {
    let bytes = serde_json::to_vec_pretty(&PersistedAccounts {
        schema_version: ACCOUNTS_SCHEMA_VERSION,
        provider_id: provider_id.to_string(),
        accounts: accounts.to_vec(),
    })
    .map_err(|error| internal_error(format!("serialize Copilot accounts: {error}")))?;
    let mut bytes_with_newline = bytes;
    bytes_with_newline.push(b'\n');
    crosslane_store::atomic::write_bytes_atomically(
        path,
        &bytes_with_newline,
        Some(ACCOUNTS_FILE_MODE),
    )
    .await
    .map_err(|error| internal_error(format!("save {}: {error}", path.display())))
}

async fn load_model_cache(
    path: &Path,
    provider_id: &ProviderId,
) -> Result<ModelCache, ProviderError> {
    let raw = match crosslane_store::atomic::read_to_string_with_retry(path).await {
        Ok(raw) => raw,
        Err(error) if crosslane_store::atomic::is_missing(&error) => {
            return Ok(ModelCache::default())
        }
        Err(error) => return Err(internal_error(format!("read {}: {error}", path.display()))),
    };
    if raw.trim().is_empty() {
        return Ok(ModelCache::default());
    }
    let persisted: PersistedModelCache = serde_json::from_str(&raw)
        .map_err(|error| internal_error(format!("parse {}: {error}", path.display())))?;
    if persisted.schema_version != MODEL_CACHE_SCHEMA_VERSION {
        return Err(internal_error(format!(
            "unsupported Copilot model-cache schema version {}",
            persisted.schema_version
        )));
    }
    if persisted.provider_id != provider_id.as_str() {
        return Err(internal_error(format!(
            "provider model cache belongs to {}, expected {}",
            persisted.provider_id, provider_id
        )));
    }
    Ok(ModelCache {
        accounts: persisted.accounts,
    })
}

async fn save_model_cache(
    path: &Path,
    provider_id: &ProviderId,
    cache: &ModelCache,
) -> Result<(), ProviderError> {
    let mut bytes = serde_json::to_vec_pretty(&PersistedModelCache {
        schema_version: MODEL_CACHE_SCHEMA_VERSION,
        provider_id: provider_id.to_string(),
        accounts: cache.accounts.clone(),
    })
    .map_err(|error| internal_error(format!("serialize Copilot model cache: {error}")))?;
    bytes.push(b'\n');
    crosslane_store::atomic::write_bytes_atomically(path, &bytes, Some(ACCOUNTS_FILE_MODE))
        .await
        .map_err(|error| internal_error(format!("save {}: {error}", path.display())))
}

fn cache_has_usable_snapshot(
    cache: &ModelCache,
    enabled_ids: &BTreeSet<String>,
    max_age_secs: i64,
) -> bool {
    let now = now_secs();
    !enabled_ids.is_empty()
        && enabled_ids.iter().any(|account_id| {
            cache
                .accounts
                .get(account_id)
                .is_some_and(|entry| cache_entry_is_usable(entry, now, max_age_secs))
        })
}

fn cache_entry_is_usable(entry: &AccountModelCache, now: i64, max_age_secs: i64) -> bool {
    now.saturating_sub(entry.updated_at) < max_age_secs
}

fn aggregate_model_cache(
    cache: &ModelCache,
    enabled_ids: &BTreeSet<String>,
    max_age_secs: i64,
) -> Vec<ProviderModel> {
    let now = now_secs();
    let mut models = BTreeMap::<String, ProviderModel>::new();
    for entry in cache.accounts.iter().filter_map(|(account_id, entry)| {
        (enabled_ids.contains(account_id) && cache_entry_is_usable(entry, now, max_age_secs))
            .then_some(entry)
    }) {
        for model in &entry.models {
            match models.entry(model.id.clone()) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(model.clone());
                }
                std::collections::btree_map::Entry::Occupied(mut slot) => {
                    merge_model_metadata(slot.get_mut(), model);
                }
            }
        }
    }
    models
        .into_values()
        .filter(|model| !is_embedding_model(model) && !model.protocols.is_empty())
        .map(|mut model| {
            if !model.capabilities.is_object() {
                model.capabilities = serde_json::json!({});
            }
            model.capabilities["native_protocols"] = serde_json::json!(model.protocols);
            model.protocols = vec![
                ProviderProtocol::ClaudeMessages,
                ProviderProtocol::OpenAiChat,
                ProviderProtocol::OpenAiResponses,
            ];
            model
        })
        .collect()
}

fn is_embedding_model(model: &ProviderModel) -> bool {
    model.id.to_ascii_lowercase().contains("embedding")
        || model.capabilities["type"].as_str().is_some_and(|kind| {
            kind.eq_ignore_ascii_case("embeddings") || kind.eq_ignore_ascii_case("embedding")
        })
}

fn merge_model_metadata(current: &mut ProviderModel, candidate: &ProviderModel) {
    current.max_input_tokens = minimum_known(current.max_input_tokens, candidate.max_input_tokens);
    current.max_output_tokens =
        minimum_known(current.max_output_tokens, candidate.max_output_tokens);
    for protocol in &candidate.protocols {
        if !current.protocols.contains(protocol) {
            current.protocols.push(*protocol);
        }
    }
}

fn minimum_known(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        _ => None,
    }
}

async fn decode_json_response<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    operation: &str,
) -> Result<T, ProviderError> {
    let status = response.status();
    let bytes = response.bytes().await.map_err(transport_error)?;
    if !status.is_success() {
        return Err(upstream_error(status, &bytes, operation));
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| internal_error(format!("decode {operation} response: {error}")))
}

fn parse_models(
    provider_id: &ProviderId,
    value: &Value,
    _account_id: Option<&str>,
) -> Result<Vec<ProviderModel>, ProviderError> {
    let items = value
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| value.get("models").and_then(Value::as_array))
        .or_else(|| value.as_array())
        .ok_or_else(|| internal_error("Copilot model response does not contain an array"))?;
    let mut models = Vec::new();
    for item in items {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        let display_name = item
            .get("name")
            .or_else(|| item.get("display_name"))
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_owned();
        let max_input_tokens = first_u64(
            item,
            &[
                "/capabilities/limits/max_prompt_tokens",
                "/capabilities/limits/max_context_window_tokens",
                "/limits/max_input_tokens",
                "/max_input_tokens",
            ],
        );
        let max_output_tokens = first_u64(
            item,
            &[
                "/capabilities/limits/max_output_tokens",
                "/limits/max_output_tokens",
                "/max_output_tokens",
                "/max_tokens",
            ],
        );
        let vendor = item
            .get("vendor")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| id.split(['-', '.', '/']).next().map(str::to_owned));
        let protocols = model_protocols(item, id);
        models.push(ProviderModel {
            provider_id: provider_id.clone(),
            id: id.to_owned(),
            display_name,
            vendor,
            max_input_tokens,
            max_output_tokens,
            protocols,
            capabilities: item.get("capabilities").cloned().unwrap_or(Value::Null),
        });
    }
    Ok(models)
}

fn model_protocols(value: &Value, model_id: &str) -> Vec<ProviderProtocol> {
    if model_id.to_ascii_lowercase().contains("embedding")
        || value
            .pointer("/capabilities/type")
            .and_then(Value::as_str)
            .is_some_and(|kind| {
                kind.eq_ignore_ascii_case("embeddings") || kind.eq_ignore_ascii_case("embedding")
            })
    {
        return Vec::new();
    }
    let declared = [
        "/capabilities/supported_endpoints",
        "/capabilities/endpoints",
        "/supported_endpoints",
        "/endpoints",
    ]
    .into_iter()
    .find_map(|pointer| value.pointer(pointer).and_then(Value::as_array));
    let Some(declared) = declared else {
        // GitHub's current catalog commonly omits endpoint metadata. Follow
        // the native protocol families verified by the reference gateway,
        // while explicit catalog metadata below always wins.
        let model = model_id.to_ascii_lowercase();
        if model.starts_with("claude") {
            return vec![ProviderProtocol::ClaudeMessages];
        }
        if model.starts_with("grok") {
            return vec![ProviderProtocol::OpenAiResponses];
        }
        if model.starts_with("gemini") {
            return vec![ProviderProtocol::OpenAiChat];
        }
        return vec![
            ProviderProtocol::OpenAiChat,
            ProviderProtocol::OpenAiResponses,
        ];
    };
    let mut protocols = Vec::new();
    for endpoint in declared.iter().filter_map(Value::as_str) {
        let normalized = endpoint.trim().trim_end_matches('/').to_ascii_lowercase();
        let protocol = if normalized.ends_with("/messages") || normalized == "messages" {
            Some(ProviderProtocol::ClaudeMessages)
        } else if normalized.ends_with("/chat/completions")
            || normalized == "chat"
            || normalized == "chat_completions"
        {
            Some(ProviderProtocol::OpenAiChat)
        } else if normalized.ends_with("/responses") || normalized == "responses" {
            Some(ProviderProtocol::OpenAiResponses)
        } else {
            None
        };
        if let Some(protocol) = protocol.filter(|protocol| !protocols.contains(protocol)) {
            protocols.push(protocol);
        }
    }
    protocols
}

fn first_u64(value: &Value, pointers: &[&str]) -> Option<u64> {
    pointers
        .iter()
        .find_map(|pointer| value.pointer(pointer).and_then(Value::as_u64))
}

fn response_from_reqwest(
    response: reqwest::Response,
    account_id: String,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> ProviderResponse {
    let status = response.status();
    let upstream_request_id = ["request-id", "x-request-id", "x-github-request-id"]
        .into_iter()
        .find_map(|name| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        });
    let headers = filtered_response_headers(response.headers());
    let mut upstream = response.bytes_stream();
    let stream = async_stream::try_stream! {
        let _permit = permit;
        while let Some(chunk) = upstream.next().await {
            yield chunk.map_err(transport_error)?;
        }
    };
    ProviderResponse {
        status: StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
        headers,
        body: ProviderResponseBody::Stream(Box::pin(stream)),
        account_id: Some(account_id),
        upstream_request_id,
    }
}

fn filtered_response_headers(input: &reqwest::header::HeaderMap) -> HeaderMap {
    let mut output = HeaderMap::new();
    for (name, value) in input {
        let name_text = name.as_str();
        let allowed = matches!(
            name_text,
            "content-type"
                | "cache-control"
                | "retry-after"
                | "request-id"
                | "x-request-id"
                | "x-github-request-id"
        ) || name_text.starts_with("x-ratelimit-")
            || name_text.starts_with("openai-")
            || name_text.starts_with("anthropic-");
        if allowed {
            output.insert(name.clone(), value.clone());
        }
    }
    output
}

fn endpoint_url(base: &str, path: &str) -> Result<Url, ProviderError> {
    let mut url = Url::parse(base)
        .map_err(|error| internal_error(format!("invalid upstream URL {base}: {error}")))?;
    let mut base_path = url.path().trim_end_matches('/').to_owned();
    base_path.push('/');
    base_path.push_str(path.trim_start_matches('/'));
    url.set_path(&base_path);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn insert_header(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(HeaderName::from_static(name), value);
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn transport_error(error: reqwest::Error) -> ProviderError {
    let mut result = ProviderError::new(
        ProviderErrorKind::Transport,
        StatusCode::BAD_GATEWAY,
        format!("Copilot transport error: {error}"),
    );
    result.retryable = error.is_connect() || error.is_timeout();
    result
}

fn internal_error(message: impl Into<String>) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Internal,
        StatusCode::INTERNAL_SERVER_ERROR,
        message,
    )
}

fn upstream_error(status: reqwest::StatusCode, body: &[u8], operation: &str) -> ProviderError {
    let message = serde_json::from_slice::<Value>(&body[..body.len().min(MAX_ERROR_BODY_BYTES)])
        .ok()
        .and_then(|value| {
            value
                .get("error_description")
                .or_else(|| value.get("message"))
                .or_else(|| value.pointer("/error/message"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| format!("{operation} returned HTTP {}", status.as_u16()));
    provider_status_error(status, message)
}

fn provider_status_error(status: reqwest::StatusCode, message: impl Into<String>) -> ProviderError {
    let kind = match status.as_u16() {
        401 => ProviderErrorKind::Authentication,
        403 => ProviderErrorKind::Authorization,
        400 | 404 | 409 | 422 => ProviderErrorKind::InvalidRequest,
        429 => ProviderErrorKind::RateLimited,
        500..=599 => ProviderErrorKind::Unavailable,
        _ => ProviderErrorKind::Upstream,
    };
    let mut error = ProviderError::new(
        kind,
        StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
        message,
    );
    error.retryable = matches!(status.as_u16(), 429 | 502 | 503 | 504);
    error.account_error = matches!(status.as_u16(), 401 | 403);
    error
}

fn provider_status_degrades_health(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 401 | 403 | 429 | 500..=599)
}

fn provider_status_allows_failover(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 401 | 403 | 429 | 502 | 503 | 504)
}

fn not_found_account(id: &str) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::InvalidRequest,
        StatusCode::NOT_FOUND,
        format!("Copilot account {id} not found"),
    )
}

#[cfg(test)]
mod tests {
    mod latency;

    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn claude_native_requests_preserve_schema_constraints_and_thinking() {
        let body = serde_json::json!({"thinking":{"type":"adaptive","display":"updates"},
            "tools":[{"name":"mode","input_schema":{"type":"object","oneOf":[{"required":["a"]},{"required":["b"]}]}}]});
        let request = ProviderRequest {
            original_model: None,
            protocol: ProviderProtocol::ClaudeMessages,
            model: "claude-opus-5".into(),
            path: "/v1/messages".into(),
            headers: HeaderMap::new(),
            body: bytes::Bytes::from(body.to_string()),
            trace_id: "test".into(),
            service_id: "test".into(),
            account_pool: None,
            api_key_id: None,
            client_context: Default::default(),
        };
        let wire: Value =
            serde_json::from_slice(&prepare_provider_request(&request).unwrap().body).unwrap();
        assert_eq!(
            wire["thinking"],
            serde_json::json!({"type":"adaptive","display":"summarized"})
        );
        assert!(wire["tools"][0]["input_schema"].get("oneOf").is_none());
        assert_eq!(
            wire["tools"][0]["input_schema"]["then"]["oneOf"],
            body["tools"][0]["input_schema"]["oneOf"]
        );
    }

    #[test]
    fn forwards_per_turn_control_beta_with_different_date_suffixes() {
        for date in ["2026-07-01", "2026-10-10", "2027-01-15"] {
            let beta = format!("per-turn-control-{date}");
            assert_eq!(forwardable_anthropic_betas(&beta), beta);
        }
        for beta in [
            "per-turn-control",
            "per-turn-control-",
            "per-turn-control-20260701",
            "per-turn-control-2026-7-1",
            "per-turn-control-2026-07-xx",
            "per-turn-control-2026-07-01-extra",
            "per-turn-control-extra-2026-07-01",
            "per-turn-controls-2026-07-01",
            "unknown-beta-2026-07-01",
        ] {
            assert_eq!(forwardable_anthropic_betas(beta), "", "{beta}");
        }
        assert_eq!(
            forwardable_anthropic_betas(
                " per-turn-control-2027-01-15,context-management-2025-06-27,per-turn-control-2027-01-15,unknown-beta-2027-01-15 "
            ),
            "context-management-2025-06-27,per-turn-control-2027-01-15"
        );
    }

    async fn mock_provider(server: &MockServer, directory: &Path) -> CopilotProvider {
        mock_provider_with_settings(server, directory, |_| {}).await
    }

    async fn mock_provider_with_settings(
        server: &MockServer,
        directory: &Path,
        configure: impl FnOnce(&mut CopilotSettings),
    ) -> CopilotProvider {
        let mut config = ProviderConfig {
            id: "copilot".into(),
            kind: "copilot".into(),
            ..ProviderConfig::default()
        };
        for key in ["github_api_base", "oauth_base"] {
            config
                .settings
                .insert(key.into(), Value::String(server.uri()));
        }
        config
            .settings
            .insert("allow_insecure_http".into(), Value::Bool(true));
        let accounts_path = directory.join("providers/copilot/accounts.json");
        tokio::fs::create_dir_all(accounts_path.parent().unwrap())
            .await
            .unwrap();
        let mut settings = CopilotSettings::from_config(&config);
        configure(&mut settings);
        CopilotProvider::load(
            ProviderId::parse("copilot").unwrap(),
            true,
            settings,
            accounts_path,
        )
        .await
        .unwrap()
    }

    async fn mount_account_exchange(server: &MockServer, github_token: &str) {
        Mock::given(method("GET"))
            .and(path("/user"))
            .and(header("authorization", format!("Bearer {github_token}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id":42,
                "login":"octocat",
                "email":"octocat@example.com"
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/copilot_internal/v2/token"))
            .and(header("authorization", format!("token {github_token}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token":"copilot-api-token",
                "expires_at":now_secs() + 3600,
                "refresh_in":3540,
                "sku":"test-seat",
                "endpoints":{"api":server.uri()}
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("authorization", "Bearer copilot-api-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data":[{
                    "id":"gpt-test",
                    "name":"GPT Test",
                    "capabilities":{
                        "limits":{"max_prompt_tokens":8192,"max_output_tokens":1024},
                        "supported_endpoints":["/v1/messages","/chat/completions","/responses"]
                    }
                }]
            })))
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn account_info_is_safe_cached_and_persisted_without_affecting_health() {
        let server = MockServer::start().await;
        let directory = tempfile::tempdir().unwrap();
        mount_account_exchange(&server, "github-token").await;
        let provider = mock_provider(&server, directory.path()).await;
        let imported = provider
            .import_credentials(
                CopilotCredentials {
                    github_access_token: "github-token".into(),
                    github_refresh_token: Some("github-refresh-secret".into()),
                    github_expires_at: Some(4_000_000_000),
                    github_refresh_token_expires_at: Some(5_000_000_000),
                    token_type: "bearer".into(),
                },
                None,
                None,
            )
            .await
            .unwrap();
        assert!(imported.info.api_token_expires_at.unwrap() > now_secs());
        assert_eq!(imported.info.api_token_refresh_in_secs, Some(3540));
        assert_eq!(imported.info.sku.as_deref(), Some("test-seat"));
        assert_eq!(imported.info.github_token_expires_at, Some(4_000_000_000));
        assert_eq!(
            imported.info.github_refresh_token_expires_at,
            Some(5_000_000_000)
        );
        {
            let _quota = Mock::given(method("GET"))
                .and(path("/copilot_internal/user"))
                .and(header("authorization", "Bearer github-token"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "copilot_plan":"business", "access_type_sku":"business-seat",
                    "token_based_billing":true,
                    "quota_reset_date":"2026-11-01", "quota_reset_date_utc":"2026-11-01T00:00:00.000Z",
                    "quota_snapshots":{"premium_interactions":{
                        "unlimited":true,"entitlement":0,"remaining":0,"credits_used":0,
                        "percent_remaining":100.0,"token_based_billing":true,"has_quota":true
                    }},
                    "organization_secret":"must-not-be-stored"
                })))
                .expect(1).mount_as_scoped(&server).await;
            provider.refresh_account_info(None, false).await;
            provider.refresh_account_info(None, false).await;
        }
        let summary = provider.account(&imported.id).await.unwrap();
        assert_eq!(summary.info.plan.as_deref(), Some("business"));
        assert_eq!(summary.info.sku.as_deref(), Some("business-seat"));
        assert_eq!(summary.info.token_based_billing, Some(true));
        assert_eq!(
            summary.info.quota_reset_date.as_deref(),
            Some("2026-11-01T00:00:00.000Z")
        );
        let quota = &summary.info.quota_snapshots["premium_interactions"];
        assert_eq!(quota.unlimited, Some(true));
        assert_eq!(quota.credits_used, Some(0.0));
        assert_eq!(quota.overage_count, None);
        let serialized = serde_json::to_string(&summary).unwrap();
        for secret in [
            "github-token",
            "github-refresh-secret",
            "copilot-api-token",
            "must-not-be-stored",
        ] {
            assert!(!serialized.contains(secret), "{secret}");
        }
        let _failure = Mock::given(method("GET"))
            .and(path("/copilot_internal/user"))
            .respond_with(ResponseTemplate::new(429).set_body_string("private-error-body"))
            .expect(1)
            .mount_as_scoped(&server)
            .await;
        provider
            .refresh_account_info(Some(&imported.id), true)
            .await;
        provider.refresh_account_info(None, false).await;
        let reloaded = mock_provider(&server, directory.path()).await;
        reloaded.refresh_account_info(None, false).await;
        let summary = reloaded.account(&imported.id).await.unwrap();
        assert_eq!(summary.auth_state, "ready");
        assert!(summary.last_error.is_none());
        assert_eq!(summary.info.plan.as_deref(), Some("business"));
        assert!(summary.info.quota_error.as_deref().unwrap().contains("429"));
        assert!(!summary
            .info
            .quota_error
            .unwrap()
            .contains("private-error-body"));
    }

    #[tokio::test]
    async fn account_info_failure_and_legacy_files_keep_unknown_values_and_health() {
        let server = MockServer::start().await;
        let directory = tempfile::tempdir().unwrap();
        mount_account_exchange(&server, "github-token").await;
        let provider = mock_provider(&server, directory.path()).await;
        let imported = provider
            .import_token("github-token".into(), None)
            .await
            .unwrap();
        assert_eq!(imported.info.github_token_expires_at, None);
        {
            let mut accounts = provider.accounts.write().await;
            accounts[0].info = CopilotAccountInfo::default();
        }
        let _failure = Mock::given(method("GET"))
            .and(path("/copilot_internal/user"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount_as_scoped(&server)
            .await;
        provider.refresh_account_info(None, false).await;
        // Missing token metadata must not bypass the failed-query backoff.
        provider.refresh_account_info(None, false).await;
        let summary = provider.account(&imported.id).await.unwrap();
        assert!(summary.info.plan.is_none());
        assert!(summary.info.api_token_expires_at.is_none());
        assert_eq!(summary.auth_state, "ready");
        assert!(summary.last_error.is_none());
        let mut legacy = serde_json::to_value(&provider.accounts.read().await[0]).unwrap();
        legacy.as_object_mut().unwrap().remove("info");
        let legacy: CopilotAccount = serde_json::from_value(legacy).unwrap();
        assert!(legacy.info.quota_snapshots.is_empty());
        assert!(legacy.info.quota_checked_at.is_none());
    }

    #[tokio::test]
    async fn account_pool_prefers_idle_then_less_busy_account() {
        let server = MockServer::start().await;
        let directory = tempfile::tempdir().unwrap();
        mount_account_exchange(&server, "github-token").await;
        let provider = mock_provider(&server, directory.path()).await;
        let first = provider
            .import_token("github-token".into(), None)
            .await
            .unwrap();
        let mut second = provider.accounts.read().await[0].clone();
        second.id = "acc_second".into();
        second.github_user_id += 1;
        provider.accounts.write().await.push(second.clone());
        let cached = provider.model_cache.read().await.accounts[&first.id].clone();
        provider
            .model_cache
            .write()
            .await
            .accounts
            .insert(second.id.clone(), cached);
        let first_slot = {
            let mut slots = provider
                .account_limits
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            slots.insert(
                second.id.clone(),
                Arc::new(AccountSlot::new(
                    provider.settings.max_concurrent_per_account,
                )),
            );
            Arc::clone(&slots[&first.id])
        };
        let second_slot = {
            let slots = provider
                .account_limits
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            Arc::clone(&slots[&second.id])
        };
        first_slot.mark_selected();

        let (idle, idle_permit, _) = provider
            .acquire_account(
                &[],
                "gpt-test",
                ProviderProtocol::ClaudeMessages,
                false,
                None,
            )
            .await
            .unwrap();
        assert_eq!(idle.id, second.id);
        let (less_busy, busy_permit, _) = provider
            .acquire_account(
                &[],
                "gpt-test",
                ProviderProtocol::ClaudeMessages,
                false,
                None,
            )
            .await
            .unwrap();
        assert_eq!(less_busy.id, first.id);
        drop((idle_permit, busy_permit));

        let mut first_permits = Vec::new();
        let mut second_permits = Vec::new();
        for _ in 0..provider.settings.max_concurrent_per_account {
            first_permits.push(
                Arc::clone(&first_slot.semaphore)
                    .try_acquire_owned()
                    .unwrap(),
            );
            second_permits.push(
                Arc::clone(&second_slot.semaphore)
                    .try_acquire_owned()
                    .unwrap(),
            );
        }
        let waiting = tokio::spawn(async move {
            provider
                .acquire_account(
                    &[],
                    "gpt-test",
                    ProviderProtocol::ClaudeMessages,
                    false,
                    None,
                )
                .await
                .map(|(account, permit, _)| (account.id, permit))
        });
        tokio::task::yield_now().await;
        drop(second_permits.pop());
        let (account_id, permit) = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("a free account should wake the queue")
            .expect("account selection task")
            .expect("account selection");
        assert_eq!(account_id, second.id);
        drop((permit, first_permits, second_permits));
    }

    #[tokio::test]
    async fn disabled_models_block_all_protocols_and_native_count_tokens_before_any_io() {
        let server = MockServer::start().await;
        let directory = tempfile::tempdir().unwrap();
        let policy = Arc::new(crosslane_provider::ModelPolicy::default());
        let provider = mock_provider(&server, directory.path())
            .await
            .with_model_policy(Arc::clone(&policy));
        policy
            .replace(&crosslane_core::config::ModelsConfig {
                disabled: vec!["gpt-test".into()],
                ..Default::default()
            })
            .await;
        for protocol in [
            ProviderProtocol::OpenAiChat,
            ProviderProtocol::OpenAiResponses,
            ProviderProtocol::ClaudeMessages,
        ] {
            for model in ["gpt-test", "mapped-allowed"] {
                for streaming in [false, true] {
                    let request = ProviderRequest {
                        model: model.into(),
                        original_model: Some("gpt-test".into()),
                        protocol,
                        path: "/ignored".into(),
                        headers: HeaderMap::new(),
                        body: bytes::Bytes::from(
                            serde_json::json!({"model":model,"stream":streaming}).to_string(),
                        ),
                        trace_id: "deny-test".into(),
                        service_id: "test".into(),
                        account_pool: None,
                        api_key_id: None,
                        client_context: Default::default(),
                    };
                    let error = provider.execute(request.clone()).await.err().unwrap();
                    assert_eq!(error.upstream_code.as_deref(), Some("model_disabled"));
                    assert!(!error.account_error && !error.retryable);
                    assert_eq!(
                        provider
                            .count_tokens(request)
                            .await
                            .err()
                            .unwrap()
                            .upstream_code
                            .as_deref(),
                        Some("model_disabled")
                    );
                }
            }
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn adapts_each_non_native_protocol_over_http_in_both_response_modes() {
        use ProviderProtocol::{ClaudeMessages, OpenAiChat, OpenAiResponses};
        for native in [ClaudeMessages, OpenAiChat, OpenAiResponses] {
            for caller in [ClaudeMessages, OpenAiChat, OpenAiResponses] {
                if caller == native {
                    continue;
                }
                for streaming in [false, true] {
                    let server = MockServer::start().await;
                    let directory = tempfile::tempdir().unwrap();
                    mount_account_exchange(&server, "github-token").await;
                    let provider = mock_provider(&server, directory.path()).await;
                    let account = provider
                        .import_token("github-token".into(), None)
                        .await
                        .unwrap();
                    provider
                        .model_cache
                        .write()
                        .await
                        .accounts
                        .get_mut(&account.id)
                        .unwrap()
                        .models[0]
                        .protocols = vec![native];
                    let (path, full, sse) = match native {
                        ClaudeMessages => ("/v1/messages",serde_json::json!({"id":"m","model":"gpt-test","type":"message","content":[{"type":"text","text":"translated"}],"stop_reason":"end_turn","usage":{"input_tokens":4,"output_tokens":2},"copilot_usage":{"total_nano_aiu":7}}),
                            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"gpt-test\",\"usage\":{\"input_tokens\":4}}}\n\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"translated\"}}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2},\"copilot_usage\":{\"total_nano_aiu\":7}}\n\ndata: {\"type\":\"message_stop\"}\n\n"),
                        OpenAiChat => ("/chat/completions",serde_json::json!({"id":"c","model":"gpt-test","choices":[{"message":{"role":"assistant","content":"translated"},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":2},"copilot_usage":{"total_nano_aiu":7}}),
                            "data: {\"id\":\"c\",\"model\":\"gpt-test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"translated\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2},\"copilot_usage\":{\"total_nano_aiu\":7}}\n\ndata: [DONE]\n\n"),
                        OpenAiResponses => ("/responses",serde_json::json!({"id":"r","model":"gpt-test","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"translated"}]}],"usage":{"input_tokens":4,"output_tokens":2},"copilot_usage":{"total_nano_aiu":7}}),
                            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\",\"model\":\"gpt-test\"}}\n\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"translated\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":4,\"output_tokens\":2},\"copilot_usage\":{\"total_nano_aiu\":7}}}\n\n"),
                    };
                    let reply = if streaming {
                        ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream")
                    } else {
                        ResponseTemplate::new(200).set_body_json(full)
                    };
                    Mock::given(method("POST"))
                        .and(wiremock::matchers::path(path))
                        .and(header("authorization", "Bearer copilot-api-token"))
                        .respond_with(reply)
                        .expect(1)
                        .mount(&server)
                        .await;
                    let mut body = match caller {
                        OpenAiResponses => serde_json::json!({"model":"gpt-test","input":"hello"}),
                        _ => {
                            serde_json::json!({"model":"gpt-test","messages":[{"role":"user","content":"hello"}],"max_tokens":64})
                        }
                    };
                    body["stream"] = serde_json::json!(streaming);
                    let response = provider
                        .execute(ProviderRequest {
                            original_model: None,
                            protocol: caller,
                            model: "gpt-test".into(),
                            path: "/client".into(),
                            headers: HeaderMap::new(),
                            body: bytes::Bytes::from(body.to_string()),
                            trace_id: "bridge-test".into(),
                            service_id: "test".into(),
                            account_pool: None,
                            api_key_id: None,
                            client_context: Default::default(),
                        })
                        .await
                        .unwrap_or_else(|error| {
                            panic!("{native:?} -> {caller:?}, stream={streaming}: {error}")
                        });
                    assert_eq!(response.status, StatusCode::OK);
                    assert_eq!(response.account_id.as_deref(), Some(account.id.as_str()));
                    let output = match response.body {
                        ProviderResponseBody::Full(bytes) => bytes.to_vec(),
                        ProviderResponseBody::Stream(mut stream) => {
                            let mut bytes = Vec::new();
                            while let Some(chunk) = stream.next().await {
                                bytes.extend_from_slice(&chunk.unwrap());
                            }
                            bytes
                        }
                    };
                    let output = String::from_utf8(output).unwrap();
                    assert!(output.contains("translated"), "{native:?}->{caller:?}");
                    assert!(output.contains("total_nano_aiu"));
                    let upstream = server
                        .received_requests()
                        .await
                        .unwrap()
                        .into_iter()
                        .find(|r| r.method == "POST")
                        .unwrap();
                    let forwarded: Value = serde_json::from_slice(&upstream.body).unwrap();
                    assert_eq!(forwarded["model"], "gpt-test");
                    assert!(forwarded
                        .get(if native == OpenAiResponses {
                            "input"
                        } else {
                            "messages"
                        })
                        .is_some());
                    server.verify().await;
                }
            }
        }
    }

    #[tokio::test]
    async fn native_responses_keep_one_item_identity_across_deltas_and_completion() {
        let server = MockServer::start().await;
        let directory = tempfile::tempdir().unwrap();
        mount_account_exchange(&server, "github-token").await;
        let provider = mock_provider(&server, directory.path()).await;
        provider
            .import_token("github-token".into(), None)
            .await
            .unwrap();
        // Copilot encrypts the same upstream ID independently in each event.
        // A client that reconciles streamed text with completed items by ID
        // must see one message, including the terminal response snapshot.
        let upstream = [
            serde_json::json!({"type":"response.created","sequence_number":0,"response":{"id":"opaque-response-start","output":[]}}),
            serde_json::json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"id":"opaque-item-start","type":"message","role":"assistant","phase":"final_answer","status":"in_progress","content":[]}}),
            serde_json::json!({"type":"response.content_part.added","sequence_number":2,"output_index":0,"content_index":0,"item_id":"opaque-item-part","part":{"type":"output_text","text":"","annotations":[]}}),
            serde_json::json!({"type":"response.output_text.delta","sequence_number":3,"output_index":0,"content_index":0,"item_id":"opaque-item-delta","delta":"Hello"}),
            serde_json::json!({"type":"response.output_text.done","sequence_number":4,"output_index":0,"content_index":0,"item_id":"opaque-item-text-done","text":"Hello"}),
            serde_json::json!({"type":"response.output_item.done","sequence_number":5,"output_index":0,"item":{"id":"opaque-item-done","type":"message","role":"assistant","phase":"final_answer","status":"completed","content":[{"type":"output_text","text":"Hello","annotations":[]}]}}),
            serde_json::json!({"type":"response.completed","sequence_number":6,"response":{"id":"opaque-response-done","status":"completed","output":[{"id":"opaque-item-snapshot","type":"message","role":"assistant","phase":"final_answer","status":"completed","content":[{"type":"output_text","text":"Hello","annotations":[]}]}],"usage":{"input_tokens":4,"output_tokens":2}},"copilot_usage":{"total_nano_aiu":7}}),
        ];
        let wire = upstream
            .iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap()
                )
            })
            .collect::<String>();
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(wire, "text/event-stream"))
            .expect(1)
            .mount(&server)
            .await;
        let response = provider.execute(ProviderRequest {
            protocol: ProviderProtocol::OpenAiResponses,
            model: "gpt-test".into(),
            original_model: None,
            path: "/v1/responses".into(),
            headers: HeaderMap::new(),
            body: bytes::Bytes::from_static(b"{\"model\":\"gpt-test\",\"input\":\"hi\",\"stream\":true,\"previous_response_id\":\"previous-opaque-id\"}"),
            trace_id: "native-responses-identity".into(),
            service_id: "test".into(),
            account_pool: None,
            api_key_id: None,
            client_context: Default::default(),
        }).await.unwrap();
        let ProviderResponseBody::Stream(mut stream) = response.body else {
            panic!("native SSE must remain streaming");
        };
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk.unwrap());
        }
        let text = String::from_utf8(bytes).unwrap();
        let events = text
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|data| serde_json::from_str::<Value>(data).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(events.len(), upstream.len());
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event["sequence_number"], index);
            if let Some(id) = event.get("item_id") {
                assert_eq!(id, "opaque-item-start");
            }
            if let Some(item) = event.get("item") {
                assert_eq!(item["id"], "opaque-item-start");
                assert_eq!(item["phase"], "final_answer");
            }
            if let Some(response) = event.get("response") {
                assert_eq!(response["id"], "opaque-response-start");
            }
        }
        assert_eq!(
            events[6]["response"]["output"][0]["id"],
            "opaque-item-start"
        );
        assert_eq!(
            events[6]["response"]["output"][0]["content"][0]["text"],
            "Hello"
        );
        assert_eq!(events[6]["copilot_usage"]["total_nano_aiu"], 7);
        let request = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .find(|request| request.method == "POST")
            .unwrap();
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["previous_response_id"], "previous-opaque-id");
        server.verify().await;
    }

    #[tokio::test]
    async fn public_catalog_expands_protocols_without_changing_native_cache_or_exposing_embeddings()
    {
        let server = MockServer::start().await;
        let directory = tempfile::tempdir().unwrap();
        mount_account_exchange(&server, "github-token").await;
        let provider = mock_provider(&server, directory.path()).await;
        let account = provider
            .import_token("github-token".into(), None)
            .await
            .unwrap();
        {
            let mut cache = provider.model_cache.write().await;
            let entry = cache.accounts.get_mut(&account.id).unwrap();
            entry.models[0].protocols = vec![ProviderProtocol::OpenAiChat];
            let mut embedding = entry.models[0].clone();
            embedding.id = "text-embedding-test".into();
            entry.models.push(embedding);
        }
        let models = provider.models(false).await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].protocols.len(), 3);
        assert_eq!(
            models[0].capabilities["native_protocols"],
            serde_json::json!(["open_ai_chat"])
        );
        assert_eq!(
            provider.model_cache.read().await.accounts[&account.id].models[0].protocols,
            [ProviderProtocol::OpenAiChat]
        );
        let mut pool = crosslane_core::config::ProxyServiceConfig {
            allowed_providers: vec!["copilot".into()],
            account_tag: Some(crosslane_core::config::ServiceAccountTags::One(
                "team".into(),
            )),
            ..Default::default()
        };
        let before = provider.pool_account_ids(Some(&pool)).await;
        assert!(provider
            .acquire_account(
                &[],
                "gpt-test",
                ProviderProtocol::ClaudeMessages,
                false,
                before.as_ref()
            )
            .await
            .is_err());
        provider
            .update_account_tags(&account.id, &["team".into()], &[])
            .await
            .unwrap();
        assert!(provider
            .acquire_account(
                &[],
                "gpt-test",
                ProviderProtocol::ClaudeMessages,
                false,
                before.as_ref()
            )
            .await
            .is_err());
        let after = provider.pool_account_ids(Some(&pool)).await;
        let selected = provider
            .acquire_account(
                &[],
                "gpt-test",
                ProviderProtocol::ClaudeMessages,
                false,
                after.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(selected.0.id, account.id);
        drop(selected);
        pool.excluded_account_ids
            .push(format!("copilot/{}", account.id));
        let excluded = provider.pool_account_ids(Some(&pool)).await;
        assert!(provider
            .acquire_account(
                &[],
                "gpt-test",
                ProviderProtocol::ClaudeMessages,
                false,
                excluded.as_ref()
            )
            .await
            .is_err());
    }

    #[test]
    fn settings_default_to_public_client_id_and_preserve_overrides() {
        let mut config = ProviderConfig {
            id: "copilot".into(),
            kind: "copilot".into(),
            ..ProviderConfig::default()
        };
        assert_eq!(
            CopilotSettings::from_config(&config).oauth_client_id(),
            DEFAULT_COPILOT_OAUTH_CLIENT_ID
        );

        config.settings.insert(
            "client_id".into(),
            Value::String(DEFAULT_COPILOT_OAUTH_CLIENT_ID.into()),
        );
        config.settings.insert(
            "client_secret".into(),
            Value::String("custom-secret".into()),
        );
        assert_eq!(
            CopilotSettings::from_config(&config).oauth_client_secret(),
            None
        );

        config
            .settings
            .insert("client_id".into(), Value::String("custom-client".into()));
        assert_eq!(
            CopilotSettings::from_config(&config).oauth_client_id(),
            "custom-client"
        );
        assert_eq!(
            CopilotSettings::from_config(&config).oauth_client_secret(),
            Some("custom-secret")
        );

        config
            .settings
            .insert("client_id".into(), Value::String("  ".into()));
        assert!(CopilotSettings::from_config(&config).validate().is_err());

        config
            .settings
            .insert("client_id".into(), Value::Bool(true));
        assert!(CopilotSettings::from_config(&config).validate().is_err());
    }

    #[test]
    fn settings_keep_driver_specific_values() {
        let mut config = ProviderConfig {
            id: "copilot-team-a".into(),
            kind: "copilot".into(),
            ..ProviderConfig::default()
        };
        config
            .settings
            .insert("client_id".into(), Value::String("client".into()));
        config.settings.insert(
            "allowed_endpoint_hosts".into(),
            serde_json::json!(["copilot.example.com"]),
        );
        let settings = CopilotSettings::from_config(&config);
        assert_eq!(settings.client_id.as_deref(), Some("client"));
        assert_eq!(settings.allowed_endpoint_hosts, ["copilot.example.com"]);
    }

    #[test]
    fn request_scheduling_rejects_model_cache_beyond_max_stale() {
        let entry = AccountModelCache {
            models: Vec::new(),
            updated_at: 100,
        };
        assert!(cache_entry_is_usable(&entry, 129, 30));
        assert!(!cache_entry_is_usable(&entry, 130, 30));
    }

    #[test]
    fn settings_reject_insecure_or_malformed_production_endpoints() {
        let mut config = ProviderConfig {
            id: "copilot".into(),
            kind: "copilot".into(),
            ..ProviderConfig::default()
        };
        config.settings.insert(
            "github_api_base".into(),
            Value::String("http://api.github.test".into()),
        );
        let error = CopilotSettings::from_config(&config)
            .validate()
            .expect_err("insecure GitHub API must fail closed");
        assert!(error.to_string().contains("HTTPS"));

        config
            .settings
            .insert("allow_insecure_http".into(), Value::Bool(true));
        CopilotSettings::from_config(&config)
            .validate()
            .expect("explicit test-only HTTP override");
    }

    #[test]
    fn enterprise_fallback_is_validated_without_blocking_plan_specific_endpoints() {
        let mut config = ProviderConfig {
            id: "copilot".into(),
            kind: "copilot".into(),
            ..ProviderConfig::default()
        };
        config.settings.insert(
            "api_endpoint_fallback".into(),
            Value::String("https://api.enterprise.githubcopilot.com".into()),
        );
        let settings = CopilotSettings::from_config(&config);
        settings.validate().expect("Enterprise host is allowed");
        settings
            .validate_copilot_endpoint("https://api.business.githubcopilot.com")
            .expect("the token response can select a different plan-specific host");

        config.settings.insert(
            "api_endpoint_fallback".into(),
            Value::String("https://api.enterprise.githubcopilot.com.example.org".into()),
        );
        assert!(CopilotSettings::from_config(&config).validate().is_err());
    }

    #[tokio::test]
    async fn token_endpoint_takes_priority_over_configured_fallback() {
        let github = MockServer::start().await;
        let enterprise = MockServer::start().await;
        let fallback = MockServer::start().await;
        let directory = tempfile::tempdir().unwrap();
        let mut provider = mock_provider(&github, directory.path()).await;
        provider.settings.api_endpoint_fallback = Some(fallback.uri());

        Mock::given(method("GET"))
            .and(path("/copilot_internal/v2/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token":"enterprise-token",
                "expires_at":now_secs() + 3600,
                "endpoints":{"api":enterprise.uri()}
            })))
            .expect(1)
            .mount(&github)
            .await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data":[{"id":"enterprise-model"}]
            })))
            .expect(1)
            .mount(&enterprise)
            .await;

        let token = provider.exchange_token_value("github-token").await.unwrap();
        assert_eq!(token.endpoint, enterprise.uri());
        let models = provider
            .fetch_models_with_token(&token, None)
            .await
            .unwrap();
        assert_eq!(models[0].id, "enterprise-model");
        github.verify().await;
        enterprise.verify().await;
        assert!(fallback.received_requests().await.unwrap().is_empty());

        github.reset().await;
        Mock::given(method("GET"))
            .and(path("/copilot_internal/v2/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token":"enterprise-token",
                "expires_at":now_secs() + 3600
            })))
            .expect(1)
            .mount(&github)
            .await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data":[{"id":"fallback-model"}]
            })))
            .expect(1)
            .mount(&fallback)
            .await;

        let token = provider.exchange_token_value("github-token").await.unwrap();
        assert_eq!(token.endpoint, fallback.uri());
        let models = provider
            .fetch_models_with_token(&token, None)
            .await
            .unwrap();
        assert_eq!(models[0].id, "fallback-model");
        github.verify().await;
        fallback.verify().await;
    }

    #[test]
    fn parses_multiple_model_envelopes_and_limits() {
        let provider = ProviderId::parse("copilot").unwrap();
        let models = parse_models(
            &provider,
            &serde_json::json!({"data":[{
                "id":"gpt-test",
                "name":"GPT Test",
                "capabilities":{
                    "limits":{"max_prompt_tokens":1234,"max_output_tokens":321},
                    "supported_endpoints":["/chat/completions", "/responses"]
                }
            }]}),
            None,
        )
        .unwrap();
        assert_eq!(models[0].max_input_tokens, Some(1234));
        assert_eq!(models[0].max_output_tokens, Some(321));
        assert_eq!(
            models[0].protocols,
            [
                ProviderProtocol::OpenAiChat,
                ProviderProtocol::OpenAiResponses
            ]
        );
    }

    #[test]
    fn model_family_fallbacks_do_not_advertise_unsupported_native_protocols() {
        let provider = ProviderId::parse("copilot").unwrap();
        let models = parse_models(
            &provider,
            &serde_json::json!({"data":[
                {"id":"claude-sonnet-test"},
                {"id":"grok-test"},
                {"id":"gemini-test"},
                {"id":"gpt-test"}
            ]}),
            None,
        )
        .unwrap();
        assert_eq!(models[0].protocols, [ProviderProtocol::ClaudeMessages]);
        assert_eq!(models[1].protocols, [ProviderProtocol::OpenAiResponses]);
        assert_eq!(models[2].protocols, [ProviderProtocol::OpenAiChat]);
        assert_eq!(
            models[3].protocols,
            [
                ProviderProtocol::OpenAiChat,
                ProviderProtocol::OpenAiResponses
            ]
        );
    }

    #[test]
    fn credentials_debug_never_contains_tokens() {
        let credentials = CopilotCredentials {
            github_access_token: "secret-access".into(),
            github_refresh_token: Some("secret-refresh".into()),
            github_expires_at: None,
            github_refresh_token_expires_at: None,
            token_type: "bearer".into(),
        };
        let rendered = format!("{credentials:?}");
        assert!(!rendered.contains("secret-access"));
        assert!(!rendered.contains("secret-refresh"));
    }

    #[tokio::test]
    async fn imports_github_token_discovers_models_and_streams_native_chat() {
        let server = MockServer::start().await;
        let directory = tempfile::tempdir().unwrap();
        mount_account_exchange(&server, "github-token").await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer copilot-api-token"))
            .and(header("accept", "text/event-stream"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "model":"gpt-test",
                "messages":[{"role":"user","content":"hi"}],
                "stream":true,
                "stream_options":{"include_usage":true}
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .insert_header("x-request-id", "copilot-request")
                    .set_body_raw(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n",
                        "text/event-stream",
                    ),
            )
            .mount(&server)
            .await;

        let provider = mock_provider(&server, directory.path()).await;
        let account = provider
            .import_token("github-token".into(), Some("test".into()))
            .await
            .unwrap();
        assert_eq!(account.login, "octocat");
        assert_eq!(account.supported_models, ["gpt-test"]);
        assert_eq!(provider.models(false).await.unwrap().len(), 1);

        let unavailable = match provider
            .execute(ProviderRequest {
                original_model: None,
                protocol: ProviderProtocol::OpenAiChat,
                model: "missing-model".into(),
                path: "/v1/chat/completions".into(),
                headers: HeaderMap::new(),
                body: bytes::Bytes::from_static(br#"{"model":"missing-model"}"#),
                trace_id: "trace-missing".into(),
                service_id: "svc-test".into(),
                account_pool: None,
                api_key_id: Some("ak-test".into()),
                client_context: Default::default(),
            })
            .await
        {
            Ok(_) => panic!("unsupported model must not reach Copilot"),
            Err(error) => error,
        };
        assert_eq!(
            unavailable.upstream_code.as_deref(),
            Some("model_not_available")
        );

        let response = provider
            .execute(ProviderRequest {
                    original_model: None,
                protocol: ProviderProtocol::OpenAiChat,
                model: "gpt-test".into(),
                path: "/v1/chat/completions".into(),
                headers: HeaderMap::new(),
                body: bytes::Bytes::from_static(
                    br#"{"model":"gpt-test","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
                ),
                trace_id: "trace-test".into(),
                service_id: "svc-test".into(),
                account_pool: None,
                api_key_id: Some("ak-test".into()),
                client_context: Default::default(),
            })
            .await
            .unwrap();
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(
            response.upstream_request_id.as_deref(),
            Some("copilot-request")
        );
        let mut stream = match response.body {
            ProviderResponseBody::Stream(stream) => stream,
            ProviderResponseBody::Full(_) => panic!("Copilot response must remain streaming"),
        };
        let mut output = Vec::new();
        while let Some(chunk) = stream.next().await {
            output.extend_from_slice(&chunk.unwrap());
        }
        assert!(String::from_utf8(output).unwrap().contains("[DONE]"));

        let persisted =
            tokio::fs::read_to_string(directory.path().join("providers/copilot/accounts.json"))
                .await
                .unwrap();
        assert!(persisted.contains("github-token"));
        let model_cache =
            tokio::fs::read_to_string(directory.path().join("providers/copilot/models.json"))
                .await
                .unwrap();
        assert!(model_cache.contains("gpt-test"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode =
                tokio::fs::metadata(directory.path().join("providers/copilot/accounts.json"))
                    .await
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777;
            assert_eq!(mode, 0o600);
            let mode = tokio::fs::metadata(directory.path().join("providers/copilot/models.json"))
                .await
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        drop(provider);
        let reloaded = mock_provider(&server, directory.path()).await;
        assert_eq!(reloaded.models(false).await.unwrap().len(), 1);
        server.verify().await;
    }

    #[tokio::test]
    async fn successful_empty_catalog_clears_account_and_persisted_cache() {
        let server = MockServer::start().await;
        let directory = tempfile::tempdir().unwrap();
        mount_account_exchange(&server, "github-token").await;
        let provider = mock_provider(&server, directory.path()).await;
        let account = provider
            .import_token("github-token".into(), None)
            .await
            .unwrap();
        server.verify().await;
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("authorization", "Bearer copilot-api-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        assert!(provider.models(true).await.unwrap().is_empty());
        assert!(provider
            .account(&account.id)
            .await
            .unwrap()
            .supported_models
            .is_empty());
        drop(provider);

        let reloaded = mock_provider(&server, directory.path()).await;
        assert!(reloaded.models(false).await.unwrap().is_empty());
        server.verify().await;
    }

    #[tokio::test]
    async fn browser_email_import_requires_a_verified_email_owned_by_the_token() {
        let server = MockServer::start().await;
        let directory = tempfile::tempdir().unwrap();
        mount_account_exchange(&server, "email-token").await;
        Mock::given(method("GET"))
            .and(path("/user/emails"))
            .and(header("authorization", "Bearer email-token"))
            .and(wiremock::matchers::query_param("per_page", "100"))
            .and(wiremock::matchers::query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"email":"unverified@example.com","verified":false},
                {"email":"private@example.com","verified":true}
            ])))
            .expect(3)
            .mount(&server)
            .await;
        let provider = mock_provider(&server, directory.path()).await;
        let credentials = CopilotCredentials {
            github_access_token: "email-token".into(),
            github_refresh_token: None,
            github_expires_at: None,
            github_refresh_token_expires_at: None,
            token_type: "bearer".into(),
        };
        for email in ["unverified@example.com", "other@example.com"] {
            let error = provider
                .import_credentials(credentials.clone(), None, Some(email))
                .await
                .unwrap_err();
            assert!(error.to_string().contains("does not match"));
            assert!(provider.account_summaries().await.is_empty());
        }
        assert!(server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| {
                request.url.path() != "/copilot_internal/v2/token"
                    && request.url.path() != "/models"
            }));
        let imported = provider
            .import_credentials(credentials, None, Some("PRIVATE@example.com"))
            .await
            .unwrap();
        assert_eq!(imported.login, "octocat");
        assert_eq!(provider.account_summaries().await.len(), 1);
    }

    #[tokio::test]
    async fn browser_email_verification_paginates_and_fails_closed_on_api_errors() {
        let server = MockServer::start().await;
        let directory = tempfile::tempdir().unwrap();
        let provider = mock_provider(&server, directory.path()).await;
        Mock::given(method("GET"))
            .and(path("/user/emails"))
            .and(wiremock::matchers::query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![
                serde_json::json!({"email":"other@example.com","verified":true});
                100
            ]))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/user/emails"))
            .and(wiremock::matchers::query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"email":"private@example.com","verified":true}
            ])))
            .expect(1)
            .mount(&server)
            .await;
        assert!(provider
            .github_verified_email("token", "private@example.com")
            .await
            .unwrap());
        server.verify().await;
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/user/emails"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        assert!(provider
            .github_verified_email("token", "private@example.com")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn device_flow_authorizes_and_imports_the_account() {
        let server = MockServer::start().await;
        let directory = tempfile::tempdir().unwrap();
        mount_account_exchange(&server, "device-token").await;
        Mock::given(method("POST"))
            .and(path("/login/device/code"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "device_code":"device-code",
                "user_code":"ABCD-EFGH",
                "verification_uri":"https://github.com/login/device",
                "expires_in":900,
                "interval":1
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token":"device-token",
                "token_type":"bearer",
                "expires_in":28800,
                "refresh_token":"refresh-token",
                "refresh_token_expires_in":15811200
            })))
            .mount(&server)
            .await;

        let provider = mock_provider(&server, directory.path()).await;
        assert!(provider.descriptor().capabilities.device_flow);
        let task = provider
            .start_device_login_for_user(Some("device".into()), Some("Octocat".into()))
            .await
            .unwrap();
        assert_eq!(task.status, DeviceLoginStatus::Pending);
        assert_eq!(task.user_code, "ABCD-EFGH");
        let completed = provider.poll_device_login(&task.id).await.unwrap();
        assert_eq!(completed.status, DeviceLoginStatus::Authorized);
        assert!(completed.account_id.is_some());
        assert_eq!(provider.account_summaries().await.len(), 1);
        let mismatch = provider
            .start_device_login_for_user(None, Some("another-user".into()))
            .await
            .unwrap();
        let rejected = provider.poll_device_login(&mismatch.id).await.unwrap();
        assert_eq!(rejected.status, DeviceLoginStatus::Failed);
        assert!(rejected.error.unwrap().contains("does not match"));
        assert_eq!(provider.account_summaries().await.len(), 1);
        let requests = server.received_requests().await.unwrap();
        for endpoint in ["/login/device/code", "/login/oauth/access_token"] {
            let request = requests
                .iter()
                .find(|request| request.url.path() == endpoint)
                .unwrap();
            let client_id = url::form_urlencoded::parse(&request.body)
                .find(|(name, _)| name == "client_id")
                .map(|(_, value)| value.into_owned());
            assert_eq!(client_id.as_deref(), Some(DEFAULT_COPILOT_OAUTH_CLIENT_ID));
        }
    }

    #[tokio::test]
    async fn github_refresh_uses_default_client_id() {
        let server = MockServer::start().await;
        let directory = tempfile::tempdir().unwrap();
        mount_account_exchange(&server, "github-token").await;
        let provider = mock_provider(&server, directory.path()).await;
        provider
            .import_token("github-token".into(), None)
            .await
            .unwrap();

        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token":"renewed-token",
                "refresh_token":"renewed-refresh-token",
                "expires_in":28800
            })))
            .expect(1)
            .mount(&server)
            .await;
        let mut account = provider.accounts.read().await[0].clone();
        account.credentials.github_refresh_token = Some("refresh-token".into());
        let token = provider.refresh_github_credentials(&account).await.unwrap();
        assert_eq!(token, "renewed-token");

        let requests = server.received_requests().await.unwrap();
        let request = requests
            .iter()
            .find(|request| request.url.path() == "/login/oauth/access_token")
            .unwrap();
        let fields = url::form_urlencoded::parse(&request.body)
            .into_owned()
            .collect::<HashMap<_, _>>();
        assert_eq!(
            fields.get("client_id").map(String::as_str),
            Some(DEFAULT_COPILOT_OAUTH_CLIENT_ID)
        );
        assert_eq!(
            fields.get("grant_type").map(String::as_str),
            Some("refresh_token")
        );
        assert_eq!(
            fields.get("refresh_token").map(String::as_str),
            Some("refresh-token")
        );
        assert!(!fields.contains_key("client_secret"));
        server.verify().await;
    }

    #[tokio::test]
    async fn forwards_native_messages_responses_and_token_counting() {
        let server = MockServer::start().await;
        let directory = tempfile::tempdir().unwrap();
        mount_account_exchange(&server, "github-token").await;
        let message_history = serde_json::json!([
            {"role": "user", "content": [{"type": "text", "text": "hello"}]},
            {"role": "system", "content": [], "output_config": {"effort": "low"}},
            {"role": "user", "content": "say ok"}
        ]);
        let messages = serde_json::json!({
            "model": "gpt-test",
            "max_tokens": 128,
            "stream": false,
            "messages": message_history.clone(),
            "output_config": {"effort": "high"},
            "tools": [{"name": "lookup", "description": "Lookup", "input_schema": {"type": "object"}}],
            "context_management": {"edits": []},
            "mcp_servers": [{"name": "unsupported"}],
            "safeguards": [{"type": "dangerous_tool_use", "classifier_context": "synthetic context"}]
        });
        let messages_upstream = serde_json::json!({
            "model": "gpt-test",
            "max_tokens": 128,
            "messages": message_history.clone(),
            "output_config": {"effort": "high"},
            "tools": [{"name": "lookup", "description": "Lookup", "input_schema": {"type": "object"}}],
            "context_management": {"edits": []}
        });
        let responses = serde_json::json!({
            "model": "gpt-test",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hello"}]}],
            "tools": [{"type": "function", "name": "lookup", "parameters": {"type": "object"}}],
            "stream": true
        });
        let count_upstream = serde_json::json!({
            "model": "gpt-test",
            "messages": message_history
        });
        let mut count = count_upstream.clone();
        count["safeguards"] = serde_json::json!([
            {"type": "dangerous_tool_use", "classifier_context": "synthetic context"}
        ]);
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("authorization", "Bearer copilot-api-token"))
            .and(header("user-agent", "GitHubCopilotChat/0.31.0"))
            .and(header("anthropic-version", DEFAULT_ANTHROPIC_VERSION))
            .and(wiremock::matchers::headers(
                "anthropic-beta",
                vec![
                    "context-management-2025-06-27",
                    "per-turn-control-2026-10-10",
                ],
            ))
            .and(wiremock::matchers::body_json(messages_upstream))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg-test",
                "type": "message",
                "content": [{"type": "text", "text": "ok"}],
                "usage": {"input_tokens": 5, "output_tokens": 1}
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("authorization", "Bearer copilot-api-token"))
            .and(header("accept", "text/event-stream"))
            .and(wiremock::matchers::body_json(responses.clone()))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
                "text/event-stream",
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages/count_tokens"))
            .and(header("authorization", "Bearer copilot-api-token"))
            .and(wiremock::matchers::headers(
                "anthropic-beta",
                vec![
                    "context-management-2025-06-27",
                    "per-turn-control-2026-10-10",
                ],
            ))
            .and(wiremock::matchers::body_json(count_upstream))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "input_tokens": 5
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = mock_provider(&server, directory.path()).await;
        provider
            .import_token("github-token".into(), None)
            .await
            .unwrap();
        let mut anthropic_headers = HeaderMap::new();
        anthropic_headers.insert(
            "anthropic-beta",
            HeaderValue::from_static(
                "context-management-2025-06-27,per-turn-control-2026-10-10,advisor-tool-2026-03-01",
            ),
        );
        let requests = [
            (
                false,
                ProviderRequest {
                    original_model: None,
                    protocol: ProviderProtocol::ClaudeMessages,
                    model: "gpt-test".into(),
                    path: "/v1/messages".into(),
                    headers: anthropic_headers.clone(),
                    body: serde_json::to_vec(&messages).unwrap().into(),
                    trace_id: "trace-messages".into(),
                    service_id: "svc-test".into(),
                    account_pool: None,
                    api_key_id: Some("ak-test".into()),
                    client_context: Default::default(),
                },
            ),
            (
                false,
                ProviderRequest {
                    original_model: None,
                    protocol: ProviderProtocol::OpenAiResponses,
                    model: "gpt-test".into(),
                    path: "/v1/responses".into(),
                    headers: HeaderMap::new(),
                    body: serde_json::to_vec(&responses).unwrap().into(),
                    trace_id: "trace-responses".into(),
                    service_id: "svc-test".into(),
                    account_pool: None,
                    api_key_id: Some("ak-test".into()),
                    client_context: Default::default(),
                },
            ),
            (
                true,
                ProviderRequest {
                    original_model: None,
                    protocol: ProviderProtocol::ClaudeMessages,
                    model: "gpt-test".into(),
                    path: "/v1/messages/count_tokens".into(),
                    headers: anthropic_headers,
                    body: serde_json::to_vec(&count).unwrap().into(),
                    trace_id: "trace-count".into(),
                    service_id: "svc-test".into(),
                    account_pool: None,
                    api_key_id: Some("ak-test".into()),
                    client_context: Default::default(),
                },
            ),
        ];
        for (count_tokens, request) in requests {
            let request_path = request.path.clone();
            let response = if count_tokens {
                provider.count_tokens(request).await.unwrap()
            } else {
                provider.execute(request).await.unwrap()
            };
            assert_eq!(
                response.status,
                StatusCode::OK,
                "upstream request {request_path}"
            );
            let mut body = match response.body {
                ProviderResponseBody::Stream(stream) => stream,
                ProviderResponseBody::Full(_) => panic!("Copilot response must remain streaming"),
            };
            let mut bytes = Vec::new();
            while let Some(chunk) = body.next().await {
                bytes.extend_from_slice(&chunk.unwrap());
            }
            assert!(!bytes.is_empty());
        }
        server.verify().await;
    }

    #[tokio::test]
    async fn failed_account_persistence_does_not_mutate_runtime_state() {
        let server = MockServer::start().await;
        let directory = tempfile::tempdir().unwrap();
        mount_account_exchange(&server, "github-token").await;
        let mut provider = mock_provider(&server, directory.path()).await;
        let account = provider
            .import_token("github-token".into(), None)
            .await
            .unwrap();
        let invalid_parent = directory.path().join("not-a-directory");
        tokio::fs::write(&invalid_parent, b"block parent creation")
            .await
            .unwrap();
        provider.accounts_path = invalid_parent.join("accounts.json");

        assert!(provider
            .set_account_enabled(&account.id, false)
            .await
            .is_err());
        assert!(provider.account(&account.id).await.unwrap().enabled);
        server.verify().await;
    }

    #[tokio::test]
    async fn upstream_health_errors_degrade_and_success_recovers_the_account() {
        let server = MockServer::start().await;
        let directory = tempfile::tempdir().unwrap();
        mount_account_exchange(&server, "github-token").await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
                "error": {"message": "temporarily unavailable"}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let provider = mock_provider(&server, directory.path()).await;
        let account = provider
            .import_token("github-token".into(), None)
            .await
            .unwrap();
        let request = ProviderRequest {
            original_model: None,
            protocol: ProviderProtocol::OpenAiChat,
            model: "gpt-test".into(),
            path: "/v1/chat/completions".into(),
            headers: HeaderMap::new(),
            body: bytes::Bytes::from_static(
                br#"{"model":"gpt-test","messages":[{"role":"user","content":"hi"}]}"#,
            ),
            trace_id: "trace-health".into(),
            service_id: "svc-test".into(),
            account_pool: None,
            api_key_id: Some("ak-test".into()),
            client_context: Default::default(),
        };
        let failed = provider.execute(request.clone()).await.unwrap();
        assert_eq!(failed.status, StatusCode::SERVICE_UNAVAILABLE);
        drop(failed);
        assert_eq!(provider.descriptor().status, "degraded");
        assert!(!provider.account(&account.id).await.unwrap().is_available());

        server.reset().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let recovered = provider.execute(request).await.unwrap();
        assert_eq!(recovered.status, StatusCode::OK);
        drop(recovered);
        assert_eq!(provider.descriptor().status, "ready");
        assert!(provider.account(&account.id).await.unwrap().is_available());
        server.verify().await;
    }
}
