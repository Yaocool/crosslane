//! API-key credit reservations and persistent multi-dimensional usage.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use crosslane_core::config::ApiKeyConfig;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

const MAX_USAGE_DAYS: usize = 366;
const MAX_USAGE_DIMENSION_KEYS: usize = 256;
const OTHER_USAGE_DIMENSION: &str = "other";

#[derive(Debug, Error)]
pub enum MeterError {
    #[error("invalid API key")]
    Unauthorized,
    #[error("API key credit limit exceeded")]
    LimitExceeded,
    #[error("service daily credit limit exceeded")]
    DailyLimitExceeded,
    #[error("usage persistence failed: {0}")]
    Persist(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageBucket {
    pub requests: u64,
    pub credits: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl UsageBucket {
    fn add(&mut self, record: &UsageRecord) {
        self.requests += 1;
        self.credits += record.credits;
        self.input_tokens += record.input_tokens;
        self.output_tokens += record.output_tokens;
    }

    fn merge(&mut self, other: &Self) {
        self.requests = self.requests.saturating_add(other.requests);
        self.credits += other.credits;
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderBilling {
    /// Version of this local envelope, independent of the upstream payload.
    pub schema_version: u32,
    /// Provider-native payload name, for example `copilot_usage`.
    pub source: String,
    /// Unit for `amount`; absent when the upstream schema is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// Exact integer amount in `unit`, when the upstream supplied one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<u64>,
    /// Original provider billing object. It is intentionally not converted to
    /// Kiro credits or a floating-point currency amount.
    pub raw: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    pub timestamp: i64,
    #[serde(default)]
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    pub model: String,
    pub original_model: Option<String>,
    pub kiro_model: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub credits: f64,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub token_usage_source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_billing: Option<ProviderBilling>,
    pub path: String,
}

/// Observed Copilot cost for one provider/account, independent of API keys.
/// Absent history remains unknown; amounts are kept in exact nano AI units.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountAiuUsage {
    pub total_nano_aiu: u64,
    pub billed_requests: u64,
    pub first_recorded_at: i64,
    pub last_recorded_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ApiKeyUsage {
    pub total_requests: u64,
    pub total_credits: f64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub daily: BTreeMap<String, UsageBucket>,
    #[serde(default)]
    pub by_provider: HashMap<String, UsageBucket>,
    #[serde(default)]
    pub provider_details: HashMap<String, ProviderApiKeyUsage>,
    pub by_model: HashMap<String, UsageBucket>,
    pub by_original_model: HashMap<String, UsageBucket>,
    pub by_kiro_model: HashMap<String, UsageBucket>,
    pub by_path: HashMap<String, UsageBucket>,
    pub history: VecDeque<UsageRecord>,
}

impl ApiKeyUsage {
    fn migrate_legacy_kiro_dimensions(&mut self) {
        // Before provider-aware accounting every persisted usage record was a
        // Kiro request. Preserve that exact aggregate when upgrading instead
        // of making `--provider kiro` appear empty after a restart.
        if self.total_requests == 0
            || !self.by_provider.is_empty()
            || !self.provider_details.is_empty()
        {
            return;
        }
        let total = UsageBucket {
            requests: self.total_requests,
            credits: self.total_credits,
            input_tokens: self.total_input_tokens,
            output_tokens: self.total_output_tokens,
        };
        self.by_provider.insert("kiro".into(), total.clone());
        self.provider_details.insert(
            "kiro".into(),
            ProviderApiKeyUsage {
                total,
                daily: self.daily.clone(),
                by_model: self.by_model.clone(),
                by_original_model: self.by_original_model.clone(),
                by_kiro_model: self.by_kiro_model.clone(),
                by_path: self.by_path.clone(),
            },
        );
    }

    fn add(&mut self, record: UsageRecord) {
        let provider = if record.provider_id.is_empty() {
            "kiro"
        } else {
            &record.provider_id
        };
        self.total_requests += 1;
        self.total_credits += record.credits;
        self.total_input_tokens += record.input_tokens;
        self.total_output_tokens += record.output_tokens;
        self.daily
            .entry(utc_day(record.timestamp))
            .or_default()
            .add(&record);
        while self.daily.len() > MAX_USAGE_DAYS {
            self.daily.pop_first();
        }
        usage_dimension_entry(&mut self.by_provider, provider).add(&record);
        provider_usage_entry(&mut self.provider_details, provider).add(&record);
        usage_dimension_entry(&mut self.by_model, &record.model).add(&record);
        if let Some(model) = &record.original_model {
            usage_dimension_entry(&mut self.by_original_model, model).add(&record);
        }
        if let Some(model) = &record.kiro_model {
            usage_dimension_entry(&mut self.by_kiro_model, model).add(&record);
        }
        usage_dimension_entry(&mut self.by_path, &record.path).add(&record);
        self.history.push_back(record);
        while self.history.len() > 100 {
            self.history.pop_front();
        }
    }

    fn bound(&mut self) {
        while self.daily.len() > MAX_USAGE_DAYS {
            self.daily.pop_first();
        }
        bound_usage_dimensions(&mut self.by_model);
        bound_usage_dimensions(&mut self.by_provider);
        bound_provider_usage(&mut self.provider_details);
        bound_usage_dimensions(&mut self.by_original_model);
        bound_usage_dimensions(&mut self.by_kiro_model);
        bound_usage_dimensions(&mut self.by_path);
        while self.history.len() > 100 {
            self.history.pop_front();
        }
    }

    pub fn retain_provider(&mut self, provider: &str) {
        let fallback = self.by_provider.get(provider).cloned().unwrap_or_default();
        let details = self.provider_details.get(provider).cloned();
        let total = details
            .as_ref()
            .map_or(fallback, |details| details.total.clone());
        self.total_requests = total.requests;
        self.total_credits = total.credits;
        self.total_input_tokens = total.input_tokens;
        self.total_output_tokens = total.output_tokens;
        self.by_provider
            .retain(|candidate, _| candidate == provider);
        self.provider_details
            .retain(|candidate, _| candidate == provider);
        if let Some(details) = details {
            self.daily = details.daily;
            self.by_model = details.by_model;
            self.by_original_model = details.by_original_model;
            self.by_kiro_model = details.by_kiro_model;
            self.by_path = details.by_path;
        } else {
            self.daily.clear();
            self.by_model.clear();
            self.by_original_model.clear();
            self.by_kiro_model.clear();
            self.by_path.clear();
        }
        self.history.retain(|record| {
            let record_provider = if record.provider_id.is_empty() {
                "kiro"
            } else {
                &record.provider_id
            };
            record_provider == provider
        });
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderApiKeyUsage {
    pub total: UsageBucket,
    pub daily: BTreeMap<String, UsageBucket>,
    pub by_model: HashMap<String, UsageBucket>,
    pub by_original_model: HashMap<String, UsageBucket>,
    pub by_kiro_model: HashMap<String, UsageBucket>,
    pub by_path: HashMap<String, UsageBucket>,
}

impl ProviderApiKeyUsage {
    fn add(&mut self, record: &UsageRecord) {
        self.total.add(record);
        self.daily
            .entry(utc_day(record.timestamp))
            .or_default()
            .add(record);
        while self.daily.len() > MAX_USAGE_DAYS {
            self.daily.pop_first();
        }
        usage_dimension_entry(&mut self.by_model, &record.model).add(record);
        if let Some(model) = &record.original_model {
            usage_dimension_entry(&mut self.by_original_model, model).add(record);
        }
        if let Some(model) = &record.kiro_model {
            usage_dimension_entry(&mut self.by_kiro_model, model).add(record);
        }
        usage_dimension_entry(&mut self.by_path, &record.path).add(record);
    }

    fn merge(&mut self, other: &Self) {
        self.total.merge(&other.total);
        for (day, bucket) in &other.daily {
            self.daily.entry(day.clone()).or_default().merge(bucket);
        }
        while self.daily.len() > MAX_USAGE_DAYS {
            self.daily.pop_first();
        }
        merge_usage_dimensions(&mut self.by_model, &other.by_model);
        merge_usage_dimensions(&mut self.by_original_model, &other.by_original_model);
        merge_usage_dimensions(&mut self.by_kiro_model, &other.by_kiro_model);
        merge_usage_dimensions(&mut self.by_path, &other.by_path);
    }

    fn bound(&mut self) {
        while self.daily.len() > MAX_USAGE_DAYS {
            self.daily.pop_first();
        }
        bound_usage_dimensions(&mut self.by_model);
        bound_usage_dimensions(&mut self.by_original_model);
        bound_usage_dimensions(&mut self.by_kiro_model);
        bound_usage_dimensions(&mut self.by_path);
    }
}

fn usage_dimension_entry<'a>(
    dimensions: &'a mut HashMap<String, UsageBucket>,
    key: &str,
) -> &'a mut UsageBucket {
    let bounded_key = if dimensions.contains_key(key)
        || (key != OTHER_USAGE_DIMENSION
            && dimensions.len() < MAX_USAGE_DIMENSION_KEYS.saturating_sub(1))
    {
        key
    } else {
        OTHER_USAGE_DIMENSION
    };
    dimensions.entry(bounded_key.to_string()).or_default()
}

fn bound_usage_dimensions(dimensions: &mut HashMap<String, UsageBucket>) {
    if dimensions.len() < MAX_USAGE_DIMENSION_KEYS {
        return;
    }
    let entries = std::mem::take(dimensions);
    for (key, bucket) in entries {
        usage_dimension_entry(dimensions, &key).merge(&bucket);
    }
}

fn merge_usage_dimensions(
    target: &mut HashMap<String, UsageBucket>,
    source: &HashMap<String, UsageBucket>,
) {
    for (key, bucket) in source {
        usage_dimension_entry(target, key).merge(bucket);
    }
}

fn provider_usage_entry<'a>(
    dimensions: &'a mut HashMap<String, ProviderApiKeyUsage>,
    provider: &str,
) -> &'a mut ProviderApiKeyUsage {
    let bounded_provider = if dimensions.contains_key(provider)
        || (provider != OTHER_USAGE_DIMENSION
            && dimensions.len() < MAX_USAGE_DIMENSION_KEYS.saturating_sub(1))
    {
        provider
    } else {
        OTHER_USAGE_DIMENSION
    };
    dimensions.entry(bounded_provider.into()).or_default()
}

fn bound_provider_usage(dimensions: &mut HashMap<String, ProviderApiKeyUsage>) {
    if dimensions.len() < MAX_USAGE_DIMENSION_KEYS {
        for usage in dimensions.values_mut() {
            usage.bound();
        }
        return;
    }
    let entries = std::mem::take(dimensions);
    for (provider, usage) in entries {
        provider_usage_entry(dimensions, &provider).merge(&usage);
    }
    for usage in dimensions.values_mut() {
        usage.bound();
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiKeyView {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub skip_user_agent_check: bool,
    pub credits_limit: Option<f64>,
    pub allowed_providers: Vec<String>,
    pub allowed_models: Vec<String>,
    pub reserved_credits: f64,
    pub usage: ApiKeyUsage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedApiKey {
    pub id: String,
    pub skip_user_agent_check: bool,
    pub allowed_providers: Vec<String>,
    pub allowed_models: Vec<String>,
}

struct KeyState {
    id: String,
    config: ApiKeyConfig,
    usage: ApiKeyUsage,
    reserved: f64,
}

// Authentication metadata is independent of the frequently mutated usage
// ledger. The digest is only an index: candidates still verify the actual key.
// Neither credentials nor this index participate in metering persistence.
struct AuthenticationCandidate {
    key: String,
    identity: AuthenticatedApiKey,
}

struct AuthenticationIndex {
    configured: bool,
    by_digest: HashMap<[u8; 32], Vec<AuthenticationCandidate>>,
}

impl AuthenticationIndex {
    fn from_keys(keys: &HashMap<String, KeyState>) -> Self {
        let mut by_digest: HashMap<_, Vec<_>> = HashMap::new();
        // Preserve the ledger's existing first-enabled-match behavior when
        // several configured identities happen to share the same token.
        for state in keys.values().filter(|state| state.config.enabled) {
            by_digest
                .entry(token_digest(&state.config.key))
                .or_default()
                .push(AuthenticationCandidate {
                    key: state.config.key.clone(),
                    identity: AuthenticatedApiKey {
                        id: state.id.clone(),
                        skip_user_agent_check: state.config.skip_user_agent_check,
                        allowed_providers: state.config.allowed_providers.clone(),
                        allowed_models: state.config.allowed_models.clone(),
                    },
                });
        }
        Self {
            // Configuring only disabled keys must not enable anonymous access.
            configured: !keys.is_empty(),
            by_digest,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default)]
    api_keys: HashMap<String, ApiKeyUsage>,
    #[serde(default)]
    service_daily: ServiceDailyUsage,
    #[serde(default)]
    account_aiu_usage: HashMap<String, AccountAiuUsage>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ServiceDailyUsage {
    day: String,
    used: f64,
}

#[derive(Debug, Default)]
struct ServiceCreditState {
    day: String,
    used: f64,
    reserved: f64,
    limit: f64,
}

pub struct Meter {
    path: PathBuf,
    keys: Mutex<HashMap<String, KeyState>>,
    authentication: RwLock<Arc<AuthenticationIndex>>,
    account_aiu_usage: Mutex<HashMap<String, AccountAiuUsage>>,
    service: Mutex<ServiceCreditState>,
    persist_lock: tokio::sync::Mutex<()>,
    persist_scheduled: AtomicBool,
    dirty_generation: AtomicU64,
    persisted_generation: AtomicU64,
    #[cfg(test)]
    persist_writes: AtomicU64,
    recovery_error: Mutex<Option<String>>,
}

pub struct CreditReservation {
    meter: Arc<Meter>,
    key_id: Option<String>,
    estimate: f64,
    settled: bool,
}

impl Meter {
    #[cfg(test)]
    pub fn empty(path: &Path, configs: &[ApiKeyConfig]) -> Arc<Self> {
        let keys = configs
            .iter()
            .cloned()
            .map(|config| {
                let id = config.id.clone().unwrap_or_else(|| key_id(&config.key));
                (
                    id.clone(),
                    KeyState {
                        id,
                        config,
                        usage: ApiKeyUsage::default(),
                        reserved: 0.0,
                    },
                )
            })
            .collect();
        Arc::new(Self {
            path: path.into(),
            authentication: RwLock::new(Arc::new(AuthenticationIndex::from_keys(&keys))),
            keys: Mutex::new(keys),
            account_aiu_usage: Mutex::new(HashMap::new()),
            service: Mutex::new(ServiceCreditState {
                day: utc_day(now_secs()),
                ..ServiceCreditState::default()
            }),
            persist_lock: tokio::sync::Mutex::new(()),
            persist_scheduled: AtomicBool::new(false),
            dirty_generation: AtomicU64::new(0),
            persisted_generation: AtomicU64::new(0),
            #[cfg(test)]
            persist_writes: AtomicU64::new(0),
            recovery_error: Mutex::new(None),
        })
    }

    pub fn fail_closed(
        path: &Path,
        configs: &[ApiKeyConfig],
        error: impl Into<String>,
    ) -> Arc<Self> {
        let keys = configs
            .iter()
            .cloned()
            .map(|config| {
                let id = config.id.clone().unwrap_or_else(|| key_id(&config.key));
                (
                    id.clone(),
                    KeyState {
                        id,
                        config,
                        usage: ApiKeyUsage::default(),
                        reserved: 0.0,
                    },
                )
            })
            .collect();
        Arc::new(Self {
            path: path.into(),
            authentication: RwLock::new(Arc::new(AuthenticationIndex::from_keys(&keys))),
            keys: Mutex::new(keys),
            account_aiu_usage: Mutex::new(HashMap::new()),
            service: Mutex::new(ServiceCreditState {
                day: utc_day(now_secs()),
                ..ServiceCreditState::default()
            }),
            persist_lock: tokio::sync::Mutex::new(()),
            persist_scheduled: AtomicBool::new(false),
            dirty_generation: AtomicU64::new(0),
            persisted_generation: AtomicU64::new(0),
            #[cfg(test)]
            persist_writes: AtomicU64::new(0),
            recovery_error: Mutex::new(Some(error.into())),
        })
    }

    pub async fn load(path: &Path, configs: &[ApiKeyConfig]) -> Result<Arc<Self>, MeterError> {
        let persisted = match tokio::fs::read_to_string(path).await {
            Ok(raw) if !raw.trim().is_empty() => {
                serde_json::from_str::<Persisted>(&raw).map_err(|error| {
                    MeterError::Persist(format!("parse {}: {error}", path.display()))
                })?
            }
            Ok(_) => Persisted::default(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Persisted::default(),
            Err(error) => {
                return Err(MeterError::Persist(format!(
                    "read {}: {error}",
                    path.display()
                )))
            }
        };
        let today = utc_day(now_secs());
        let service_used = if persisted.service_daily.day == today {
            persisted.service_daily.used.max(0.0)
        } else {
            0.0
        };
        let keys = configs
            .iter()
            .cloned()
            .map(|config| {
                let generated = config.id.is_none();
                let id = config.id.clone().unwrap_or_else(|| key_id(&config.key));
                let mut usage = persisted
                    .api_keys
                    .get(&id)
                    .or_else(|| {
                        generated
                            .then(|| persisted.api_keys.get(&legacy_key_id(&config.key)))
                            .flatten()
                    })
                    .cloned()
                    .unwrap_or_default();
                usage.migrate_legacy_kiro_dimensions();
                usage.bound();
                (
                    id.clone(),
                    KeyState {
                        id,
                        config,
                        usage,
                        reserved: 0.0,
                    },
                )
            })
            .collect();
        Ok(Arc::new(Self {
            path: path.into(),
            authentication: RwLock::new(Arc::new(AuthenticationIndex::from_keys(&keys))),
            keys: Mutex::new(keys),
            account_aiu_usage: Mutex::new(persisted.account_aiu_usage),
            service: Mutex::new(ServiceCreditState {
                day: today,
                used: service_used,
                reserved: 0.0,
                limit: 0.0,
            }),
            persist_lock: tokio::sync::Mutex::new(()),
            persist_scheduled: AtomicBool::new(false),
            dirty_generation: AtomicU64::new(0),
            persisted_generation: AtomicU64::new(0),
            #[cfg(test)]
            persist_writes: AtomicU64::new(0),
            recovery_error: Mutex::new(None),
        }))
    }

    pub fn set_daily_limit(&self, limit: f64) {
        lock(&self.service).limit = limit.max(0.0);
    }

    pub fn account_aiu_usage(&self, provider: &str, account: &str) -> Option<AccountAiuUsage> {
        lock(&self.account_aiu_usage)
            .get(&format!("{provider}/{account}"))
            .cloned()
    }

    fn record_account_aiu(&self, record: &UsageRecord) {
        let Some(account) = record.account_id.as_deref().filter(|id| !id.is_empty()) else {
            return;
        };
        let Some(billing) = record.provider_billing.as_ref().filter(|billing| {
            billing.source == "copilot_usage" && billing.unit.as_deref() == Some("nano_aiu")
        }) else {
            return;
        };
        let Some(amount) = billing.amount else {
            return;
        };
        let mut accounts = lock(&self.account_aiu_usage);
        let usage = accounts
            .entry(format!("{}/{account}", record.provider_id))
            .or_insert(AccountAiuUsage {
                total_nano_aiu: 0,
                billed_requests: 0,
                first_recorded_at: record.timestamp,
                last_recorded_at: record.timestamp,
            });
        usage.total_nano_aiu = usage.total_nano_aiu.saturating_add(amount);
        usage.billed_requests = usage.billed_requests.saturating_add(1);
        usage.first_recorded_at = usage.first_recorded_at.min(record.timestamp);
        usage.last_recorded_at = usage.last_recorded_at.max(record.timestamp);
    }

    pub fn daily_snapshot(&self) -> (String, f64, f64, f64) {
        let service = lock(&self.service);
        (
            service.day.clone(),
            service.used,
            service.reserved,
            service.limit,
        )
    }

    pub fn recovery_error(&self) -> Option<String> {
        lock(&self.recovery_error).clone()
    }

    pub async fn reset_daily(&self) -> Result<(), MeterError> {
        let previous_recovery = lock(&self.recovery_error).take();
        {
            let mut service = lock(&self.service);
            service.day = utc_day(now_secs());
            service.used = 0.0;
        }
        if let Err(error) = self.persist().await {
            if let Some(previous) = previous_recovery {
                *lock(&self.recovery_error) = Some(previous);
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn authenticate(
        &self,
        presented: Option<&str>,
    ) -> Result<Option<AuthenticatedApiKey>, MeterError> {
        let authentication = Arc::clone(
            &self
                .authentication
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        if !authentication.configured {
            return Ok(None);
        }
        let presented = presented.ok_or(MeterError::Unauthorized)?;
        authentication
            .by_digest
            .get(&token_digest(presented))
            .and_then(|candidates| {
                candidates
                    .iter()
                    .find(|candidate| constant_time_eq(&candidate.key, presented))
            })
            .map(|candidate| Some(candidate.identity.clone()))
            .ok_or(MeterError::Unauthorized)
    }

    pub fn reserve(
        self: &Arc<Self>,
        key_id: Option<&str>,
        estimate: f64,
    ) -> Result<CreditReservation, MeterError> {
        if let Some(error) = self.recovery_error() {
            return Err(MeterError::Persist(format!(
                "metering is in fail-closed recovery mode: {error}"
            )));
        }
        let estimate = estimate.max(0.0);
        {
            let mut service = lock(&self.service);
            reset_service_day_if_needed(&mut service);
            if service.limit > 0.0 && service.used + service.reserved + estimate > service.limit {
                return Err(MeterError::DailyLimitExceeded);
            }
            service.reserved += estimate;
        }
        if let Some(id) = key_id {
            let mut keys = lock(&self.keys);
            let Some(state) = keys.get_mut(id) else {
                let mut service = lock(&self.service);
                service.reserved = (service.reserved - estimate).max(0.0);
                return Err(MeterError::Unauthorized);
            };
            if state
                .config
                .credits_limit
                .is_some_and(|limit| state.usage.total_credits + state.reserved + estimate > limit)
            {
                let mut service = lock(&self.service);
                service.reserved = (service.reserved - estimate).max(0.0);
                return Err(MeterError::LimitExceeded);
            }
            state.reserved += estimate;
        }
        Ok(CreditReservation {
            meter: Arc::clone(self),
            key_id: key_id.map(str::to_string),
            estimate,
            settled: false,
        })
    }

    pub fn list(&self) -> Vec<ApiKeyView> {
        let mut entries = lock(&self.keys)
            .values()
            .map(|state| ApiKeyView {
                id: state.id.clone(),
                name: state.config.name.clone(),
                enabled: state.config.enabled,
                skip_user_agent_check: state.config.skip_user_agent_check,
                credits_limit: state.config.credits_limit,
                allowed_providers: state.config.allowed_providers.clone(),
                allowed_models: state.config.allowed_models.clone(),
                reserved_credits: state.reserved,
                usage: state.usage.clone(),
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.name
                .to_ascii_lowercase()
                .cmp(&right.name.to_ascii_lowercase())
                .then_with(|| left.name.cmp(&right.name))
                .then_with(|| left.id.cmp(&right.id))
        });
        entries
    }

    pub async fn reset_usage(self: &Arc<Self>, id_or_name: &str) -> Result<bool, MeterError> {
        {
            let mut keys = lock(&self.keys);
            let Some(id) = keys
                .get(id_or_name)
                .or_else(|| keys.values().find(|state| state.config.name == id_or_name))
                .map(|state| state.id.clone())
            else {
                return Ok(false);
            };
            let state = keys
                .get_mut(&id)
                .expect("selected key exists under the same lock");
            state.usage = ApiKeyUsage::default();
        }
        self.persist().await?;
        Ok(true)
    }

    pub fn replace_configs(&self, configs: &[ApiKeyConfig]) {
        let mut keys = lock(&self.keys);
        let mut next = HashMap::new();
        for config in configs.iter().cloned() {
            let id = config.id.clone().unwrap_or_else(|| key_id(&config.key));
            let (usage, reserved) = keys
                .remove(&id)
                .map(|state| (state.usage, state.reserved))
                .unwrap_or_default();
            next.insert(
                id.clone(),
                KeyState {
                    id,
                    config,
                    usage,
                    reserved,
                },
            );
        }
        let next_authentication = Arc::new(AuthenticationIndex::from_keys(&next));
        let mut authentication = self
            .authentication
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *keys = next;
        *authentication = next_authentication;
    }

    pub async fn persist(&self) -> Result<(), MeterError> {
        // Mark after the caller's mutations. A writer that has already copied
        // its snapshot cannot acknowledge this generation, while a queued
        // writer can cover several completed settlements in one durable write.
        let generation = self.dirty_generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.persist_through(generation).await
    }

    async fn persist_through(&self, generation: u64) -> Result<(), MeterError> {
        if let Some(error) = self.recovery_error() {
            return Err(MeterError::Persist(format!(
                "metering is in fail-closed recovery mode: {error}"
            )));
        }
        let _persist_guard = self.persist_lock.lock().await;
        if self.persisted_generation.load(Ordering::Acquire) >= generation {
            return Ok(());
        }
        // Capture the watermark before copying any state. Mutations arriving
        // during the copy or write must wait for a subsequent snapshot.
        let snapshot_generation = self.dirty_generation.load(Ordering::Acquire);
        let persisted = Persisted {
            api_keys: lock(&self.keys)
                .iter()
                .map(|(id, state)| (id.clone(), state.usage.clone()))
                .collect(),
            service_daily: {
                let service = lock(&self.service);
                ServiceDailyUsage {
                    day: service.day.clone(),
                    used: service.used,
                }
            },
            account_aiu_usage: lock(&self.account_aiu_usage).clone(),
        };
        let bytes = tokio::task::spawn_blocking(move || serde_json::to_vec_pretty(&persisted))
            .await
            .map_err(|error| MeterError::Persist(format!("encode metering state: {error}")))?
            .map_err(|error| MeterError::Persist(error.to_string()))?;
        crosslane_store::atomic::write_bytes_atomically(&self.path, &bytes, Some(0o600))
            .await
            .map_err(|error| MeterError::Persist(error.to_string()))?;
        // Atomic writes include syncing both the file and its parent directory.
        // Never advance this watermark on failure or before durable completion.
        self.persisted_generation
            .store(snapshot_generation, Ordering::Release);
        #[cfg(test)]
        self.persist_writes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn mark_dirty(self: &Arc<Self>) {
        self.dirty_generation.fetch_add(1, Ordering::AcqRel);
        if self
            .persist_scheduled
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let meter = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let generation = meter.dirty_generation.load(Ordering::Acquire);
                if let Err(error) = meter.persist_through(generation).await {
                    tracing::error!(%error, "failed to persist metering state");
                }
                if meter.dirty_generation.load(Ordering::Acquire) != generation {
                    continue;
                }
                meter.persist_scheduled.store(false, Ordering::Release);
                if meter.dirty_generation.load(Ordering::Acquire) != generation
                    && meter
                        .persist_scheduled
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                {
                    continue;
                }
                break;
            }
        });
    }
}

impl CreditReservation {
    /// Reserve capacity for an additional upstream round while keeping the
    /// original request atomic from the caller's perspective.
    pub fn extend(&mut self, additional: f64) -> Result<(), MeterError> {
        let additional = additional.max(0.0);
        if additional == 0.0 {
            return Ok(());
        }
        {
            let mut service = lock(&self.meter.service);
            reset_service_day_if_needed(&mut service);
            if service.limit > 0.0 && service.used + service.reserved + additional > service.limit {
                return Err(MeterError::DailyLimitExceeded);
            }
            service.reserved += additional;
        }
        if let Some(id) = &self.key_id {
            let mut keys = lock(&self.meter.keys);
            let Some(state) = keys.get_mut(id) else {
                let mut service = lock(&self.meter.service);
                service.reserved = (service.reserved - additional).max(0.0);
                return Err(MeterError::Unauthorized);
            };
            if state.config.credits_limit.is_some_and(|limit| {
                state.usage.total_credits + state.reserved + additional > limit
            }) {
                let mut service = lock(&self.meter.service);
                service.reserved = (service.reserved - additional).max(0.0);
                return Err(MeterError::LimitExceeded);
            }
            state.reserved += additional;
        }
        self.estimate += additional;
        Ok(())
    }

    pub async fn settle(mut self, record: UsageRecord) -> Result<(), MeterError> {
        self.meter.record_account_aiu(&record);
        let mut durable_limit_state = {
            let mut service = lock(&self.meter.service);
            reset_service_day_if_needed(&mut service);
            service.reserved = (service.reserved - self.estimate).max(0.0);
            service.used += record.credits.max(0.0);
            service.limit > 0.0
        };
        if let Some(id) = &self.key_id {
            if let Some(state) = lock(&self.meter.keys).get_mut(id) {
                state.reserved = (state.reserved - self.estimate).max(0.0);
                state.usage.add(record);
                durable_limit_state |= state.config.credits_limit.is_some();
            }
        }
        self.settled = true;
        if durable_limit_state {
            self.meter.persist().await
        } else {
            self.meter.mark_dirty();
            Ok(())
        }
    }
}

impl Drop for CreditReservation {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        {
            let mut service = lock(&self.meter.service);
            service.reserved = (service.reserved - self.estimate).max(0.0);
        }
        if let Some(id) = &self.key_id {
            if let Some(state) = lock(&self.meter.keys).get_mut(id) {
                state.reserved = (state.reserved - self.estimate).max(0.0);
            }
        }
    }
}

fn reset_service_day_if_needed(service: &mut ServiceCreditState) {
    let today = utc_day(now_secs());
    if service.day != today {
        service.day = today;
        service.used = 0.0;
    }
}

fn token_digest(key: &str) -> [u8; 32] {
    Sha256::digest(key.as_bytes()).into()
}

fn key_id(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    digest[..8]
        .iter()
        .fold(String::from("ak_"), |mut output, byte| {
            use std::fmt::Write as _;
            let _ = write!(output, "{byte:02x}");
            output
        })
}

fn legacy_key_id(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    format!("ak_{:02x}{:02x}", digest[0], digest[1])
}

pub(crate) fn constant_time_eq(expected: &str, presented: &str) -> bool {
    let expected = expected.as_bytes();
    let presented = presented.as_bytes();
    let mut difference = expected.len() ^ presented.len();
    for index in 0..expected.len().max(presented.len()) {
        difference |= expected.get(index).copied().unwrap_or(0) as usize
            ^ presented.get(index).copied().unwrap_or(0) as usize;
    }
    difference == 0
}

fn utc_day(timestamp: i64) -> String {
    let days = timestamp.div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

// Howard Hinnant's civil calendar conversion, epoch adjusted to Unix day zero.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosslane_core::config::ApiKeyFormat;

    fn authentication_config(id: &str, key: &str) -> ApiKeyConfig {
        ApiKeyConfig {
            id: Some(id.into()),
            key: key.into(),
            enabled: true,
            ..ApiKeyConfig::default()
        }
    }

    fn usage(credits: f64) -> UsageRecord {
        UsageRecord {
            timestamp: now_secs(),
            provider_id: "kiro".into(),
            account_id: None,
            model: "mapped".into(),
            original_model: Some("client".into()),
            kiro_model: Some("kiro".into()),
            input_tokens: 100,
            output_tokens: 50,
            credits,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            token_usage_source: "server".into(),
            provider_billing: None,
            path: "/v1/messages".into(),
        }
    }

    #[test]
    fn authentication_index_preserves_empty_disabled_and_duplicate_key_semantics() {
        let directory = tempfile::tempdir().expect("tempdir");
        let meter = Meter::empty(&directory.path().join("daily.json"), &[]);
        assert_eq!(meter.authenticate(None).unwrap(), None);
        assert_eq!(meter.authenticate(Some("unconfigured")).unwrap(), None);

        let mut disabled = authentication_config("disabled", "shared-token");
        disabled.enabled = false;
        meter.replace_configs(std::slice::from_ref(&disabled));
        assert!(matches!(
            meter.authenticate(None),
            Err(MeterError::Unauthorized)
        ));
        assert!(matches!(
            meter.authenticate(Some("shared-token")),
            Err(MeterError::Unauthorized)
        ));
        meter.replace_configs(&[
            disabled,
            authentication_config("enabled-a", "shared-token"),
            authentication_config("enabled-b", "shared-token"),
        ]);
        let expected = lock(&meter.keys)
            .values()
            .find(|state| {
                state.config.enabled && constant_time_eq(&state.config.key, "shared-token")
            })
            .expect("legacy first enabled match")
            .id
            .clone();
        assert_eq!(
            meter
                .authenticate(Some("shared-token"))
                .unwrap()
                .unwrap()
                .id,
            expected
        );

        meter.replace_configs(&[
            authentication_config("same-id", "old-token"),
            authentication_config("same-id", "new-token"),
        ]);
        assert!(matches!(
            meter.authenticate(Some("old-token")),
            Err(MeterError::Unauthorized)
        ));
        assert_eq!(
            meter.authenticate(Some("new-token")).unwrap().unwrap().id,
            "same-id"
        );
        let mut generated = authentication_config("unused", "generated-id-token");
        generated.id = None;
        meter.replace_configs(&[generated]);
        assert_eq!(
            meter
                .authenticate(Some("generated-id-token"))
                .unwrap()
                .unwrap()
                .id,
            key_id("generated-id-token")
        );
        meter.replace_configs(&[]);
        assert_eq!(meter.authenticate(None).unwrap(), None);
    }

    #[test]
    fn authentication_rotation_updates_permissions_without_resetting_usage() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut config = authentication_config("stable-id", "old-token");
        config.credits_limit = Some(10.0);
        let meter = Meter::empty(&directory.path().join("daily.json"), &[config.clone()]);
        lock(&meter.keys)
            .get_mut("stable-id")
            .unwrap()
            .usage
            .add(usage(2.0));
        let reservation = meter.reserve(Some("stable-id"), 3.0).expect("reserve");
        config.key = "rotated-token".into();
        config.skip_user_agent_check = true;
        config.allowed_providers = vec!["copilot".into()];
        config.allowed_models = vec!["gpt-5-mini".into()];
        meter.replace_configs(&[config]);

        assert!(matches!(
            meter.authenticate(Some("old-token")),
            Err(MeterError::Unauthorized)
        ));
        assert_eq!(
            meter.authenticate(Some("rotated-token")).unwrap().unwrap(),
            AuthenticatedApiKey {
                id: "stable-id".into(),
                skip_user_agent_check: true,
                allowed_providers: vec!["copilot".into()],
                allowed_models: vec!["gpt-5-mini".into()],
            }
        );
        let view = meter.list().pop().unwrap();
        assert_eq!(view.usage.total_credits, 2.0);
        assert_eq!(view.reserved_credits, 3.0);
        drop(reservation);
        assert_eq!(meter.list()[0].reserved_credits, 0.0);
    }

    #[test]
    fn authentication_candidates_still_verify_the_presented_token() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config = authentication_config("valid-id", "actual-token");
        let meter = Meter::empty(&directory.path().join("daily.json"), &[config]);
        let mut index = AuthenticationIndex::from_keys(&lock(&meter.keys));
        let actual = index
            .by_digest
            .remove(&token_digest("actual-token"))
            .unwrap();
        // Simulate a digest collision: an index hit alone is not authentication.
        index
            .by_digest
            .insert(token_digest("different-token"), actual);
        *meter.authentication.write().unwrap() = Arc::new(index);
        assert!(matches!(
            meter.authenticate(Some("different-token")),
            Err(MeterError::Unauthorized)
        ));
    }

    #[test]
    fn authentication_does_not_wait_for_the_usage_ledger_lock() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config = authentication_config("valid-id", "actual-token");
        let meter = Meter::empty(&directory.path().join("daily.json"), &[config]);
        let ledger = lock(&meter.keys);
        let (send, receive) = std::sync::mpsc::channel();
        let auth_meter = Arc::clone(&meter);
        let worker = std::thread::spawn(move || {
            send.send(auth_meter.authenticate(Some("actual-token")))
                .unwrap();
        });
        let result = receive.recv_timeout(Duration::from_secs(1));
        // Release before joining so a regression fails instead of deadlocking.
        drop(ledger);
        worker.join().expect("authentication worker");
        assert_eq!(
            result
                .expect("authentication bypasses ledger lock")
                .unwrap()
                .unwrap()
                .id,
            "valid-id"
        );
    }

    #[tokio::test]
    async fn authentication_index_loads_without_persisting_credentials_or_bypassing_recovery() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("daily.json");
        let config = authentication_config("stable-id", "fixture-secret-not-persisted");
        let meter = Meter::load(&path, std::slice::from_ref(&config))
            .await
            .unwrap();
        assert_eq!(
            meter.authenticate(Some(&config.key)).unwrap().unwrap().id,
            "stable-id"
        );
        meter.persist().await.expect("persist usage only");
        let persisted = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(!persisted.contains(&config.key));
        assert!(!persisted.contains("by_digest"));

        let recovering = Meter::fail_closed(&path, &[config.clone()], "invalid persisted usage");
        assert_eq!(
            recovering
                .authenticate(Some(&config.key))
                .unwrap()
                .unwrap()
                .id,
            "stable-id"
        );
        assert!(matches!(
            recovering.reserve(Some("stable-id"), 0.0),
            Err(MeterError::Persist(_))
        ));
        let rotated = authentication_config("stable-id", "rotated-token");
        recovering.replace_configs(&[rotated]);
        assert!(matches!(
            recovering.authenticate(Some(&config.key)),
            Err(MeterError::Unauthorized)
        ));
        assert!(recovering.authenticate(Some("rotated-token")).is_ok());
        assert!(matches!(
            recovering.reserve(Some("stable-id"), 0.0),
            Err(MeterError::Persist(_))
        ));
    }

    /// Compares indexed authentication with the previous ledger scan, using
    /// synthetic tokens only. Run with `cargo test -p crosslaned authentication_scaling_profile -- --ignored --nocapture`.
    #[test]
    #[ignore = "manual API key authentication scaling profile"]
    fn authentication_scaling_profile() {
        fn linear_reference(meter: &Meter, presented: &str) -> Option<AuthenticatedApiKey> {
            lock(&meter.keys)
                .values()
                .find(|state| {
                    state.config.enabled && constant_time_eq(&state.config.key, presented)
                })
                .map(|state| AuthenticatedApiKey {
                    id: state.id.clone(),
                    skip_user_agent_check: state.config.skip_user_agent_check,
                    allowed_providers: state.config.allowed_providers.clone(),
                    allowed_models: state.config.allowed_models.clone(),
                })
        }
        for key_count in [1, 100, 1_000, 10_000] {
            let directory = tempfile::tempdir().expect("tempdir");
            let configs = (0..key_count)
                .map(|index| {
                    authentication_config(&format!("key-{index}"), &format!("fixture-{index:08}"))
                })
                .collect::<Vec<_>>();
            let meter = Meter::empty(&directory.path().join("daily.json"), &configs);
            let valid = &configs[key_count - 1].key;
            for (case, presented) in [("hit", valid.as_str()), ("miss", "fixture-missing!")] {
                assert_eq!(
                    meter.authenticate(Some(presented)).ok().flatten(),
                    linear_reference(&meter, presented)
                );
                let start = std::time::Instant::now();
                for _ in 0..10_000 {
                    let _ = std::hint::black_box(
                        meter.authenticate(Some(std::hint::black_box(presented))),
                    );
                }
                let indexed_ns = start.elapsed().as_nanos() / 10_000;
                let start = std::time::Instant::now();
                for _ in 0..100 {
                    let _ = std::hint::black_box(linear_reference(
                        &meter,
                        std::hint::black_box(presented),
                    ));
                }
                let linear_ns = start.elapsed().as_nanos() / 100;
                eprintln!("authentication keys={key_count} case={case} indexed_ns_per_lookup={indexed_ns} linear_ns_per_lookup={linear_ns} indexed_iterations=10000 linear_iterations=100");
            }
        }
    }

    #[test]
    fn usage_history_and_dimensions_are_bounded() {
        let mut aggregate = ApiKeyUsage::default();
        for index in 0..(MAX_USAGE_DIMENSION_KEYS + MAX_USAGE_DAYS + 100) {
            let mut record = usage(1.0);
            record.timestamp = index as i64 * 86_400;
            record.model = format!("model-{index}");
            record.original_model = Some(format!("original-{index}"));
            record.kiro_model = Some(format!("kiro-{index}"));
            record.path = format!("/path/{index}");
            aggregate.add(record);
        }

        assert_eq!(aggregate.daily.len(), MAX_USAGE_DAYS);
        assert_eq!(aggregate.by_model.len(), MAX_USAGE_DIMENSION_KEYS);
        assert_eq!(aggregate.by_original_model.len(), MAX_USAGE_DIMENSION_KEYS);
        assert_eq!(aggregate.by_kiro_model.len(), MAX_USAGE_DIMENSION_KEYS);
        assert_eq!(aggregate.by_path.len(), MAX_USAGE_DIMENSION_KEYS);
        assert_eq!(aggregate.history.len(), 100);
    }

    #[test]
    fn provider_filter_rebuilds_exact_usage_and_preserves_native_billing() {
        let mut aggregate = ApiKeyUsage::default();
        aggregate.add(usage(2.5));

        let mut copilot = usage(0.0);
        copilot.provider_id = "copilot".into();
        copilot.model = "gpt-5-mini".into();
        copilot.original_model = Some("team-fast".into());
        copilot.kiro_model = None;
        copilot.input_tokens = 12;
        copilot.output_tokens = 7;
        copilot.provider_billing = Some(ProviderBilling {
            schema_version: 1,
            source: "copilot_usage".into(),
            unit: Some("nano_aiu".into()),
            amount: Some(42),
            raw: serde_json::json!({"total_nano_aiu": 42}),
        });
        aggregate.add(copilot);

        aggregate.retain_provider("copilot");

        assert_eq!(aggregate.total_requests, 1);
        assert_eq!(aggregate.total_credits, 0.0);
        assert_eq!(aggregate.total_input_tokens, 12);
        assert_eq!(aggregate.total_output_tokens, 7);
        assert_eq!(aggregate.by_provider.len(), 1);
        assert!(aggregate.by_provider.contains_key("copilot"));
        assert_eq!(aggregate.by_model["gpt-5-mini"].requests, 1);
        assert_eq!(aggregate.by_original_model["team-fast"].requests, 1);
        assert!(aggregate.by_kiro_model.is_empty());
        assert_eq!(aggregate.history.len(), 1);
        let billing = aggregate.history[0]
            .provider_billing
            .as_ref()
            .expect("provider billing");
        assert_eq!(billing.unit.as_deref(), Some("nano_aiu"));
        assert_eq!(billing.amount, Some(42));
    }

    #[test]
    fn legacy_usage_is_backfilled_as_kiro_before_provider_filtering() {
        let mut aggregate = ApiKeyUsage::default();
        aggregate.add(usage(2.5));
        aggregate.by_provider.clear();
        aggregate.provider_details.clear();

        aggregate.migrate_legacy_kiro_dimensions();
        aggregate.retain_provider("kiro");

        assert_eq!(aggregate.total_requests, 1);
        assert_eq!(aggregate.total_credits, 2.5);
        assert_eq!(aggregate.total_input_tokens, 100);
        assert_eq!(aggregate.by_model["mapped"].requests, 1);
        assert_eq!(aggregate.by_path["/v1/messages"].requests, 1);
    }

    #[tokio::test]
    async fn reservations_prevent_concurrent_limit_overshoot() {
        let directory = tempfile::tempdir().expect("tempdir");
        let meter = Meter::load(
            &directory.path().join("daily.json"),
            &[ApiKeyConfig {
                id: Some("ak_test".into()),
                name: "test".into(),
                key: "secret".into(),
                format: ApiKeyFormat::Sk,
                enabled: true,
                skip_user_agent_check: false,
                credits_limit: Some(10.0),
                ..ApiKeyConfig::default()
            }],
        )
        .await
        .expect("load");
        let first = meter.reserve(Some("ak_test"), 6.0).expect("reserve");
        assert!(matches!(
            meter.reserve(Some("ak_test"), 6.0),
            Err(MeterError::LimitExceeded)
        ));
        drop(first);
        assert!(meter.reserve(Some("ak_test"), 6.0).is_ok());
    }

    #[tokio::test]
    async fn management_id_collision_never_resets_the_named_neighbors_usage() {
        let directory = tempfile::tempdir().unwrap();
        let configs =
            [("ak_first", "ak_second"), ("ak_second", "ak_first")].map(|(id, name)| ApiKeyConfig {
                id: Some(id.into()),
                name: name.into(),
                key: format!("fixture-{id}"),
                ..ApiKeyConfig::default()
            });
        let meter = Meter::load(&directory.path().join("daily.json"), &configs)
            .await
            .unwrap();
        // Cyclic aliases force at least one incorrect selection with a combined
        // ID/name search, independently of the HashMap's randomized order.
        for target in ["ak_first", "ak_second"] {
            {
                for state in lock(&meter.keys).values_mut() {
                    state.usage = ApiKeyUsage::default();
                    state.usage.add(usage(2.5));
                }
            }
            assert!(meter.reset_usage(target).await.unwrap());
            for key in meter.list() {
                assert_eq!(
                    key.usage.total_requests,
                    u64::from(key.id != target),
                    "reset affected a name instead of the exact ID"
                );
            }
        }
    }

    #[tokio::test]
    async fn api_key_list_is_sorted_by_name_and_id() {
        let directory = tempfile::tempdir().expect("tempdir");
        let configs = [
            ("ak_bravo", "Bravo"),
            ("ak_lower", "alpha"),
            ("ak_upper", "Alpha"),
        ]
        .map(|(id, name)| ApiKeyConfig {
            id: Some(id.into()),
            name: name.into(),
            key: format!("secret-{id}"),
            format: ApiKeyFormat::Sk,
            enabled: true,
            skip_user_agent_check: false,
            credits_limit: None,
            ..ApiKeyConfig::default()
        });
        let meter = Meter::load(&directory.path().join("daily.json"), &configs)
            .await
            .expect("load");

        assert_eq!(
            meter
                .list()
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["Alpha", "alpha", "Bravo"]
        );
    }

    #[tokio::test]
    async fn service_daily_limit_and_auto_round_extensions_survive_restart() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("daily.json");
        let meter = Meter::load(&path, &[]).await.expect("load");
        meter.set_daily_limit(10.0);
        let mut reservation = meter.reserve(None, 3.0).expect("first round");
        reservation.extend(3.0).expect("second round");
        assert!(matches!(
            reservation.extend(5.0),
            Err(MeterError::DailyLimitExceeded)
        ));
        reservation.settle(usage(6.0)).await.expect("settle");

        let reloaded = Meter::load(&path, &[]).await.expect("reload");
        reloaded.set_daily_limit(10.0);
        assert!(matches!(
            reloaded.reserve(None, 5.0),
            Err(MeterError::DailyLimitExceeded)
        ));
        assert_eq!(reloaded.daily_snapshot().1, 6.0);
    }

    #[tokio::test]
    async fn concurrent_limited_settlements_share_a_durable_snapshot() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("daily.json");
        let config = ApiKeyConfig {
            id: Some("ak_test".into()),
            key: "secret".into(),
            enabled: true,
            credits_limit: Some(100.0),
            ..ApiKeyConfig::default()
        };
        let meter = Meter::empty(&path, std::slice::from_ref(&config));
        meter.set_daily_limit(100.0);
        let gate = meter.persist_lock.lock().await;
        let mut settlements = Vec::new();
        for _ in 0..32 {
            let reservation = meter.reserve(Some("ak_test"), 1.0).expect("reserve");
            settlements.push(tokio::spawn(reservation.settle(usage(1.0))));
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while meter.dirty_generation.load(Ordering::Acquire) < 32 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("settlements wait for persistence");
        assert!(settlements.iter().all(|task| !task.is_finished()));
        assert!(!path.exists());
        assert_eq!(meter.persisted_generation.load(Ordering::Acquire), 0);
        drop(gate);
        for settlement in settlements {
            settlement.await.expect("join").expect("durable settlement");
        }

        assert_eq!(meter.persist_writes.load(Ordering::Relaxed), 1);
        assert_eq!(meter.persisted_generation.load(Ordering::Acquire), 32);
        let reloaded = Meter::load(&path, &[config]).await.expect("reload");
        assert_eq!(reloaded.daily_snapshot().1, 32.0);
        assert_eq!(reloaded.list()[0].usage.total_requests, 32);
        assert_eq!(reloaded.list()[0].usage.total_credits, 32.0);
        reloaded.set_daily_limit(33.0);
        assert!(matches!(
            reloaded.reserve(None, 2.0),
            Err(MeterError::DailyLimitExceeded)
        ));
    }

    #[tokio::test]
    async fn failed_snapshot_does_not_acknowledge_or_lose_settled_usage() {
        let directory = tempfile::tempdir().expect("tempdir");
        let parent = directory.path().join("blocked");
        tokio::fs::write(&parent, b"not a directory")
            .await
            .expect("block parent");
        let path = parent.join("daily.json");
        let meter = Meter::empty(&path, &[]);
        meter.set_daily_limit(10.0);
        assert!(matches!(
            meter.reserve(None, 2.0).unwrap().settle(usage(2.0)).await,
            Err(MeterError::Persist(_))
        ));
        assert_eq!(meter.persisted_generation.load(Ordering::Acquire), 0);
        assert_eq!(meter.persist_writes.load(Ordering::Relaxed), 0);
        assert_eq!(meter.daily_snapshot().1, 2.0);

        tokio::fs::remove_file(&parent)
            .await
            .expect("unblock parent");
        meter.persist().await.expect("retry persistence");
        let reloaded = Meter::load(&path, &[]).await.expect("reload");
        assert_eq!(reloaded.daily_snapshot().1, 2.0);
        assert_eq!(meter.persisted_generation.load(Ordering::Acquire), 2);
        assert_eq!(meter.persist_writes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn settlement_arriving_after_snapshot_capture_requires_another_durable_write() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let directory = tempfile::tempdir().expect("tempdir");
            let path = directory.path().join("daily.json");
            let meter = Meter::empty(&path, &[]);
            meter.set_daily_limit(10.0);
            // Hold the encoder worker so the first snapshot remains in flight
            // while the second settlement updates the ledger.
            let (release, blocked) = std::sync::mpsc::channel();
            let (started, waiting) = tokio::sync::oneshot::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                let _ = started.send(());
                let _ = blocked.recv();
            });
            waiting.await.expect("encoder worker blocked");
            let first = meter.reserve(None, 1.0).unwrap().settle(usage(1.0));
            tokio::pin!(first);
            let pending = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(std::future::Future::poll(first.as_mut(), cx))
            })
            .await;
            assert!(pending.is_pending());
            assert_eq!(meter.dirty_generation.load(Ordering::Acquire), 1);

            let second = meter.reserve(None, 1.0).unwrap().settle(usage(1.0));
            tokio::pin!(second);
            let pending = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(std::future::Future::poll(second.as_mut(), cx))
            })
            .await;
            assert!(pending.is_pending());
            assert_eq!(meter.dirty_generation.load(Ordering::Acquire), 2);
            assert_eq!(meter.persisted_generation.load(Ordering::Acquire), 0);

