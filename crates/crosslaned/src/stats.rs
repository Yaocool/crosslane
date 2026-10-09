//! Bounded request statistics and model cache.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crosslane_kiro::ModelInfo;
use serde::{Deserialize, Serialize};
use tracing::warn;

const MAX_DIMENSION_KEYS: usize = 1_024;
const MAX_BUCKET_DIMENSION_KEYS: usize = MAX_DIMENSION_KEYS;
const MAX_BUCKET_LATENCIES: usize = 256;
const OTHER_DIMENSION: &str = "other";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpstreamAttemptLog {
    pub attempt: u32,
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub account_name: String,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_models: Vec<String>,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default)]
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextOverflowDiagnostics {
    #[serde(flatten)]
    pub tokens: crosslane_translate::ContextTokenBreakdown,
    pub maximum_input_tokens: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestDiagnostics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_context: Option<crosslane_core::client_context::ClientContext>,
    #[serde(default)]
    pub original_tool_count: usize,
    #[serde(default)]
    pub loaded_tool_count: usize,
    #[serde(default)]
    pub deferred_tool_count: usize,
    #[serde(default)]
    pub loaded_tool_bytes: usize,
    #[serde(default)]
    pub catalog_bytes: usize,
    #[serde(default)]
    pub tool_tokens: u64,
    #[serde(default)]
    pub payload_bytes: usize,
    #[serde(default)]
    pub tool_search_rounds: usize,
    #[serde(default)]
    pub tool_search_matches: usize,
    #[serde(default)]
    pub search_requested_limit: usize,
    #[serde(default)]
    pub search_returned_count: usize,
    #[serde(default)]
    pub search_budget_truncated: bool,
    #[serde(default)]
    pub web_search_rounds: usize,
    #[serde(default)]
    pub web_search_results: usize,
    #[serde(default)]
    pub client_status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_status: Option<u16>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error_code: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error_stage: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub failure_scope: String,
    #[serde(default)]
    pub account_error: bool,
    /// "server" when Kiro reported credits, otherwise "estimated". Empty for
    /// legacy records and requests that never reached generation.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub credits_source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_overflow: Option<ContextOverflowDiagnostics>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestLog {
    pub timestamp: i64,
    #[serde(default)]
    pub trace_id: String,
    pub request_id: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user_agent: String,
    /// Provider instance that handled the request. Empty legacy records are
    /// interpreted as `kiro` when grouped.
    #[serde(default)]
    pub provider_id: String,
    /// Model after an explicit `model_mapping` rule or runtime fallback.
    pub model: String,
    /// Model name received from the client.
    pub original_model: String,
    /// Account-discovered Kiro model ID after automatic alias resolution.
    pub kiro_model: String,
    pub account_id: String,
    #[serde(default)]
    pub account_name: String,
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    /// Full route for diagnostics; `model_mapping_rule` is set only for an
    /// explicit operator-configured mapping.
    pub model_path: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_mapping_rule: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<UpstreamAttemptLog>,
    pub duration_ms: u64,
    pub status: u16,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub credits: f64,
    pub error: Option<String>,
    #[serde(default)]
    pub diagnostics: RequestDiagnostics,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Counter {
    pub requests: u64,
    pub successes: u64,
    pub failures: u64,
    pub credits: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Sum of request durations used for an exact average over the selected range.
    #[serde(default)]
    pub duration_ms: u64,
    /// Number of requests represented by `duration_ms` (zero for legacy state).
    #[serde(default)]
    pub duration_samples: u64,
}

impl Counter {
    fn record(&mut self, request: &RequestLog) {
        self.requests += 1;
        self.successes += u64::from(request.status < 400);
        self.failures += u64::from(request.status >= 400);
        self.credits += request.credits;
        self.input_tokens += request.input_tokens;
        self.output_tokens += request.output_tokens;
        self.duration_ms = self.duration_ms.saturating_add(request.duration_ms);
        self.duration_samples = self.duration_samples.saturating_add(1);
    }

    fn merge(&mut self, other: &Self) {
        self.requests += other.requests;
        self.successes += other.successes;
        self.failures += other.failures;
        self.credits += other.credits;
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.duration_ms = self.duration_ms.saturating_add(other.duration_ms);
        self.duration_samples = self.duration_samples.saturating_add(other.duration_samples);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WindowBucket {
    pub total: Counter,
    #[serde(default)]
    pub by_provider: HashMap<String, Counter>,
    /// Provider-local dimensions preserve exact filtering without relying on
    /// account/model naming conventions or mixing equal model IDs.
    #[serde(default)]
    pub provider_details: HashMap<String, ProviderStatsBreakdown>,
    pub by_account: HashMap<String, Counter>,
    pub by_endpoint: HashMap<String, Counter>,
    pub by_model: HashMap<String, Counter>,
    pub latencies_ms: VecDeque<u64>,
}

impl WindowBucket {
    fn migrate_legacy_kiro_dimensions(&mut self) {
        if self.total.requests == 0
            || !self.by_provider.is_empty()
            || !self.provider_details.is_empty()
        {
            return;
        }
        self.by_provider.insert("kiro".into(), self.total.clone());
        self.provider_details.insert(
            "kiro".into(),
            ProviderStatsBreakdown {
                total: self.total.clone(),
                by_account: self.by_account.clone(),
                by_endpoint: self.by_endpoint.clone(),
                by_model: self.by_model.clone(),
                latencies_ms: self.latencies_ms.clone(),
            },
        );
    }

    fn record(&mut self, request: &RequestLog) {
        self.total.record(request);
        provider_breakdown_entry(
            &mut self.provider_details,
            request_provider(request),
            MAX_BUCKET_DIMENSION_KEYS,
        )
        .record(request, MAX_BUCKET_DIMENSION_KEYS);
        record_dimension_with_limit(
            &mut self.by_provider,
            request_provider(request),
            request,
            MAX_BUCKET_DIMENSION_KEYS,
        );
        record_dimension_with_limit(
            &mut self.by_account,
            &request.account_id,
            request,
            MAX_BUCKET_DIMENSION_KEYS,
        );
        record_dimension_with_limit(
            &mut self.by_endpoint,
            &request.endpoint,
            request,
            MAX_BUCKET_DIMENSION_KEYS,
        );
        record_dimension_with_limit(
            &mut self.by_model,
            &request.model,
            request,
            MAX_BUCKET_DIMENSION_KEYS,
        );
        self.latencies_ms.push_back(request.duration_ms);
        while self.latencies_ms.len() > MAX_BUCKET_LATENCIES {
            self.latencies_ms.pop_front();
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderStatsBreakdown {
    pub total: Counter,
    #[serde(default)]
    pub by_account: HashMap<String, Counter>,
    #[serde(default)]
    pub by_endpoint: HashMap<String, Counter>,
    #[serde(default)]
    pub by_model: HashMap<String, Counter>,
    #[serde(default)]
    pub latencies_ms: VecDeque<u64>,
}

impl ProviderStatsBreakdown {
    fn record(&mut self, request: &RequestLog, maximum: usize) {
        self.total.record(request);
        record_dimension_with_limit(&mut self.by_account, &request.account_id, request, maximum);
        record_dimension_with_limit(&mut self.by_endpoint, &request.endpoint, request, maximum);
        record_dimension_with_limit(&mut self.by_model, &request.model, request, maximum);
        self.latencies_ms.push_back(request.duration_ms);
        while self.latencies_ms.len() > MAX_BUCKET_LATENCIES {
            self.latencies_ms.pop_front();
        }
    }

    fn merge(&mut self, other: &Self, maximum: usize, maximum_latencies: usize) {
        self.total.merge(&other.total);
        merge_dimensions_with_limit(&mut self.by_account, &other.by_account, maximum);
        merge_dimensions_with_limit(&mut self.by_endpoint, &other.by_endpoint, maximum);
        merge_dimensions_with_limit(&mut self.by_model, &other.by_model, maximum);
        self.latencies_ms.extend(other.latencies_ms.iter().copied());
        while self.latencies_ms.len() > maximum_latencies {
            self.latencies_ms.pop_front();
        }
    }

    fn bound(&mut self, maximum: usize, maximum_latencies: usize) {
        bound_dimensions_to(&mut self.by_account, maximum);
        bound_dimensions_to(&mut self.by_endpoint, maximum);
        bound_dimensions_to(&mut self.by_model, maximum);
        while self.latencies_ms.len() > maximum_latencies {
            self.latencies_ms.pop_front();
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyStats {
    pub total: Counter,
    #[serde(default)]
    pub by_provider: HashMap<String, Counter>,
    #[serde(default)]
    pub provider_details: HashMap<String, ProviderStatsBreakdown>,
    pub by_account: HashMap<String, Counter>,
    pub by_endpoint: HashMap<String, Counter>,
    pub by_model: HashMap<String, Counter>,
    pub recent_requests: VecDeque<RequestLog>,
    pub latencies_ms: VecDeque<u64>,
    #[serde(default)]
    pub minute_buckets: BTreeMap<i64, WindowBucket>,
    /// Earliest minute for which time-window aggregates are available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_started_at: Option<i64>,
    /// Whether history before `history_started_at` is known to be complete.
    /// Legacy files default to false because older versions evicted buckets.
    /// Known gaps after that point are tracked separately in `history_gaps`.
    #[serde(default)]
    pub history_complete: bool,
    /// Known holes in the retained time series, for example quarantined shards.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history_gaps: Vec<HistoryGap>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryGap {
    /// Inclusive Unix-second start of the unavailable interval.
    pub start: i64,
    /// Inclusive Unix-second end of the unavailable interval.
    pub end: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

impl Default for ProxyStats {
    fn default() -> Self {
        Self {
            total: Counter::default(),
            by_provider: HashMap::new(),
            provider_details: HashMap::new(),
            by_account: HashMap::new(),
            by_endpoint: HashMap::new(),
            by_model: HashMap::new(),
            recent_requests: VecDeque::new(),
            latencies_ms: VecDeque::new(),
            minute_buckets: BTreeMap::new(),
            history_started_at: None,
            history_complete: true,
            history_gaps: Vec::new(),
        }
    }
}

impl ProxyStats {
    fn migrate_legacy_kiro_dimensions(&mut self) {
        // Provider dimensions did not exist before the multi-provider schema,
        // so every non-empty aggregate without them is known to be Kiro data.
        if self.total.requests > 0
            && self.by_provider.is_empty()
            && self.provider_details.is_empty()
        {
            self.by_provider.insert("kiro".into(), self.total.clone());
            self.provider_details.insert(
                "kiro".into(),
                ProviderStatsBreakdown {
                    total: self.total.clone(),
                    by_account: self.by_account.clone(),
                    by_endpoint: self.by_endpoint.clone(),
                    by_model: self.by_model.clone(),
                    latencies_ms: self.latencies_ms.clone(),
                },
            );
        }
        for bucket in self.minute_buckets.values_mut() {
            bucket.migrate_legacy_kiro_dimensions();
        }
    }

    fn bound_dimensions(&mut self) {
        bound_dimensions(&mut self.by_account);
        bound_dimensions(&mut self.by_provider);
        bound_provider_breakdowns(&mut self.provider_details, MAX_DIMENSION_KEYS, 10_000);
        for details in self.provider_details.values_mut() {
            details.bound(MAX_DIMENSION_KEYS, 10_000);
        }
        bound_dimensions(&mut self.by_endpoint);
        bound_dimensions(&mut self.by_model);
        for bucket in self.minute_buckets.values_mut() {
            bound_dimensions_to(&mut bucket.by_provider, MAX_BUCKET_DIMENSION_KEYS);
            bound_provider_breakdowns(
                &mut bucket.provider_details,
                MAX_BUCKET_DIMENSION_KEYS,
                MAX_BUCKET_LATENCIES,
            );
            for details in bucket.provider_details.values_mut() {
                details.bound(MAX_BUCKET_DIMENSION_KEYS, MAX_BUCKET_LATENCIES);
            }
            bound_dimensions_to(&mut bucket.by_account, MAX_BUCKET_DIMENSION_KEYS);
            bound_dimensions_to(&mut bucket.by_endpoint, MAX_BUCKET_DIMENSION_KEYS);
            bound_dimensions_to(&mut bucket.by_model, MAX_BUCKET_DIMENSION_KEYS);
            while bucket.latencies_ms.len() > MAX_BUCKET_LATENCIES {
                bucket.latencies_ms.pop_front();
            }
        }
        if self.history_started_at.is_none() {
            self.history_started_at = self
                .minute_buckets
                .first_key_value()
                .map(|(minute, _)| minute.saturating_mul(60));
        }
    }

    pub fn percentiles(&self) -> (u64, u64, u64) {
        let mut values = self.latencies_ms.iter().copied().collect::<Vec<_>>();
        values.sort_unstable();
        (
            percentile(&values, 0.50),
            percentile(&values, 0.95),
            percentile(&values, 0.99),
        )
    }

    /// Restrict a materialized statistics window to one provider. New
    /// snapshots include exact provider-local dimensions. Legacy snapshots
    /// retain the provider total and report empty local dimensions rather than
    /// returning unrelated global values.
    pub fn retain_provider(&mut self, provider: &str) {
        let provider_total = self.by_provider.get(provider).cloned().unwrap_or_default();
        let details = self.provider_details.get(provider).cloned();
        self.total = details
            .as_ref()
            .map_or(provider_total, |details| details.total.clone());
        self.by_provider
            .retain(|candidate, _| candidate == provider);
        self.provider_details
            .retain(|candidate, _| candidate == provider);
        if let Some(details) = details {
            self.by_account = details.by_account;
            self.by_endpoint = details.by_endpoint;
            self.by_model = details.by_model;
            self.latencies_ms = details.latencies_ms;
        } else {
            self.by_account.clear();
            self.by_endpoint.clear();
            self.by_model.clear();
            self.latencies_ms.clear();
        }
        self.recent_requests
            .retain(|request| request_provider(request) == provider);
    }
}

pub struct StatsStore {
    path: PathBuf,
    history_dir: PathBuf,
    state: Mutex<StatsRuntimeState>,
    persist_lock: tokio::sync::Mutex<()>,
    session_started_at: i64,
    sender: tokio::sync::broadcast::Sender<RequestLog>,
}

#[derive(Default)]
struct StatsRuntimeState {
    persistent: ProxyStats,
    recent_requests: VecDeque<Arc<RequestLog>>,
    /// Absolute minute buckets for the current UTC hour and any checkpoint
    /// minutes that have not yet been migrated to hourly history files.
    active_minutes: BTreeMap<i64, Arc<WindowBucket>>,
    /// Minutes whose absolute buckets have not completed the checkpoint ->
    /// history-shard -> compact-checkpoint transaction.
    dirty_minutes: BTreeSet<i64>,
    session: SessionStats,
}

#[derive(Default)]
struct SessionStats {
    total: Counter,
    by_provider: HashMap<String, Counter>,
    minute_buckets: BTreeMap<i64, SessionBucket>,
}

#[derive(Default)]
struct SessionBucket {
    total: Counter,
    by_provider: HashMap<String, Counter>,
}

impl SessionStats {
    fn record(&mut self, request: &RequestLog) {
        self.total.record(request);
        record_dimension(&mut self.by_provider, request_provider(request), request);
        let bucket = self
            .minute_buckets
            .entry(request.timestamp.div_euclid(60))
            .or_default();
        bucket.total.record(request);
        record_dimension(&mut bucket.by_provider, request_provider(request), request);
    }
}

#[derive(Default, Serialize, Deserialize)]
struct HistoryDay {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    minutes: BTreeMap<i64, WindowBucket>,
}

const HISTORY_VERSION: u32 = 1;

impl StatsStore {
    pub async fn load(path: &Path) -> anyhow::Result<Self> {
        let mut persistent = match tokio::fs::read_to_string(path).await {
            // Bootstrap and older releases used `{}` as the canonical empty
            // statistics document. Keep that representation compatible while
            // still rejecting partially corrupt/non-conforming objects.
            Ok(raw) if matches!(raw.trim(), "" | "{}") => ProxyStats::default(),
            Ok(raw) => serde_json::from_str(&raw)
                .map_err(|error| anyhow::anyhow!("parse {}: {error}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => ProxyStats::default(),
            Err(error) => return Err(error.into()),
        };
        persistent.migrate_legacy_kiro_dimensions();
        persistent.bound_dimensions();
        let recent_requests = std::mem::take(&mut persistent.recent_requests)
            .into_iter()
            .map(Arc::new)
            .collect::<VecDeque<_>>();
        let history_dir = history_dir_for(path);
        let legacy_minutes = std::mem::take(&mut persistent.minute_buckets);
        let now = crate::now_secs();
        let current_minute = now.div_euclid(60);
        let current_day = utc_day_from_minute(current_minute);
        let current_day_start = now.div_euclid(86_400).saturating_mul(86_400);
        let current_day_path = history_dir.join(format!("{current_day}.json"));
        let (current_history, daily_gap) = read_current_history_shard(
            &current_day_path,
            current_day_start,
            current_day_start.saturating_add(86_399),
        )
        .await?;
        if let Some(gap) = daily_gap {
            insert_history_gap(&mut persistent, gap);
        }
        let current_hour = utc_hour_from_minute(current_minute);
        let current_hour_start = now.div_euclid(3_600).saturating_mul(3_600);
        let current_hour_path = history_dir.join(format!("{current_hour}.json"));
        let (hour_history, hour_gap) = read_current_history_shard(
            &current_hour_path,
            current_hour_start,
            current_hour_start.saturating_add(3_599),
        )
        .await?;
        if let Some(gap) = hour_gap {
            insert_history_gap(&mut persistent, gap);
        }
        let current_hour_minute = current_minute.div_euclid(60);
        let mut active_minutes = current_history
            .minutes
            .into_iter()
            .filter(|(minute, _)| minute.div_euclid(60) == current_hour_minute)
            .map(|(minute, bucket)| (minute, Arc::new(bucket)))
            .collect::<BTreeMap<_, _>>();
        active_minutes.extend(
            hour_history
                .minutes
                .into_iter()
                .map(|(minute, bucket)| (minute, Arc::new(bucket))),
        );
        // A legacy stats.json is authoritative for the minutes it contains.
        // Replacing instead of merging makes migration idempotent after a
        // partially completed persistence cycle.
        let dirty_minutes = legacy_minutes.keys().copied().collect();
        active_minutes.extend(
            legacy_minutes
                .into_iter()
                .map(|(minute, bucket)| (minute, Arc::new(bucket))),
        );
        let (sender, _) = tokio::sync::broadcast::channel(1_024);
        Ok(Self {
            path: path.into(),
            history_dir,
            state: Mutex::new(StatsRuntimeState {
                persistent,
                recent_requests,
                active_minutes,
                dirty_minutes,
                session: SessionStats::default(),
            }),
            persist_lock: tokio::sync::Mutex::new(()),
            session_started_at: crate::now_secs(),
            sender,
        })
    }

    pub fn empty(path: &Path) -> Self {
        let (sender, _) = tokio::sync::broadcast::channel(1_024);
        Self {
            path: path.into(),
            history_dir: history_dir_for(path),
            state: Mutex::new(StatsRuntimeState::default()),
            persist_lock: tokio::sync::Mutex::new(()),
            session_started_at: crate::now_secs(),
            sender,
        }
    }

    pub fn record(&self, mut request: RequestLog) {
        use crosslane_translate::error::{log_model, sanitize_error_message};
        request.model = log_model(&request.model);
        request.original_model = log_model(&request.original_model);
        request.kiro_model = log_model(&request.kiro_model);
        for model in &mut request.model_path {
            *model = log_model(model);
        }
        for attempt in &mut request.attempts {
            attempt.model = log_model(&attempt.model);
            for model in &mut attempt.available_models {
                *model = log_model(model);
            }
            attempt.error = sanitize_error_message(&attempt.error);
        }
        request.error = request.error.as_deref().map(sanitize_error_message);
        let notification = request.clone();
        let request = Arc::new(request);
        let mut state = lock(&self.state);
        record_persistent(&mut state.persistent, &request);
        let minute = request.timestamp.div_euclid(60);
        Arc::make_mut(
            state
                .active_minutes
                .entry(minute)
                .or_insert_with(|| Arc::new(WindowBucket::default())),
        )
        .record(&request);
        state.dirty_minutes.insert(minute);
        state.session.record(&request);
        state.recent_requests.push_back(Arc::clone(&request));
        while state.recent_requests.len() > 1_000 {
            state.recent_requests.pop_front();
        }
        drop(state);
        let _result = self.sender.send(notification);
    }

    pub fn session_started_at(&self) -> i64 {
        self.session_started_at
    }

    pub fn snapshot(&self, recent: Option<usize>) -> ProxyStats {
        let (mut output, active_minutes, recent_requests) = {
            let state = lock(&self.state);
            (
                state.persistent.clone(),
                state.active_minutes.clone(),
                recent_refs(&state.recent_requests, recent),
            )
        };
        output.minute_buckets = active_minutes
            .into_iter()
            .map(|(minute, bucket)| (minute, (*bucket).clone()))
            .collect();
        output.recent_requests = materialize_recent(&recent_requests);
        output
    }

    /// A compact persisted snapshot that excludes the retained time-series buckets.
    pub fn summary_snapshot(&self, recent: Option<usize>) -> ProxyStats {
        let (mut output, recent_requests) = {
            let state = lock(&self.state);
            (
                compact_snapshot(&state.persistent),
                recent_refs(&state.recent_requests, recent),
            )
        };
        output.recent_requests = materialize_recent(&recent_requests);
        output
    }

    /// Returns cumulative latency percentiles without cloning dimension maps
    /// or the recent request ring during the periodic persistence task.
    pub fn latency_percentiles(&self) -> (u64, u64, u64) {
        let mut values = {
            let state = lock(&self.state);
            state
                .persistent
                .latencies_ms
                .iter()
                .copied()
                .collect::<Vec<_>>()
        };
        values.sort_unstable();
        (
            percentile(&values, 0.50),
            percentile(&values, 0.95),
            percentile(&values, 0.99),
        )
    }

    #[cfg(test)]
    pub async fn window(
        &self,
        since: Option<i64>,
        recent: Option<usize>,
    ) -> anyhow::Result<ProxyStats> {
        self.window_between(since, None, recent).await
    }

    /// Returns persisted statistics in the inclusive Unix-second range.
    ///
    /// Aggregates use minute buckets, while recent request details are filtered
    /// at their exact timestamps.
    pub async fn window_between(
        &self,
        start: Option<i64>,
        end: Option<i64>,
        recent: Option<usize>,
    ) -> anyhow::Result<ProxyStats> {
        if start.is_none() && end.is_none() {
            return Ok(self.summary_snapshot(recent));
        }

        let (active_minutes, recent_requests, history_started_at, history_complete, history_gaps) = {
            let state = lock(&self.state);
            (
                active_buckets_between(&state.active_minutes, start, end),
                recent_between(&state.recent_requests, start, end, recent),
                state.persistent.history_started_at,
                state.persistent.history_complete && state.persistent.history_gaps.is_empty(),
                state.persistent.history_gaps.clone(),
            )
        };
        let active_keys = active_minutes.keys().copied().collect::<BTreeSet<_>>();
        let mut output = read_history_between(
            &self.history_dir,
            start,
            end,
            &active_keys,
            history_started_at,
            history_complete,
            history_gaps,
        )
        .await?;
        if !output.history_gaps.is_empty() {
            let mut state = lock(&self.state);
            for gap in output.history_gaps.iter().cloned() {
                insert_history_gap(&mut state.persistent, gap);
            }
        }
        tokio::task::spawn_blocking(move || {
            for bucket in active_minutes.values() {
                merge_bucket(&mut output, bucket);
            }
            output.recent_requests = materialize_recent(&recent_requests);
            output
        })
        .await
        .map_err(|error| anyhow::anyhow!("join statistics aggregator: {error}"))
    }

    /// Returns statistics recorded by the current daemon process only.
    pub fn session_window(
        &self,
        start: Option<i64>,
        end: Option<i64>,
        _recent: Option<usize>,
    ) -> ProxyStats {
        let state = lock(&self.state);
        if start.is_none() && end.is_none() {
            return ProxyStats {
                total: state.session.total.clone(),
                by_provider: state.session.by_provider.clone(),
                history_started_at: Some(self.session_started_at),
                ..ProxyStats::default()
            };
        }
        let mut total = Counter::default();
        let mut by_provider = HashMap::new();
        if !start.zip(end).is_some_and(|(start, end)| start > end) {
            let start_minute = start.unwrap_or(i64::MIN).div_euclid(60);
            let end_minute = end.unwrap_or(i64::MAX).div_euclid(60);
            for bucket in state
                .session
                .minute_buckets
                .range(start_minute..=end_minute)
                .map(|(_, bucket)| bucket)
            {
                total.merge(&bucket.total);
                merge_dimensions(&mut by_provider, &bucket.by_provider);
            }
        }
        ProxyStats {
            total,
            by_provider,
            history_started_at: Some(self.session_started_at),
            ..ProxyStats::default()
        }
    }

    pub fn persistent_history_started_at(&self) -> Option<i64> {
        lock(&self.state).persistent.history_started_at
    }

    #[cfg(test)]
    pub fn persistent_history_complete(&self) -> bool {
        let state = lock(&self.state);
        state.persistent.history_complete && state.persistent.history_gaps.is_empty()
    }

    pub fn persistent_history_prefix_complete(&self) -> bool {
        lock(&self.state).persistent.history_complete
    }

    #[cfg(test)]
    pub fn persistent_history_gaps(&self) -> Vec<HistoryGap> {
        lock(&self.state).persistent.history_gaps.clone()
    }

    #[cfg(test)]
    pub fn session_snapshot(&self) -> ProxyStats {
        let state = lock(&self.state);
        ProxyStats {
            total: state.session.total.clone(),
            by_provider: state.session.by_provider.clone(),
            history_started_at: Some(self.session_started_at),
            ..ProxyStats::default()
        }
    }

    pub async fn persist(&self) -> anyhow::Result<()> {
        // Manual task runs and the periodic worker may overlap. Serialize the
        // complete read-modify-write transaction so an older snapshot can
        // never overwrite a newer hourly shard or cumulative summary.
        let _persist_guard = self.persist_lock.lock().await;
        let (mut snapshot, recent_requests, dirty_minutes) = {
            let state = lock(&self.state);
            (
                compact_snapshot(&state.persistent),
                state.recent_requests.clone(),
                state
                    .dirty_minutes
                    .iter()
                    .filter_map(|minute| {
                        state
                            .active_minutes
                            .get(minute)
                            .map(|bucket| (*minute, Arc::clone(bucket)))
                    })
                    .collect::<BTreeMap<_, _>>(),
            )
        };
        // Deep-cloning recent diagnostic strings happens after releasing the
        // request accounting lock.
        snapshot.recent_requests = materialize_recent(&recent_requests);
        if dirty_minutes.is_empty() {
            write_json_off_thread(&self.path, snapshot).await?;
        } else {
            // stats.json first becomes a self-contained recovery checkpoint.
            // If any later write or the process fails, startup reloads these
            // absolute dirty buckets and idempotently finishes the transaction.
            let (checkpoint, compact) = serialize_stats_documents(snapshot, &dirty_minutes).await?;
            crosslane_store::atomic::write_bytes_atomically(&self.path, &checkpoint, Some(0o600))
                .await?;
            persist_history(&self.history_dir, &dirty_minutes).await?;
            crosslane_store::atomic::write_bytes_atomically(&self.path, &compact, Some(0o600))
                .await?;
        }

        // A bucket changed during I/O remains dirty. Clean buckets outside the
        // current hour can be released because their shard is now durable.
        let current_hour = crate::now_secs().div_euclid(3_600);
        let mut state = lock(&self.state);
        for (minute, persisted) in &dirty_minutes {
            if state
                .active_minutes
                .get(minute)
                .is_some_and(|current| current.as_ref() == persisted.as_ref())
            {
                state.dirty_minutes.remove(minute);
            }
        }
        let dirty = state.dirty_minutes.clone();
        state.active_minutes.retain(|minute, _current| {
            minute.div_euclid(60) >= current_hour || dirty.contains(minute)
        });
        Ok(())
    }

    pub async fn follow(
        &self,
        after_request_id: Option<&str>,
        tail: usize,
        wait_ms: u64,
        level: Option<&str>,
        account: Option<&str>,
        provider: Option<&str>,
    ) -> Vec<RequestLog> {
        // Subscribe before inspecting the ring to avoid missing a record between
        // the initial read and registration with the broadcast channel.
        let mut receiver = self.sender.subscribe();
        let current = self.filtered_logs(after_request_id, tail, level, account, provider);
        if !current.is_empty() || wait_ms == 0 {
            return current;
        }
        let deadline = std::time::Duration::from_millis(wait_ms.min(60_000));
        let _result = tokio::time::timeout(deadline, async {
            loop {
                match receiver.recv().await {
                    Ok(request) if matches_log(&request, level, account, provider) => break,
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
        .await;
        self.filtered_logs(after_request_id, tail, level, account, provider)
    }

    fn filtered_logs(
        &self,
        after_request_id: Option<&str>,
        tail: usize,
        level: Option<&str>,
        account: Option<&str>,
        provider: Option<&str>,
    ) -> Vec<RequestLog> {
        let entries = {
            let state = lock(&self.state);
            let start = after_request_id.and_then(|id| {
                state
                    .recent_requests
                    .iter()
                    .position(|request| request.request_id == id)
                    .map(|index| index + 1)
            });
            let tail = tail.min(1_000);
            if let Some(start) = start {
                state
                    .recent_requests
                    .iter()
                    .skip(start)
                    .filter(|request| matches_log(request, level, account, provider))
                    .take(tail)
                    .cloned()
                    .collect::<Vec<_>>()
            } else {
                let mut entries = state
                    .recent_requests
                    .iter()
                    .rev()
                    .filter(|request| matches_log(request, level, account, provider))
                    .take(tail)
                    .cloned()
                    .collect::<Vec<_>>();
                entries.reverse();
                entries
            }
        };
        entries
            .into_iter()
            .map(|request| (*request).clone())
            .collect()
    }
}

fn record_persistent(state: &mut ProxyStats, request: &RequestLog) {
    state.total.record(request);
    record_dimension(&mut state.by_provider, request_provider(request), request);
    provider_breakdown_entry(
        &mut state.provider_details,
        request_provider(request),
        MAX_DIMENSION_KEYS,
    )
    .record(request, MAX_DIMENSION_KEYS);
    record_dimension(&mut state.by_account, &request.account_id, request);
    record_dimension(&mut state.by_endpoint, &request.endpoint, request);
    record_dimension(&mut state.by_model, &request.model, request);
    state.latencies_ms.push_back(request.duration_ms);
    let minute = request.timestamp.div_euclid(60);
    state.history_started_at = Some(state.history_started_at.map_or_else(
        || minute.saturating_mul(60),
        |current| current.min(minute.saturating_mul(60)),
    ));
    while state.latencies_ms.len() > 10_000 {
        state.latencies_ms.pop_front();
    }
}

fn active_buckets_between(
    buckets: &BTreeMap<i64, Arc<WindowBucket>>,
    start: Option<i64>,
    end: Option<i64>,
) -> BTreeMap<i64, Arc<WindowBucket>> {
    if start.zip(end).is_some_and(|(start, end)| start > end) {
        return BTreeMap::new();
    }
    let start_minute = start.unwrap_or(i64::MIN).div_euclid(60);
    let end_minute = end.unwrap_or(i64::MAX).div_euclid(60);
    buckets
        .range(start_minute..=end_minute)
        .map(|(minute, bucket)| (*minute, Arc::clone(bucket)))
        .collect()
}

fn recent_between(
    requests: &VecDeque<Arc<RequestLog>>,
    start: Option<i64>,
    end: Option<i64>,
    recent: Option<usize>,
) -> VecDeque<Arc<RequestLog>> {
    let mut output: VecDeque<Arc<RequestLog>> = requests
        .iter()
        .filter(|request| {
            start.is_none_or(|start| request.timestamp >= start)
                && end.is_none_or(|end| request.timestamp <= end)
        })
        .cloned()
        .collect();
    if let Some(maximum) = recent {
        while output.len() > maximum {
            output.pop_front();
        }
    }
    output
}

fn merge_bucket(output: &mut ProxyStats, bucket: &WindowBucket) {
    output.total.merge(&bucket.total);
    merge_dimensions(&mut output.by_provider, &bucket.by_provider);
    merge_provider_breakdowns(
        &mut output.provider_details,
        &bucket.provider_details,
        MAX_DIMENSION_KEYS,
        10_000,
    );
    merge_dimensions(&mut output.by_account, &bucket.by_account);
    merge_dimensions(&mut output.by_endpoint, &bucket.by_endpoint);
    merge_dimensions(&mut output.by_model, &bucket.by_model);
    output
        .latencies_ms
        .extend(bucket.latencies_ms.iter().copied());
    while output.latencies_ms.len() > 10_000 {
        output.latencies_ms.pop_front();
    }
}

fn history_dir_for(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or("stats");
    path.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{stem}-history"))
}

async fn read_history_day(path: &Path) -> anyhow::Result<HistoryDay> {
    let mut history = match tokio::fs::read_to_string(path).await {
        Ok(raw) if matches!(raw.trim(), "" | "{}") => Ok(HistoryDay::default()),
        Ok(raw) => {
            let path = path.to_path_buf();
            tokio::task::spawn_blocking(move || {
                serde_json::from_str(&raw)
                    .map_err(|error| anyhow::anyhow!("parse {}: {error}", path.display()))
            })
            .await
            .map_err(|error| anyhow::anyhow!("join history parser: {error}"))?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(HistoryDay::default()),
        Err(error) => Err(error.into()),
    }?;
    if history.version > HISTORY_VERSION {
        anyhow::bail!(
            "unsupported statistics history version {} in {}",
            history.version,
            path.display()
        );
    }
    for bucket in history.minutes.values_mut() {
        bucket.migrate_legacy_kiro_dimensions();
    }
    Ok(history)
}

async fn read_current_history_shard(
    path: &Path,
    start: i64,
    end: i64,
) -> anyhow::Result<(HistoryDay, Option<HistoryGap>)> {
    match read_history_day(path).await {
        Ok(history) => Ok((history, None)),
        Err(error) => {
            let quarantine = quarantine_history_file(path).await.map_err(|quarantine_error| {
                anyhow::anyhow!(
                    "read current statistics history: {error}; quarantine failed: {quarantine_error}"
                )
            })?;
            warn!(
                error = %error,
                path = %path.display(),
                quarantine = %quarantine.display(),
                "quarantined unreadable current statistics history shard"
            );
            Ok((
                HistoryDay::default(),
                Some(HistoryGap {
                    start,
                    end,
                    reason: "unreadable history shard was quarantined".into(),
                }),
            ))
        }
    }
}

async fn quarantine_history_file(path: &Path) -> anyhow::Result<PathBuf> {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("history.json");
    let quarantine = path.with_file_name(format!("{file_name}.corrupt-{unique}"));
    tokio::fs::rename(path, &quarantine).await?;
    Ok(quarantine)
}

async fn read_history_between(
    history_dir: &Path,
    start: Option<i64>,
    end: Option<i64>,
    active_minutes: &BTreeSet<i64>,
    history_started_at: Option<i64>,
    history_complete: bool,
    history_gaps: Vec<HistoryGap>,
) -> anyhow::Result<ProxyStats> {
    let mut output = ProxyStats {
        history_started_at,
        history_complete,
        history_gaps,
        ..ProxyStats::default()
    };
    if start.zip(end).is_some_and(|(start, end)| start > end) {
        return Ok(output);
    }
    let mut entries = match tokio::fs::read_dir(history_dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(output);
        }
        Err(error) => return Err(error.into()),
    };
    let requested_start = start.unwrap_or(i64::MIN);
    let requested_end = end.unwrap_or(i64::MAX);
    let start_minute = start.unwrap_or(i64::MIN).div_euclid(60);
    let end_minute = end.unwrap_or(i64::MAX).div_euclid(60);
    let mut shards: BTreeMap<String, Vec<(u8, String, PathBuf)>> = BTreeMap::new();
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let Some((day, priority, shard_name)) = name
            .to_str()
            .and_then(|name| name.strip_suffix(".json"))
            .and_then(history_shard_descriptor)
        else {
            continue;
        };
        let Some((shard_start, shard_end)) = history_range_from_shard_name(&shard_name) else {
            continue;
        };
        if shard_end < requested_start || shard_start > requested_end {
            continue;
        }
        shards
            .entry(day)
            .or_default()
            .push((priority, shard_name, entry.path()));
    }

    let active_minutes = Arc::new(active_minutes.clone());
    for day_shards in shards.values_mut() {
        // Legacy daily files are the base; newer hourly shards replace any
        // overlapping minute with a more recent absolute bucket.
        day_shards.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
        let mut day_minutes = BTreeMap::new();
        for (_, _, path) in day_shards.drain(..) {
            let history = match read_history_day(&path).await {
                Ok(history) => history,
                Err(error) => {
                    let Some((start, end)) = path
                        .file_stem()
                        .and_then(|name| name.to_str())
                        .and_then(history_range_from_shard_name)
                    else {
                        return Err(error);
                    };
                    let quarantine = quarantine_history_file(&path).await.map_err(
                        |quarantine_error| {
                            anyhow::anyhow!(
                                "read statistics history: {error}; quarantine failed: {quarantine_error}"
                            )
                        },
                    )?;
                    warn!(
                        error = %error,
                        path = %path.display(),
                        quarantine = %quarantine.display(),
                        "quarantined unreadable statistics history shard"
                    );
                    output.history_complete = false;
                    insert_history_gap(
                        &mut output,
                        HistoryGap {
                            start,
                            end,
                            reason: "unreadable history shard was quarantined".into(),
                        },
                    );
                    continue;
                }
            };
            day_minutes.extend(
                history
                    .minutes
                    .into_iter()
                    .filter(|(minute, _)| *minute >= start_minute && *minute <= end_minute),
            );
        }
        let active_minutes = Arc::clone(&active_minutes);
        output = tokio::task::spawn_blocking(move || {
            let mut output = output;
            for (minute, bucket) in day_minutes {
                if !active_minutes.contains(&minute) {
                    merge_bucket(&mut output, &bucket);
                }
            }
            output
        })
        .await
        .map_err(|error| anyhow::anyhow!("join statistics shard aggregator: {error}"))?;
    }
    Ok(output)
}

fn history_shard_descriptor(stem: &str) -> Option<(String, u8, String)> {
    let bytes = stem.as_bytes();
    let valid_day = bytes.len() >= 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit);
    if !valid_day {
        return None;
    }
    let day = stem[..10].to_owned();
    if bytes.len() == 10 {
        return Some((day, 0, stem.to_owned()));
    }
    if bytes.len() == 13
        && bytes[10] == b'-'
        && bytes[11..13].iter().all(u8::is_ascii_digit)
        && stem[11..13]
            .parse::<u8>()
            .ok()
            .is_some_and(|hour| hour < 24)
    {
        return Some((day, 1, stem.to_owned()));
    }
    None
}

fn history_range_from_shard_name(stem: &str) -> Option<(i64, i64)> {
    let (day, priority, _) = history_shard_descriptor(stem)?;
    let year = day[..4].parse::<i64>().ok()?;
    let month = day[5..7].parse::<i64>().ok()?;
    let date = day[8..10].parse::<i64>().ok()?;
    let days = days_from_civil(year, month, date);
    if civil_from_days(days) != (year, month, date) {
        return None;
    }
    let day_start = days.saturating_mul(86_400);
    if priority == 0 {
        return Some((day_start, day_start.saturating_add(86_399)));
    }
    let hour = stem[11..13].parse::<i64>().ok()?;
    let start = day_start.saturating_add(hour.saturating_mul(3_600));
    Some((start, start.saturating_add(3_599)))
}

async fn persist_history(
    history_dir: &Path,
    minutes: &BTreeMap<i64, Arc<WindowBucket>>,
) -> anyhow::Result<()> {
    if minutes.is_empty() {
        return Ok(());
    }
    let mut hours: BTreeMap<String, BTreeMap<i64, Arc<WindowBucket>>> = BTreeMap::new();
    for (minute, bucket) in minutes {
        hours
            .entry(utc_hour_from_minute(*minute))
            .or_default()
            .insert(*minute, Arc::clone(bucket));
    }
    tokio::fs::create_dir_all(history_dir).await?;
    for (hour, hour_minutes) in hours {
        let path = history_dir.join(format!("{hour}.json"));
        let history = read_history_day(&path).await?;
        let display = path.display().to_string();
        let raw = tokio::task::spawn_blocking(move || {
            let mut history = history;
            history.version = HISTORY_VERSION;
            history.minutes.extend(
                hour_minutes
                    .into_iter()
                    .map(|(minute, bucket)| (minute, (*bucket).clone())),
            );
            serde_json::to_vec_pretty(&history)
                .map_err(|error| anyhow::anyhow!("serialize json for {display}: {error}"))
        })
        .await
        .map_err(|error| anyhow::anyhow!("join statistics serializer: {error}"))??;
        crosslane_store::atomic::write_bytes_atomically(&path, &raw, Some(0o600)).await?;
    }
    Ok(())
}

async fn serialize_stats_documents(
    mut snapshot: ProxyStats,
    dirty_minutes: &BTreeMap<i64, Arc<WindowBucket>>,
) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let dirty_minutes = dirty_minutes.clone();
    tokio::task::spawn_blocking(move || {
        let compact = serde_json::to_vec_pretty(&snapshot)
            .map_err(|error| anyhow::anyhow!("serialize compact statistics: {error}"))?;
        snapshot.minute_buckets = dirty_minutes
            .into_iter()
            .map(|(minute, bucket)| (minute, (*bucket).clone()))
            .collect();
        let checkpoint = serde_json::to_vec_pretty(&snapshot)
            .map_err(|error| anyhow::anyhow!("serialize statistics checkpoint: {error}"))?;
        Ok((checkpoint, compact))
    })
    .await
    .map_err(|error| anyhow::anyhow!("join statistics checkpoint serializer: {error}"))?
}

async fn write_json_off_thread<T>(path: &Path, value: T) -> anyhow::Result<()>
where
    T: Serialize + Send + 'static,
{
    let display = path.display().to_string();
    let raw = tokio::task::spawn_blocking(move || {
        serde_json::to_vec_pretty(&value)
            .map_err(|error| anyhow::anyhow!("serialize json for {display}: {error}"))
    })
    .await
    .map_err(|error| anyhow::anyhow!("join statistics serializer: {error}"))??;
    crosslane_store::atomic::write_bytes_atomically(path, &raw, Some(0o600)).await
}

fn utc_day_from_minute(minute: i64) -> String {
    let (year, month, day) = civil_from_days(minute.div_euclid(1_440));
    format!("{year:04}-{month:02}-{day:02}")
}

fn utc_hour_from_minute(minute: i64) -> String {
    let day = utc_day_from_minute(minute);
    let hour = minute.rem_euclid(1_440).div_euclid(60);
    format!("{day}-{hour:02}")
}

fn insert_history_gap(stats: &mut ProxyStats, gap: HistoryGap) {
    stats.history_gaps.push(gap);
    stats.history_gaps.sort_by_key(|gap| gap.start);
    let mut merged: Vec<HistoryGap> = Vec::with_capacity(stats.history_gaps.len());
    for gap in std::mem::take(&mut stats.history_gaps) {
        if let Some(previous) = merged.last_mut() {
            if gap.start <= previous.end.saturating_add(1) {
                previous.end = previous.end.max(gap.end);
                if previous.reason != gap.reason {
                    previous.reason = "one or more history shards are unavailable".into();
                }
                continue;
            }
        }
        merged.push(gap);
    }
    stats.history_gaps = merged;
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let days = days_since_epoch + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
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

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn compact_snapshot(state: &ProxyStats) -> ProxyStats {
    ProxyStats {
        total: state.total.clone(),
        by_provider: state.by_provider.clone(),
        provider_details: state.provider_details.clone(),
        by_account: state.by_account.clone(),
        by_endpoint: state.by_endpoint.clone(),
        by_model: state.by_model.clone(),
        recent_requests: VecDeque::new(),
        latencies_ms: state.latencies_ms.clone(),
        minute_buckets: BTreeMap::new(),
        history_started_at: state.history_started_at,
        history_complete: state.history_complete,
        history_gaps: state.history_gaps.clone(),
    }
}

fn recent_refs(
    requests: &VecDeque<Arc<RequestLog>>,
    recent: Option<usize>,
) -> VecDeque<Arc<RequestLog>> {
    let skip = recent.map_or(0, |maximum| requests.len().saturating_sub(maximum));
    requests.iter().skip(skip).cloned().collect()
}

fn materialize_recent(requests: &VecDeque<Arc<RequestLog>>) -> VecDeque<RequestLog> {
    requests.iter().map(|request| (**request).clone()).collect()
}

fn request_provider(request: &RequestLog) -> &str {
    if request.provider_id.is_empty() {
        "kiro"
    } else {
        &request.provider_id
    }
}

fn matches_log(
    request: &RequestLog,
    level: Option<&str>,
    account: Option<&str>,
    provider: Option<&str>,
) -> bool {
    if account.is_some_and(|account| request.account_id != account) {
        return false;
    }
    if provider.is_some_and(|provider| request_provider(request) != provider) {
        return false;
    }
    match level.map(str::to_ascii_lowercase).as_deref() {
        Some("error") => request.status >= 500,
        Some("warn" | "warning") => request.status >= 400,
        _ => true,
    }
}

fn merge_dimensions(target: &mut HashMap<String, Counter>, source: &HashMap<String, Counter>) {
    for (key, counter) in source {
        merge_dimension(target, key, counter);
    }
}

fn merge_dimensions_with_limit(
    target: &mut HashMap<String, Counter>,
    source: &HashMap<String, Counter>,
    maximum: usize,
) {
    for (key, counter) in source {
        dimension_entry_with_limit(target, key, maximum).merge(counter);
    }
}

fn provider_breakdown_entry<'a>(
    dimensions: &'a mut HashMap<String, ProviderStatsBreakdown>,
    key: &str,
    maximum: usize,
) -> &'a mut ProviderStatsBreakdown {
    let maximum = maximum.max(2);
    let bounded_key = if dimensions.contains_key(key)
        || (key != OTHER_DIMENSION && dimensions.len() < maximum - 1)
    {
        key
    } else {
        OTHER_DIMENSION
    };
    dimensions.entry(bounded_key.to_owned()).or_default()
}

fn merge_provider_breakdowns(
    target: &mut HashMap<String, ProviderStatsBreakdown>,
    source: &HashMap<String, ProviderStatsBreakdown>,
    maximum: usize,
    maximum_latencies: usize,
) {
    for (key, details) in source {
        provider_breakdown_entry(target, key, maximum).merge(details, maximum, maximum_latencies);
    }
}

fn bound_provider_breakdowns(
    dimensions: &mut HashMap<String, ProviderStatsBreakdown>,
    maximum: usize,
    maximum_latencies: usize,
) {
    if dimensions.len() < maximum {
        return;
    }
    let entries = std::mem::take(dimensions);
    for (key, details) in entries {
        provider_breakdown_entry(dimensions, &key, maximum).merge(
            &details,
            maximum,
            maximum_latencies,
        );
    }
}

fn record_dimension(dimensions: &mut HashMap<String, Counter>, key: &str, request: &RequestLog) {
    record_dimension_with_limit(dimensions, key, request, MAX_DIMENSION_KEYS);
}

fn record_dimension_with_limit(
    dimensions: &mut HashMap<String, Counter>,
    key: &str,
    request: &RequestLog,
    maximum: usize,
) {
    dimension_entry_with_limit(dimensions, key, maximum).record(request);
}

fn merge_dimension(dimensions: &mut HashMap<String, Counter>, key: &str, counter: &Counter) {
    dimension_entry(dimensions, key).merge(counter);
}

fn dimension_entry<'a>(dimensions: &'a mut HashMap<String, Counter>, key: &str) -> &'a mut Counter {
    dimension_entry_with_limit(dimensions, key, MAX_DIMENSION_KEYS)
}

fn dimension_entry_with_limit<'a>(
    dimensions: &'a mut HashMap<String, Counter>,
    key: &str,
    maximum: usize,
) -> &'a mut Counter {
    let maximum = maximum.max(2);
    let bounded_key = if dimensions.contains_key(key)
        || (key != OTHER_DIMENSION && dimensions.len() < maximum - 1)
    {
        key
    } else {
        OTHER_DIMENSION
    };
    dimensions.entry(bounded_key.to_owned()).or_default()
}

fn bound_dimensions(dimensions: &mut HashMap<String, Counter>) {
    bound_dimensions_to(dimensions, MAX_DIMENSION_KEYS);
}

fn bound_dimensions_to(dimensions: &mut HashMap<String, Counter>, maximum: usize) {
    if dimensions.len() < maximum {
        return;
    }
    let entries = std::mem::take(dimensions);
    for (key, counter) in entries {
        dimension_entry_with_limit(dimensions, &key, maximum).merge(&counter);
    }
}

#[derive(Default)]
pub struct ModelCache {
    state: Mutex<ModelCacheState>,
}

#[derive(Default)]
struct ModelCacheState {
    models: Vec<ModelInfo>,
    updated_at_ms: u64,
    refreshing: bool,
}

impl ModelCache {
    pub fn get(&self, ttl_ms: u64) -> (Vec<ModelInfo>, bool) {
        let state = lock(&self.state);
        let fresh =
            !state.models.is_empty() && now_ms().saturating_sub(state.updated_at_ms) <= ttl_ms;
        (state.models.clone(), fresh)
    }

    pub fn begin_refresh(&self) -> bool {
        let mut state = lock(&self.state);
        if state.refreshing {
            false
        } else {
            state.refreshing = true;
            true
        }
    }

    pub fn finish_refresh(&self, models: Vec<ModelInfo>) {
        let mut state = lock(&self.state);
        if !models.is_empty() {
            state.models = models;
            state.updated_at_ms = now_ms();
        }
        state.refreshing = false;
    }

    pub fn merge_refresh(&self, models: Vec<ModelInfo>) {
        let mut state = lock(&self.state);
        if !models.is_empty() {
            let mut merged = std::mem::take(&mut state.models)
                .into_iter()
                .map(|model| (model.model_id.clone(), model))
                .collect::<BTreeMap<_, _>>();
            for model in models {
                merged.insert(model.model_id.clone(), model);
            }
            state.models = merged.into_values().collect();
            state.updated_at_ms = now_ms();
        }
        state.refreshing = false;
    }
}

fn percentile(values: &[u64], quantile: f64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let index = ((values.len() - 1) as f64 * quantile).round() as usize;
    values[index]
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: &str, timestamp: i64, status: u16) -> RequestLog {
        RequestLog {
            timestamp,
            trace_id: format!("trace-{id}"),
            request_id: id.into(),
            path: "/v1/messages".into(),
            user_agent: String::new(),
            provider_id: "kiro".into(),
            model: "mapped".into(),
            original_model: "client".into(),
            kiro_model: "kiro".into(),
            account_id: "acc_test".into(),
            account_name: "Team account".into(),
            endpoint: "amazonq".into(),
            model_path: vec!["client".into(), "mapped".into(), "kiro".into()],
            model_mapping_rule: Some("primary".into()),
            attempts: Vec::new(),
            duration_ms: 10,
            status,
            input_tokens: 1,
            output_tokens: 1,
            credits: 0.1,
            error: (status >= 400).then(|| "failed".into()),
            diagnostics: RequestDiagnostics::default(),
        }
    }

    #[test]
    fn legacy_request_logs_default_new_diagnostic_fields() {
        let mut value = serde_json::to_value(request("req-old", 42, 502)).expect("serialize");
        let object = value.as_object_mut().expect("request object");
        object.remove("provider_id");
        object.remove("user_agent");
        object.remove("account_name");
        object.remove("model_path");
        object.remove("model_mapping_rule");
        object.remove("attempts");
        object.remove("diagnostics");
        let decoded: RequestLog = serde_json::from_value(value).expect("deserialize legacy log");
        assert!(decoded.provider_id.is_empty());
        assert!(decoded.user_agent.is_empty());
        assert!(decoded.account_name.is_empty());
        assert!(decoded.model_path.is_empty());
        assert!(decoded.model_mapping_rule.is_none());
        assert!(decoded.attempts.is_empty());
        assert_eq!(decoded.diagnostics.loaded_tool_count, 0);
        assert!(decoded.diagnostics.client_context.is_none());
    }

    #[test]
    fn legacy_aggregate_dimensions_are_backfilled_as_kiro() {
        let entry = request("legacy-provider", 42, 200);
        let mut aggregate = ProxyStats::default();
        record_persistent(&mut aggregate, &entry);
        aggregate.by_provider.clear();
        aggregate.provider_details.clear();
        let mut bucket = WindowBucket::default();
        bucket.record(&entry);
        bucket.by_provider.clear();
        bucket.provider_details.clear();
        aggregate.minute_buckets.insert(0, bucket);

        aggregate.migrate_legacy_kiro_dimensions();
        aggregate.retain_provider("kiro");

        assert_eq!(aggregate.total.requests, 1);
        assert_eq!(aggregate.by_account["acc_test"].requests, 1);
        assert_eq!(aggregate.by_endpoint["amazonq"].requests, 1);
        assert_eq!(aggregate.by_model["mapped"].requests, 1);
        let bucket = aggregate.minute_buckets.get(&0).expect("legacy minute");
        assert_eq!(bucket.by_provider["kiro"].requests, 1);
        assert_eq!(bucket.provider_details["kiro"].total.requests, 1);
    }

    #[tokio::test]
    async fn time_windows_are_not_limited_by_the_recent_request_ring() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = StatsStore::empty(&directory.path().join("stats.json"));
        let now = 2_000_000_000;
        for index in 0..1_500 {
            store.record(request(&format!("req-{index}"), now, 200));
        }
        assert_eq!(store.snapshot(None).recent_requests.len(), 1_000);
        assert_eq!(
            store
                .window(Some(now - 60), None)
                .await
                .expect("window")
                .total
                .requests,
            1_500
        );
    }

    #[tokio::test]
    async fn dimensions_are_bounded_in_totals_buckets_and_merged_windows() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = StatsStore::empty(&directory.path().join("stats.json"));
        let request_count = MAX_DIMENSION_KEYS + 100;
        for index in 0..request_count {
            let mut entry = request(&format!("req-{index}"), index as i64 * 60, 200);
            entry.account_id = format!("account-{index}");
            entry.endpoint = format!("endpoint-{index}");
            entry.model = format!("model-{index}");
            store.record(entry);
        }

        let snapshot = store.snapshot(None);
        for dimensions in [
            &snapshot.by_account,
            &snapshot.by_endpoint,
            &snapshot.by_model,
        ] {
            assert_eq!(dimensions.len(), MAX_DIMENSION_KEYS);
            assert_eq!(
                dimensions
                    .get(OTHER_DIMENSION)
                    .expect("overflow bucket")
                    .requests,
                101
            );
        }
        assert!(snapshot.minute_buckets.values().all(|bucket| {
            bucket.by_account.len() <= MAX_BUCKET_DIMENSION_KEYS
                && bucket.by_endpoint.len() <= MAX_BUCKET_DIMENSION_KEYS
                && bucket.by_model.len() <= MAX_BUCKET_DIMENSION_KEYS
                && bucket.latencies_ms.len() <= MAX_BUCKET_LATENCIES
        }));

        let window = store.window(Some(0), None).await.expect("window");
        assert_eq!(window.total.requests, request_count as u64);
        assert!(window.by_account.len() <= MAX_DIMENSION_KEYS);
        assert!(window.by_endpoint.len() <= MAX_DIMENSION_KEYS);
        assert!(window.by_model.len() <= MAX_DIMENSION_KEYS);
    }

    #[tokio::test]
    async fn provider_filter_uses_provider_local_dimensions() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = StatsStore::empty(&directory.path().join("stats.json"));
        let mut kiro = request("kiro", 100, 200);
        kiro.input_tokens = 11;
        let mut copilot = request("copilot", 101, 500);
        copilot.provider_id = "copilot-team".into();
        copilot.account_id = "acc_copilot".into();
        copilot.endpoint = "api.githubcopilot.com".into();
        copilot.model = "gpt-test".into();
        copilot.input_tokens = 22;
        store.record(kiro);
        store.record(copilot);

        let mut scoped = store.window(None, None).await.expect("stats window");
        scoped.retain_provider("copilot-team");
        assert_eq!(scoped.total.requests, 1);
        assert_eq!(scoped.total.failures, 1);
        assert_eq!(scoped.total.input_tokens, 22);
        assert_eq!(scoped.by_model["gpt-test"].requests, 1);
        assert_eq!(scoped.by_account["acc_copilot"].requests, 1);
        assert_eq!(scoped.by_endpoint["api.githubcopilot.com"].requests, 1);
        assert_eq!(scoped.recent_requests.len(), 1);
        assert_eq!(scoped.recent_requests[0].provider_id, "copilot-team");
    }

    #[test]
    fn minute_buckets_bound_cardinality_without_dropping_history() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = StatsStore::empty(&directory.path().join("stats.json"));
        for index in 0..(MAX_BUCKET_DIMENSION_KEYS + 100) {
            let mut entry = request(&format!("same-minute-{index}"), 1_000_000, 200);
            entry.account_id = format!("account-{index}");
            entry.endpoint = format!("endpoint-{index}");
            entry.model = format!("model-{index}");
            store.record(entry);
        }
        let high_cardinality = store.snapshot(None);
        let bucket = high_cardinality
            .minute_buckets
            .values()
            .next()
            .expect("minute bucket");
        assert_eq!(bucket.by_account.len(), MAX_BUCKET_DIMENSION_KEYS);
        assert_eq!(bucket.by_endpoint.len(), MAX_BUCKET_DIMENSION_KEYS);
        assert_eq!(bucket.by_model.len(), MAX_BUCKET_DIMENSION_KEYS);
        assert_eq!(
            bucket.latencies_ms.len(),
            MAX_BUCKET_LATENCIES.min(MAX_BUCKET_DIMENSION_KEYS + 100)
        );
        let history_minutes = 7 * 24 * 60 + 100;
        for minute in 0..history_minutes {
            store.record(request(
                &format!("minute-{minute}"),
                2_000_000 + minute as i64 * 60,
                200,
            ));
        }

        let snapshot = store.snapshot(None);
        assert_eq!(snapshot.minute_buckets.len(), history_minutes + 1);
        assert!(snapshot.minute_buckets.values().all(|bucket| {
            bucket.by_account.len() <= MAX_BUCKET_DIMENSION_KEYS
                && bucket.by_endpoint.len() <= MAX_BUCKET_DIMENSION_KEYS
                && bucket.by_model.len() <= MAX_BUCKET_DIMENSION_KEYS
                && bucket.latencies_ms.len() <= MAX_BUCKET_LATENCIES
        }));
    }

    #[tokio::test]
    async fn log_follow_wakes_on_a_matching_record() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = std::sync::Arc::new(StatsStore::empty(&directory.path().join("stats.json")));
        let follower = {
            let store = std::sync::Arc::clone(&store);
            tokio::spawn(async move {
                store
                    .follow(None, 10, 1_000, Some("error"), None, None)
                    .await
            })
        };
        tokio::task::yield_now().await;
        store.record(request("ok", 1, 200));
        store.record(request("failed", 2, 503));
        let entries = follower.await.expect("join");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].request_id, "failed");
    }

    #[tokio::test]
    async fn log_tail_is_applied_after_level_and_provider_filters() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = StatsStore::empty(&directory.path().join("stats.json"));
        let mut first = request("failed-kiro", 1, 400);
        first.provider_id = "kiro".into();
        store.record(first);
        for index in 0..20 {
            store.record(request(&format!("ok-{index}"), index + 2, 200));
        }
        let mut last = request("failed-copilot", 23, 502);
        last.provider_id = "copilot".into();
        store.record(last);

        let failures = store.follow(None, 2, 0, Some("warn"), None, None).await;
        assert_eq!(
            failures
                .iter()
                .map(|entry| entry.request_id.as_str())
                .collect::<Vec<_>>(),
            ["failed-kiro", "failed-copilot"]
        );
        let kiro = store
            .follow(None, 1, 0, Some("warn"), None, Some("kiro"))
            .await;
        assert_eq!(kiro[0].request_id, "failed-kiro");
        let after_first = store
            .follow(Some("failed-kiro"), 1, 0, Some("warn"), None, None)
            .await;
        assert_eq!(after_first[0].request_id, "failed-copilot");
    }

    #[tokio::test]
    async fn bootstrap_empty_stats_remain_compatible_but_partial_state_fails() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("stats.json");
        tokio::fs::write(&path, "{}\n").await.expect("write empty");
        let store = StatsStore::load(&path).await.expect("load bootstrap state");
        assert_eq!(store.snapshot(None).total.requests, 0);

        tokio::fs::write(&path, r#"{"total":{}}"#)
            .await
            .expect("write partial");
        assert!(StatsStore::load(&path).await.is_err());
    }

    #[tokio::test]
    async fn legacy_time_series_is_marked_incomplete() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("stats.json");
        let store = StatsStore::empty(&path);
        store.record(request("legacy", 1_000_000, 200));
        store.persist().await.expect("persist");
        let mut value: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(&path)
                .await
                .expect("read persisted stats"),
        )
        .expect("stats JSON");
        value
            .as_object_mut()
            .expect("stats object")
            .remove("history_complete");
        tokio::fs::write(&path, serde_json::to_vec(&value).expect("encode legacy"))
            .await
            .expect("write legacy");

        let legacy = StatsStore::load(&path).await.expect("load legacy");
        assert!(!legacy.persistent_history_complete());
        assert_eq!(legacy.persistent_history_started_at(), Some(999_960));
    }

    #[tokio::test]
    async fn persisted_and_current_session_statistics_are_independent() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("stats.json");
        let store = StatsStore::empty(&path);
        store.record(request("before-restart", crate::now_secs(), 200));
        store.persist().await.expect("persist");

        let reloaded = StatsStore::load(&path).await.expect("reload");
        assert_eq!(reloaded.snapshot(None).total.requests, 1);
        assert_eq!(reloaded.session_snapshot().total.requests, 0);

        reloaded.record(request("after-restart", crate::now_secs(), 503));
        assert_eq!(reloaded.snapshot(None).total.requests, 2);
        let session = reloaded.session_snapshot();
        assert_eq!(session.total.requests, 1);
        assert_eq!(session.total.failures, 1);
    }

    #[tokio::test]
    async fn persistence_shards_history_by_utc_hour_and_keeps_summary_compact() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("stats.json");
        let store = StatsStore::empty(&path);
        store.record(request("day-one", 60, 200));
        store.record(request("day-two", 86_460, 503));
        store.persist().await.expect("persist");

        let persisted: ProxyStats = serde_json::from_str(
            &tokio::fs::read_to_string(&path)
                .await
                .expect("read summary"),
        )
        .expect("parse summary");
        assert!(persisted.minute_buckets.is_empty());
        assert_eq!(persisted.total.requests, 2);

        let history_dir = history_dir_for(&path);
        assert!(
            tokio::fs::try_exists(history_dir.join("1970-01-01-00.json"))
                .await
                .expect("first shard")
        );
        assert!(
            tokio::fs::try_exists(history_dir.join("1970-01-02-00.json"))
                .await
                .expect("second shard")
        );

        let reloaded = StatsStore::load(&path).await.expect("reload");
        let window = reloaded
            .window_between(Some(0), Some(172_799), None)
            .await
            .expect("history window");
        assert_eq!(window.total.requests, 2);
        assert_eq!(window.total.successes, 1);
        assert_eq!(window.total.failures, 1);
    }

    #[tokio::test]
    async fn current_day_memory_overrides_persisted_bucket_without_double_counting() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("stats.json");
        let now = crate::now_secs();
        let store = StatsStore::empty(&path);
        store.record(request("first", now, 200));
        store.record(request("second", now, 200));
        store.persist().await.expect("persist");

        let reloaded = StatsStore::load(&path).await.expect("reload");
        let window = reloaded
            .window_between(Some(now - 60), Some(now + 60), None)
            .await
            .expect("history window");
        assert_eq!(window.total.requests, 2);
    }

    #[tokio::test]
    async fn legacy_embedded_minutes_migrate_idempotently_to_hourly_shards() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("stats.json");
        let legacy_store = StatsStore::empty(&path);
        legacy_store.record(request("legacy", 60, 200));
        let legacy = legacy_store.snapshot(None);
        write_json_off_thread(&path, legacy)
            .await
            .expect("write legacy state");

        let migrated = StatsStore::load(&path).await.expect("load legacy state");
        migrated.persist().await.expect("migrate");
        // Repeating the migration write must replace the same absolute bucket.
        migrated
            .persist()
            .await
            .expect("repeat migration persistence");
        let reloaded = StatsStore::load(&path)
            .await
            .expect("reload migrated state");
        let window = reloaded
            .window_between(Some(0), Some(120), None)
            .await
            .expect("migrated window");
        assert_eq!(window.total.requests, 1);
    }

    #[tokio::test]
    async fn failed_history_persistence_checkpoint_survives_a_restart() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("stats.json");
        let history_dir = history_dir_for(&path);
        tokio::fs::write(&history_dir, b"blocks directory creation")
            .await
            .expect("write blocking file");
        let store = StatsStore::empty(&path);
        store.record(request("retained", crate::now_secs(), 200));

        assert!(store.persist().await.is_err());
        drop(store);
        tokio::fs::remove_file(&history_dir)
            .await
            .expect("remove blocking file");
        let recovered = StatsStore::load(&path).await.expect("recover checkpoint");
        assert_eq!(recovered.snapshot(None).total.requests, 1);
        recovered.persist().await.expect("retry persistence");

        let reloaded = StatsStore::load(&path).await.expect("reload");
        assert_eq!(reloaded.snapshot(None).total.requests, 1);
    }

    #[tokio::test]
    async fn corrupt_current_history_is_quarantined_without_losing_summary() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("stats.json");
        let now = crate::now_secs();
        let store = StatsStore::empty(&path);
        store.record(request("persisted", now, 200));
        store.persist().await.expect("persist");
        let current_hour = utc_hour_from_minute(now.div_euclid(60));
        let current_path = history_dir_for(&path).join(format!("{current_hour}.json"));
        tokio::fs::write(&current_path, b"not-json")
            .await
            .expect("corrupt shard");

        let recovered = StatsStore::load(&path).await.expect("recover");
        assert_eq!(recovered.snapshot(None).total.requests, 1);
        assert!(!recovered.persistent_history_complete());
        let gaps = recovered.persistent_history_gaps();
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].end - gaps[0].start, 3_599);
        assert!(!tokio::fs::try_exists(&current_path)
            .await
            .expect("current shard removed"));
        let mut entries = tokio::fs::read_dir(history_dir_for(&path))
            .await
            .expect("history directory");
        let mut found_quarantine = false;
        while let Some(entry) = entries.next_entry().await.expect("history entry") {
            found_quarantine |= entry
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("{current_hour}.json.corrupt-"));
        }
        assert!(found_quarantine);
    }

    #[tokio::test]
    async fn historical_corruption_is_reported_as_a_persisted_range_gap() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("stats.json");
        let summary = ProxyStats {
            history_started_at: Some(0),
            ..ProxyStats::default()
        };
        write_json_off_thread(&path, summary)
            .await
            .expect("write summary");
        let history_dir = history_dir_for(&path);
        tokio::fs::create_dir_all(&history_dir)
            .await
            .expect("create history directory");
        let corrupt_path = history_dir.join("1970-01-01-00.json");
        tokio::fs::write(&corrupt_path, b"not-json")
            .await
            .expect("write corrupt history");

        let store = StatsStore::load(&path).await.expect("load summary");
        let window = store
            .window_between(Some(0), Some(3_599), None)
            .await
            .expect("read partial history");
        assert!(!window.history_complete);
        assert_eq!(window.history_gaps.len(), 1);
        assert_eq!(window.history_gaps[0].start, 0);
        assert_eq!(window.history_gaps[0].end, 3_599);
        assert_eq!(store.persistent_history_gaps(), window.history_gaps);

        store.persist().await.expect("persist discovered gap");
        let reloaded = StatsStore::load(&path).await.expect("reload gap metadata");
        assert_eq!(reloaded.persistent_history_gaps(), window.history_gaps);
    }

    #[tokio::test]
    async fn hourly_history_overrides_legacy_daily_minutes() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("stats.json");
        write_json_off_thread(&path, ProxyStats::default())
            .await
            .expect("write summary");
        let history_dir = history_dir_for(&path);
        tokio::fs::create_dir_all(&history_dir)
            .await
            .expect("create history directory");
        let mut daily_bucket = WindowBucket::default();
        daily_bucket.record(&request("daily", 60, 200));
        write_json_off_thread(
            &history_dir.join("1970-01-01.json"),
            HistoryDay {
                version: HISTORY_VERSION,
                minutes: BTreeMap::from([(1, daily_bucket)]),
            },
        )
        .await
        .expect("write legacy daily shard");
        let mut hourly_bucket = WindowBucket::default();
        hourly_bucket.record(&request("hourly-one", 60, 200));
        hourly_bucket.record(&request("hourly-two", 61, 200));
        write_json_off_thread(
            &history_dir.join("1970-01-01-00.json"),
            HistoryDay {
                version: HISTORY_VERSION,
                minutes: BTreeMap::from([(1, hourly_bucket)]),
            },
        )
        .await
        .expect("write hourly shard");

        let store = StatsStore::load(&path).await.expect("load history");
        let window = store
            .window_between(Some(0), Some(120), None)
            .await
            .expect("query history");
        assert_eq!(window.total.requests, 2);
    }

    #[tokio::test]
    async fn narrow_queries_do_not_open_unrelated_hourly_shards() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("stats.json");
        let store = StatsStore::empty(&path);
        store.record(request("first-hour", 60, 200));
        store.persist().await.expect("persist first hour");
        let unrelated = history_dir_for(&path).join("1970-01-01-01.json");
        tokio::fs::write(&unrelated, b"not-json")
            .await
            .expect("write unrelated corrupt shard");

        let reloaded = StatsStore::load(&path).await.expect("reload");
        let window = reloaded
            .window_between(Some(0), Some(120), None)
            .await
            .expect("narrow query");
        assert_eq!(window.total.requests, 1);
        assert!(window.history_gaps.is_empty());
        assert!(tokio::fs::try_exists(unrelated)
            .await
            .expect("unrelated shard remains untouched"));
    }

    #[tokio::test]
    async fn multi_day_ranges_stream_all_hourly_shards_without_double_counting() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("stats.json");
        let store = StatsStore::empty(&path);
        for hour in 0..72 {
            store.record(request(
                &format!("hour-{hour}"),
                hour * 3_600 + 60,
                if hour % 2 == 0 { 200 } else { 503 },
            ));
        }
        store.persist().await.expect("persist hourly history");

        let reloaded = StatsStore::load(&path).await.expect("reload");
        let window = reloaded
            .window_between(Some(0), Some(72 * 3_600 - 1), None)
            .await
            .expect("multi-day range");
        assert_eq!(window.total.requests, 72);
        assert_eq!(window.total.successes, 36);
        assert_eq!(window.total.failures, 36);
    }

    #[test]
    fn utc_history_day_names_cover_epoch_boundaries() {
        assert_eq!(utc_day_from_minute(-1), "1969-12-31");
        assert_eq!(utc_day_from_minute(0), "1970-01-01");
        assert_eq!(utc_day_from_minute(1_440), "1970-01-02");
        assert_eq!(utc_hour_from_minute(-1), "1969-12-31-23");
        assert_eq!(utc_hour_from_minute(60), "1970-01-01-01");
        assert_eq!(
            history_range_from_shard_name("1970-01-01"),
            Some((0, 86_399))
        );
        assert_eq!(
            history_range_from_shard_name("1970-01-01-01"),
            Some((3_600, 7_199))
        );
        assert_eq!(history_range_from_shard_name("2026-02-30-01"), None);
    }

    #[tokio::test]
    async fn bounded_ranges_keep_all_minute_aggregates_and_exact_recent_details() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = StatsStore::empty(&directory.path().join("stats.json"));
        store.record(request("first", 120, 200));
        store.record(request("middle", 180, 200));
        store.record(request("last", 240, 500));

        let window = store
            .window_between(Some(180), Some(239), None)
            .await
            .expect("window");
        assert_eq!(window.total.requests, 1);
        assert_eq!(window.total.successes, 1);
        assert_eq!(window.recent_requests.len(), 1);
        assert_eq!(window.recent_requests[0].request_id, "middle");
    }
}