            release.send(()).expect("release encoder");
            let (first, second) = tokio::join!(first, second);
            first.expect("first durable settlement");
            second.expect("second durable settlement");
            blocker.await.expect("encoder blocker");
            assert_eq!(meter.persist_writes.load(Ordering::Relaxed), 2);
            assert_eq!(meter.persisted_generation.load(Ordering::Acquire), 2);
            let reloaded = Meter::load(&path, &[]).await.expect("reload");
            assert_eq!(reloaded.daily_snapshot().1, 2.0);
        });
    }

    /// Local filesystem benchmark; deliberately excluded from the normal suite.
    /// Run with `cargo test -p crosslaned durable_snapshot_burst_profile -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "manual durable snapshot throughput profile"]
    async fn durable_snapshot_burst_profile() {
        for key_count in [1, 100, 1_000] {
            let directory = tempfile::tempdir().expect("tempdir");
            let path = directory.path().join("daily.json");
            let configs = (0..key_count)
                .map(|index| ApiKeyConfig {
                    id: Some(format!("key-{index}")),
                    key: format!("secret-{index}"),
                    enabled: true,
                    credits_limit: Some(1_000.0),
                    ..ApiKeyConfig::default()
                })
                .collect::<Vec<_>>();
            let meter = Meter::empty(&path, &configs);
            for state in lock(&meter.keys).values_mut() {
                for _ in 0..100 {
                    state.usage.add(usage(1.0));
                }
            }
            let start = std::time::Instant::now();
            let mut tasks = Vec::new();
            for _ in 0..32 {
                let reservation = meter.reserve(Some("key-0"), 1.0).expect("reserve");
                tasks.push(tokio::spawn(reservation.settle(usage(1.0))));
            }
            for task in tasks {
                task.await.expect("join").expect("settle");
            }
            let elapsed = start.elapsed();
            let reloaded = Meter::load(&path, &configs).await.expect("reload");
            assert_eq!(lock(&reloaded.keys)["key-0"].usage.total_requests, 132);
            assert_eq!(reloaded.daily_snapshot().1, 32.0);
            eprintln!(
                "meter keys={key_count} history_per_key=100 settlements=32 elapsed_ms={:.3} durable_writes={} bytes={}",
                elapsed.as_secs_f64() * 1_000.0,
                meter.persist_writes.load(Ordering::Relaxed),
                tokio::fs::metadata(&path).await.expect("metadata").len(),
            );
        }
    }

    #[tokio::test]
    async fn corrupt_usage_file_fails_closed() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("daily.json");
        tokio::fs::write(&path, "{not-json").await.expect("write");
        assert!(matches!(
            Meter::load(&path, &[]).await,
            Err(MeterError::Persist(_))
        ));
    }

    #[tokio::test]
    async fn recovery_meter_blocks_requests_until_an_explicit_daily_reset() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("daily.json");
        let meter = Meter::fail_closed(&path, &[], "corrupt usage state");

        assert!(matches!(
            meter.reserve(None, 1.0),
            Err(MeterError::Persist(_))
        ));
        assert!(meter.recovery_error().is_some());

        meter.reset_daily().await.expect("explicit recovery reset");
        assert!(meter.recovery_error().is_none());
        assert!(meter.reserve(None, 1.0).is_ok());
        Meter::load(&path, &[])
            .await
            .expect("recovery state persisted");
    }

    #[tokio::test]
    async fn reset_usage_persists_immediately_without_releasing_inflight_credit() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("daily.json");
        let config = ApiKeyConfig {
            id: Some("ak_test".into()),
            name: "test".into(),
            key: "secret".into(),
            format: ApiKeyFormat::Sk,
            enabled: true,
            skip_user_agent_check: false,
            credits_limit: Some(10.0),
            ..ApiKeyConfig::default()
        };
        let meter = Meter::load(&path, std::slice::from_ref(&config))
            .await
            .expect("load");
        let settled = meter.reserve(Some("ak_test"), 1.0).expect("reserve");
        settled.settle(usage(1.0)).await.expect("settle");
        let inflight = meter.reserve(Some("ak_test"), 4.0).expect("inflight");
        assert!(meter.reset_usage("ak_test").await.expect("reset"));
        let view = meter.list().pop().expect("key");
        assert_eq!(view.usage.total_credits, 0.0);
        assert_eq!(view.reserved_credits, 4.0);
        drop(inflight);

        let reloaded = Meter::load(&path, &[config]).await.expect("reload");
        assert_eq!(reloaded.list()[0].usage.total_credits, 0.0);
    }

    fn copilot_usage(provider: &str, account: Option<&str>, amount: Option<u64>) -> UsageRecord {
        let mut record = usage(0.0);
        record.provider_id = provider.into();
        record.account_id = account.map(str::to_owned);
        record.kiro_model = None;
        record.provider_billing = Some(ProviderBilling {
            schema_version: 1,
            source: "copilot_usage".into(),
            unit: Some("nano_aiu".into()),
            amount,
            raw: serde_json::json!({"total_nano_aiu":amount}),
        });
        record
    }

    #[tokio::test]
    async fn account_aiu_survives_key_reset_removal_and_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("daily.json");
        let config = ApiKeyConfig {
            id: Some("ak_test".into()),
            name: "test".into(),
            key: "secret".into(),
            enabled: true,
            ..ApiKeyConfig::default()
        };
        let meter = Meter::load(&path, &[config]).await.unwrap();
        meter
            .reserve(Some("ak_test"), 0.0)
            .unwrap()
            .settle(copilot_usage("copilot", Some("acc_a"), Some(17_000_000)))
            .await
            .unwrap();
        assert!(meter.reset_usage("ak_test").await.unwrap());
        let inflight = meter.reserve(Some("ak_test"), 0.0).unwrap();
        meter.replace_configs(&[]);
        inflight
            .settle(copilot_usage("copilot", Some("acc_a"), Some(76_200_000)))
            .await
            .unwrap();
        meter
            .reserve(None, 0.0)
            .unwrap()
            .settle(copilot_usage("copilot", Some("acc_a"), Some(0)))
            .await
            .unwrap();
        meter
            .reserve(None, 0.0)
            .unwrap()
            .settle(copilot_usage("copilot-other", Some("acc_a"), Some(9)))
            .await
            .unwrap();
        meter
            .reserve(None, 0.0)
            .unwrap()
            .settle(copilot_usage("copilot", Some("acc_b"), None))
            .await
            .unwrap();
        meter
            .reserve(None, 0.0)
            .unwrap()
            .settle(copilot_usage("copilot", None, Some(10)))
            .await
            .unwrap();
        meter
            .reserve(None, 0.0)
            .unwrap()
            .settle(usage(1.5))
            .await
            .unwrap();
        meter.persist().await.unwrap();
        let reloaded = Meter::load(&path, &[]).await.unwrap();
        let usage = reloaded.account_aiu_usage("copilot", "acc_a").unwrap();
        assert_eq!(usage.total_nano_aiu, 93_200_000);
        assert_eq!(usage.billed_requests, 3);
        assert!(usage.first_recorded_at <= usage.last_recorded_at);
        assert_eq!(
            reloaded
                .account_aiu_usage("copilot-other", "acc_a")
                .unwrap()
                .total_nano_aiu,
            9
        );
        assert!(reloaded.account_aiu_usage("copilot", "acc_b").is_none());
        assert_eq!(reloaded.daily_snapshot().1, 1.5);
        assert!(reloaded.list().is_empty());
    }

    #[tokio::test]
    async fn legacy_meter_does_not_guess_historical_account_aiu() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("daily.json");
        tokio::fs::write(
            &path,
            r#"{"api_keys":{},"service_daily":{"day":"2026-10-09","used":0}}"#,
        )
        .await
        .unwrap();
        let meter = Meter::load(&path, &[]).await.unwrap();
        assert!(meter.account_aiu_usage("copilot", "acc_a").is_none());
        let legacy_record: UsageRecord = serde_json::from_value({
            let mut value = serde_json::to_value(usage(0.0)).unwrap();
            value.as_object_mut().unwrap().remove("account_id");
            value
        })
        .unwrap();
        assert!(legacy_record.account_id.is_none());
    }

    #[test]
    fn utc_date_conversion_is_stable() {
        assert_eq!(utc_day(0), "1970-01-01");
        assert_eq!(utc_day(1_704_067_200), "2024-01-01");
    }
}
