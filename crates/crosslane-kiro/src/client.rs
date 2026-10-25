//! Upstream HTTP client with independent short-request and streaming pools.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use crosslane_core::account::{Account, AuthMethod, Subscription, SubscriptionKind, Usage};
use crosslane_core::config::{AgentMode, UpstreamConfig};
use crosslane_translate::{validate_kiro_tool_history, KiroPayload, WebSearchResults};
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use futures::future::{BoxFuture, FutureExt, Shared};
use futures::{Stream, StreamExt};
use reqwest::{Client, Response};
use serde::{Deserialize, Serialize};
use tokio_util::codec::Decoder;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::endpoint::{
    account_api_region, management_base, supports_native_management, EndpointCache,
    EndpointDefinition, EndpointKey, EndpointOverrides, EndpointPurpose,
};
use crate::event_stream::{EventStreamDecoder, KiroEvent};
use crate::identity::{
    cli_amz_user_agent, cli_user_agent, ide_amz_user_agent, ide_service_amz_user_agent,
    ide_service_user_agent, ide_user_agent, CliService, IdeService,
};

const KIRO_BUILDER_ID_PROFILE_ARN: &str =
    "arn:aws:codewhisperer:us-east-1:638616132270:profile/AAAACCCCXXXX";
const KIRO_SOCIAL_PROFILE_ARN: &str =
    "arn:aws:codewhisperer:us-east-1:699475941385:profile/EHGA3GRVQMUK";
const LOCAL_STREAM_SLOT_ENDPOINT: &str = "crosslane.stream-slot";
const PROFILE_CACHE_CAPACITY: usize = 500;
const PROFILE_CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_DISCOVERY_PAGES: usize = 100;

#[derive(Clone)]
pub struct KiroError {
    pub status: Option<u16>,
    pub endpoint: String,
    pub message: String,
}

impl std::fmt::Display for KiroError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Kiro {} returned {:?}: {}",
            crosslane_translate::error::redact_kiro_keys(&self.endpoint),
            self.status,
            crosslane_translate::error::redact_kiro_keys(&self.message)
        )
    }
}

impl std::fmt::Debug for KiroError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KiroError")
            .field("status", &self.status)
            .field(
                "endpoint",
                &crosslane_translate::error::redact_kiro_keys(&self.endpoint),
            )
            .field(
                "message",
                &crosslane_translate::error::redact_kiro_keys(&self.message),
            )
            .finish()
    }
}

impl std::error::Error for KiroError {}

impl KiroError {
    /// A local capacity failure has no supplier response and must not affect
    /// account health or cause another attempt against the same semaphore.
    pub fn stream_slot_timeout(timeout_ms: u64) -> Self {
        Self {
            status: Some(503),
            endpoint: LOCAL_STREAM_SLOT_ENDPOINT.into(),
            message: format!("timed out after {timeout_ms} ms waiting for an upstream stream slot"),
        }
    }

    pub fn is_local_overload(&self) -> bool {
        self.endpoint == LOCAL_STREAM_SLOT_ENDPOINT && self.status == Some(503)
    }

    pub fn is_auth(&self) -> bool {
        self.status == Some(401)
            || (!self.is_quota()
                && !self.is_throttle()
                && (self.status == Some(403) || text_is_auth_error(&self.message)))
    }

    pub fn is_quota(&self) -> bool {
        self.status == Some(402) || text_is_quota_error(&self.message)
    }

    pub fn is_throttle(&self) -> bool {
        !self.is_quota() && (self.status == Some(429) || text_is_throttle_error(&self.message))
    }

    pub fn is_model_temporarily_unavailable(&self) -> bool {
        matches!(self.status, None | Some(500..=599))
            && text_is_model_temporarily_unavailable(&self.message)
    }

    pub fn is_model_capacity_error(&self) -> bool {
        self.is_throttle() || self.is_model_temporarily_unavailable()
    }

    pub fn is_context_too_long(&self) -> bool {
        text_is_context_too_long(&self.message)
    }

    pub fn is_retriable(&self) -> bool {
        !self.is_local_overload()
            && (self.status.is_none()
                || self.is_throttle()
                || matches!(self.status, Some(500..=599)))
    }

    pub fn is_request_rejection(&self) -> bool {
        matches!(self.status, Some(400 | 413 | 422)) || text_is_request_rejection(&self.message)
    }

    pub fn retry_after_ms(&self) -> Option<u64> {
        retry_after_millis(&self.message)
    }
}

/// Preserve Kiro's bounded retry hint without changing the public error shape.
pub fn retry_after_millis(message: &str) -> Option<u64> {
    let start = message.find('{')?;
    let value: serde_json::Value = serde_json::from_str(&message[start..]).ok()?;
    value
        .get("retryAfterMilliseconds")?
        .as_u64()
        .map(|delay| delay.min(300_000))
}

/// Stream assembly retains the structured local endpoint in KiroError's
/// display form. Do not classify arbitrary supplier 503s as local overload.
pub fn text_is_local_overload(message: &str) -> bool {
    message.starts_with("Kiro crosslane.stream-slot returned Some(503): ")
}

pub fn text_is_auth_error(message: &str) -> bool {
    let text = message.to_ascii_lowercase();
    [
        "unauthorized",
        "unauthenticated",
        "invalid token",
        "expired token",
        "token expired",
        "access denied",
        "authentication",
    ]
    .iter()
    .any(|marker| text.contains(marker))
        || contains_status(&text, 401)
        || contains_status(&text, 403)
}

pub fn text_is_quota_error(message: &str) -> bool {
    let text = message.to_ascii_lowercase().replace('_', " ");
    [
        "quota",
        "credit exhausted",
        "credits exhausted",
        "insufficient credit",
        "usage limit",
        "request count",
        "overage request limit exceeded",
    ]
    .iter()
    .any(|marker| text.contains(marker))
        || contains_status(&text, 402)
}

pub fn text_is_throttle_error(message: &str) -> bool {
    // Kiro reports exhausted allowances through ThrottlingException/429 too.
    // Its quota reason takes precedence over the generic exception name.
    if text_is_quota_error(message) {
        return false;
    }
    let text = message.to_ascii_lowercase().replace('_', " ");
    [
        "throttl",
        "too many requests",
        "rate limit",
        "rate exceeded",
    ]
    .iter()
    .any(|marker| text.contains(marker))
        || contains_status(&text, 429)
}

pub fn text_is_model_temporarily_unavailable(message: &str) -> bool {
    let text = message.to_ascii_lowercase();
    [
        "model_temporarily_unavailable",
        "model temporarily unavailable",
        "unexpectedly high load",
        "insufficient_model_capacity",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

pub fn text_is_context_too_long(message: &str) -> bool {
    crosslane_translate::error::is_context_length_error(message)
}

pub fn text_is_request_rejection(message: &str) -> bool {
    let text = message.to_ascii_lowercase();
    text_is_context_too_long(&text)
        || [
            "too many tools",
            "tool definitions are too large",
            "tool schema",
            "payload too large",
            "request too large",
            "request entity too large",
            "validationexception",
            "validationerror",
        ]
        .iter()
        .any(|marker| text.contains(marker))
        || contains_status(&text, 413)
        || contains_status(&text, 422)
}

fn contains_status(text: &str, status: u16) -> bool {
    let status = status.to_string();
    text.split(|character: char| !character.is_ascii_alphanumeric())
        .any(|word| word == status)
}

pub struct KiroResponse {
    pub endpoint: EndpointDefinition,
    pub response: Response,
    permit: tokio::sync::OwnedSemaphorePermit,
    stream_slot_wait_ms: u64,
    thinking_enabled: bool,
    redaction_session: crosslane_translate::RedactionSession,
    #[cfg(feature = "crs")]
    prepared_payload: KiroPayload,
}

impl KiroResponse {
    /// Private request-local mapping; never serialize this into model context.
    pub fn redaction_session(&self) -> &crosslane_translate::RedactionSession {
        &self.redaction_session
    }

    /// Thinking requested by this actual attempt, after model-specific preparation.
    pub fn thinking_enabled(&self) -> bool {
        self.thinking_enabled
    }

    /// Time spent waiting for an upstream streaming connection slot.
    pub fn stream_slot_wait_ms(&self) -> u64 {
        self.stream_slot_wait_ms
    }

    pub fn into_parts(
        self,
    ) -> (
        EndpointDefinition,
        Response,
        tokio::sync::OwnedSemaphorePermit,
    ) {
        (self.endpoint, self.response, self.permit)
    }

    /// Includes the prepared payload used by this actual attempt, so an
    /// isolated endpoint retry need not resolve model controls a second time.
    #[cfg(feature = "crs")]
    pub fn into_parts_with_payload(
        self,
    ) -> (
        EndpointDefinition,
        Response,
        tokio::sync::OwnedSemaphorePermit,
        KiroPayload,
    ) {
        (
            self.endpoint,
            self.response,
            self.permit,
            self.prepared_payload,
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub model_id: String,
    #[serde(default)]
    pub model_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub rate_multiplier: Option<f64>,
    #[serde(default)]
    pub token_limits: Option<TokenLimits>,
    #[serde(default)]
    pub additional_model_request_fields_schema: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenLimits {
    #[serde(default)]
    pub max_input_tokens: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionList {
    #[serde(default)]
    pub disclaimer: Vec<String>,
    #[serde(default)]
    pub subscription_plans: Vec<SubscriptionPlan>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionPlan {
    pub name: String,
    #[serde(default)]
    pub q_subscription_type: String,
    #[serde(default)]
    pub description: SubscriptionDescription,
    #[serde(default)]
    pub pricing: SubscriptionPricing,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionDescription {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub billing_interval: String,
    #[serde(default)]
    pub feature_header: String,
    #[serde(default)]
    pub features: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubscriptionPricing {
    #[serde(default)]
    pub amount: f64,
    #[serde(default)]
    pub currency: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageInfo {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub credits: f64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageLimits {
    #[serde(default)]
    pub usage_breakdown_list: Vec<UsageBreakdown>,
    #[serde(default)]
    pub overage_configuration: Option<OverageConfiguration>,
    #[serde(default)]
    pub next_date_reset: Option<serde_json::Value>,
    #[serde(default)]
    pub days_until_reset: Option<i64>,
    #[serde(default)]
    pub subscription_info: Option<UsageSubscription>,
    #[serde(default)]
    pub user_info: Option<UsageUserInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageUserInfo {
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub user_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageBreakdown {
    #[serde(default, alias = "type")]
    pub resource_type: String,
    #[serde(default)]
    pub current_usage: Option<f64>,
    #[serde(default)]
    pub current_usage_with_precision: Option<f64>,
    #[serde(default)]
    pub usage_limit: Option<f64>,
    #[serde(default)]
    pub usage_limit_with_precision: Option<f64>,
    #[serde(default)]
    pub overage_cap: Option<f64>,
    #[serde(default)]
    pub overage_cap_with_precision: Option<f64>,
    #[serde(default)]
    pub free_trial_info: Option<UsageAllowance>,
    #[serde(default)]
    pub free_trial_usage: Option<UsageAllowance>,
    #[serde(default)]
    pub bonuses: Vec<UsageAllowance>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverageConfiguration {
    #[serde(default)]
    pub overage_status: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageAllowance {
    #[serde(default)]
    pub current_usage: Option<f64>,
    #[serde(default)]
    pub current_usage_with_precision: Option<f64>,
    #[serde(default)]
    pub usage_limit: Option<f64>,
    #[serde(default)]
    pub usage_limit_with_precision: Option<f64>,
    #[serde(default)]
    pub free_trial_status: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSubscription {
    #[serde(default)]
    pub subscription_title: String,
    #[serde(default, alias = "type")]
    pub subscription_type: String,
}

impl UsageLimits {
    pub fn normalized_usage(&self, now: i64) -> Option<Usage> {
        let credit = self
            .usage_breakdown_list
            .iter()
            .find(|item| matches!(item.resource_type.as_str(), "CREDIT" | "AGENTIC_REQUEST"))?;
        let mut current = precise(credit.current_usage_with_precision, credit.current_usage);
        let mut limit = precise(credit.usage_limit_with_precision, credit.usage_limit);
        if let Some(trial) = credit
            .free_trial_info
            .as_ref()
            .or(credit.free_trial_usage.as_ref())
            .filter(|trial| trial.free_trial_status.as_deref() == Some("ACTIVE"))
        {
            current += precise(trial.current_usage_with_precision, trial.current_usage);
            limit += precise(trial.usage_limit_with_precision, trial.usage_limit);
        }
        for bonus in &credit.bonuses {
            current += precise(bonus.current_usage_with_precision, bonus.current_usage);
            limit += precise(bonus.usage_limit_with_precision, bonus.usage_limit);
        }
        let mut overage_cap = None;
        if self
            .overage_configuration
            .as_ref()
            .is_some_and(|configuration| {
                configuration.overage_status.eq_ignore_ascii_case("ENABLED")
            })
        {
            let cap = precise(credit.overage_cap_with_precision, credit.overage_cap);
            if cap.is_finite() && cap > 0.0 {
                // Kiro includes consumed overage in currentUsage; only the cap extends capacity.
                limit += cap;
                overage_cap = Some(cap);
            }
        }
        Some(Usage {
            current,
            limit,
            overage_cap,
            percent_used: if limit > 0.0 {
                (current / limit * 100.0).clamp(0.0, 100.0)
            } else {
                0.0
            },
            next_reset_date: self.next_date_reset.as_ref().map(display_reset),
            updated_at: now,
        })
    }

    pub fn normalized_subscription(&self) -> Option<Subscription> {
        let source = self.subscription_info.as_ref()?;
        let raw = if source.subscription_type.is_empty() {
            source.subscription_title.clone()
        } else {
            source.subscription_type.clone()
        };
        Some(Subscription {
            kind: subscription_kind(&raw, &source.subscription_title),
            title: (!source.subscription_title.is_empty())
                .then(|| source.subscription_title.clone()),
            raw_type: (!raw.is_empty()).then_some(raw),
            expires_at: self
                .next_date_reset
                .as_ref()
                .and_then(serde_json::Value::as_i64),
            days_remaining: self.days_until_reset,
        })
    }
}

fn precise(precise: Option<f64>, fallback: Option<f64>) -> f64 {
    precise.or(fallback).unwrap_or(0.0)
}

fn display_reset(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

fn subscription_kind(raw: &str, title: &str) -> SubscriptionKind {
    let value = format!("{raw} {title}").to_ascii_uppercase();
    if value.contains("PRO_PLUS") || value.contains("PRO+") {
        SubscriptionKind::ProPlus
    } else if value.contains("POWER") {
        SubscriptionKind::Power
    } else if value.contains("ENTERPRISE") {
        SubscriptionKind::Enterprise
    } else if value.contains("TEAM") {
        SubscriptionKind::Teams
    } else if value.contains("PRO") {
        SubscriptionKind::Pro
    } else if value.contains("FREE") {
        SubscriptionKind::Free
    } else {
        SubscriptionKind::Unknown
    }
}

#[derive(Clone)]
pub struct KiroClient {
    model_policy: Arc<crosslane_provider::ModelPolicy>,
    short: Client,
    mcp: Client,
    stream: Client,
    cache: Arc<EndpointCache>,
    overrides: EndpointOverrides,
    upstream: UpstreamConfig,
    short_slots: Arc<tokio::sync::Semaphore>,
    stream_slots: Arc<tokio::sync::Semaphore>,
    model_fetches: Arc<DashMap<(String, String), InFlightModelFetch>>,
    profile_fetches: Arc<DashMap<ProfileFetchKey, InFlightProfileFetch>>,
    profile_cache: Arc<Mutex<lru::LruCache<ProfileFetchKey, CachedProfile>>>,
}

type ProfileFetchKey = (String, String, String);

struct CachedProfile {
    arn: String,
    expires: Instant,
}

type SharedModelFetch = Shared<BoxFuture<'static, Result<Vec<ModelInfo>, KiroError>>>;

struct InFlightModelFetch {
    id: Uuid,
    future: SharedModelFetch,
}

type SharedProfileFetch = Shared<BoxFuture<'static, Result<String, KiroError>>>;

struct InFlightProfileFetch {
    id: Uuid,
    future: SharedProfileFetch,
}

impl KiroClient {
    pub fn new(upstream: UpstreamConfig, overrides: EndpointOverrides) -> Result<Self, KiroError> {
        let idle = Duration::from_millis(
            upstream
                .pool
                .keep_alive_idle_ms
                .saturating_sub(2_000)
                .max(1)
                .min(upstream.pool.keep_alive_max_ms.max(1)),
        );
        let short = Client::builder()
            .pool_max_idle_per_host(upstream.pool.http_max_connections)
            .pool_idle_timeout(idle)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(build_error)?;
        let mcp = Client::builder()
            .pool_max_idle_per_host(upstream.pool.http_max_connections)
            .pool_idle_timeout(idle)
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_millis(
                upstream.web_search_timeout_ms.clamp(1_000, 300_000),
            ))
            .build()
            .map_err(build_error)?;
        // There is deliberately no total timeout: legitimate generations may be long.
        // A finite per-read timeout still prevents a silent peer from pinning a slot forever.
        // hyper does not pipeline HTTP/1.1 requests, avoiding long-stream HOL blocking.
        let stream = Client::builder()
            .pool_max_idle_per_host(upstream.pool.stream_max_connections)
            .pool_idle_timeout(idle)
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_millis(upstream.stream_read_timeout_ms))
            .build()
            .map_err(build_error)?;
        Ok(Self {
            model_policy: Arc::new(crosslane_provider::ModelPolicy::default()),
            short,
            mcp,
            stream,
            cache: Arc::new(EndpointCache::default()),
            overrides,
            short_slots: Arc::new(tokio::sync::Semaphore::new(
                upstream
                    .pool
                    .http_max_connections
                    .saturating_mul(upstream.pool.http_pipelining)
                    .max(1),
            )),
            stream_slots: Arc::new(tokio::sync::Semaphore::new(
                upstream
                    .pool
                    .stream_max_connections
                    .saturating_mul(upstream.pool.stream_pipelining)
                    .max(1),
            )),
            model_fetches: Arc::new(DashMap::new()),
            profile_fetches: Arc::new(DashMap::new()),
            profile_cache: Arc::new(Mutex::new(lru::LruCache::new(
                NonZeroUsize::new(PROFILE_CACHE_CAPACITY).expect("nonzero profile cache capacity"),
            ))),
            upstream,
        })
    }

    pub fn endpoint_cache(&self) -> Arc<EndpointCache> {
        Arc::clone(&self.cache)
    }

    pub fn with_model_policy(mut self, policy: Arc<crosslane_provider::ModelPolicy>) -> Self {
        self.model_policy = policy;
        self
    }

    async fn admit_model<'a>(
        &'a self,
        payload: &KiroPayload,
    ) -> Result<tokio::sync::RwLockReadGuard<'a, crosslane_core::config::ModelsConfig>, KiroError>
    {
        let actual = payload
            .conversation_state
            .current_message
            .user_input_message
            .model_id
            .as_str();
        let original = payload
            .model_request_intent
            .as_ref()
            .map_or(actual, |intent| intent.requested_model.as_str());
        self.model_policy
            .admit(&[actual, original])
            .await
            .map_err(|error| KiroError {
                status: Some(400),
                endpoint: "model-policy".into(),
                message: error.to_string(),
            })
    }

    pub async fn generate(
        &self,
        account: &Account,
        payload: &KiroPayload,
        explicit: Option<EndpointKey>,
    ) -> Result<KiroResponse, KiroError> {
        #[cfg(feature = "crs")]
        if explicit.is_none()
            && payload.requires_native_generation()
            && self.runtime_isolated_retry_eligible(account, payload, EndpointKey::Amazonq)
        {
            // Legacy generation omits refusal metadata and can terminate in
            // the middle of a tool argument while still returning HTTP 200.
            // Claude 5 clients need the native terminal reason to cancel that
            // call and run their own recovery. Select that protocol before
            // generation; do not learn a preference for unrelated requests.
            return self
                .generate_at_endpoint_isolated(account, payload, EndpointKey::Runtime)
                .await;
        }
        drop(self.admit_model(payload).await?);
        let explicit = explicit.or(self.upstream.preferred_endpoint.map(EndpointKey::from));
        let order = self
            .cache
            .order(account, explicit, EndpointPurpose::Generation);
        if order.is_empty() {
            return Err(KiroError {
                status: Some(403),
                endpoint: "all".into(),
                message: "All Kiro endpoints previously returned 403; token refresh required"
                    .into(),
            });
        }
        let mut auth_rejections = 0usize;
        let mut forbidden_rejections = 0usize;
        let attempt_count = order.len();
        for key in order {
            let endpoint = EndpointDefinition::for_account(key, &self.overrides, account)?;
            match self
                .send_generation(account, payload, endpoint.clone())
                .await
            {
                Ok(response) => {
                    self.cache
                        .mark_success(&account.id, EndpointPurpose::Generation, endpoint.key);
                    return Ok(response);
                }
                Err(mut error) => {
                    if error.status == Some(401) {
                        auth_rejections += 1;
                        continue;
                    }
                    if error.status == Some(403) {
                        if error.is_quota() || error.is_throttle() {
                            error.status = Some(429);
                            return Err(error);
                        }
                        self.cache.mark_disabled(
                            &account.id,
                            EndpointPurpose::Generation,
                            endpoint.key,
                        );
                        auth_rejections += 1;
                        forbidden_rejections += 1;
                        continue;
                    }
                    return Err(error);
                }
            }
        }
        if auth_rejections == attempt_count {
            return Err(KiroError {
                status: Some(403),
                endpoint: "all".into(),
                message: if account.credentials.auth_method == AuthMethod::ApiKey {
                    "Kiro rejected the API key; replace or reissue the key"
                } else if forbidden_rejections == attempt_count {
                    "All Kiro endpoints returned 403; token refresh required"
                } else {
                    "All Kiro endpoints returned 401/403; token refresh required"
                }
                .into(),
            });
        }
        Err(KiroError {
            status: None,
            endpoint: "none".into(),
            message: "no usable upstream endpoint".into(),
        })
    }

    /// Checks whether an automatic legacy request can use the native Runtime
    /// for its existing empty-response retry without changing endpoint state.
    #[cfg(feature = "crs")]
    pub fn runtime_isolated_retry_eligible(
        &self,
        account: &Account,
        prepared_payload: &KiroPayload,
        previous_endpoint: EndpointKey,
    ) -> bool {
        let Some(profile) = prepared_payload
            .profile_arn
            .as_deref()
            .map(str::trim)
            .filter(|profile| !profile.is_empty())
        else {
            return false;
        };
        let mut frozen_account = account.clone();
        frozen_account.profile_arn = Some(profile.into());
        self.upstream.preferred_endpoint.is_none()
            && matches!(
                previous_endpoint,
                EndpointKey::Amazonq | EndpointKey::Codewhisperer
            )
            && account.credentials.auth_method != AuthMethod::ApiKey
            && account_api_region(account).is_ok_and(|region| !region.starts_with("us-gov-"))
            && account_api_region(&frozen_account)
                .is_ok_and(|region| !region.starts_with("us-gov-"))
            && !self.cache.is_disabled_for(
                &account.id,
                EndpointPurpose::Generation,
                EndpointKey::Runtime,
            )
    }

    /// Tries the native Runtime once after a legacy endpoint returned an empty
    /// response. This deliberately neither learns endpoint preferences nor
    /// updates endpoint failures; the caller owns the existing retry budget.
    /// `None` means this isolated attempt is not applicable.
    #[cfg(feature = "crs")]
    pub async fn generate_runtime_isolated(
        &self,
        account: &Account,
        prepared_payload: &KiroPayload,
        previous_endpoint: EndpointKey,
    ) -> Option<Result<KiroResponse, KiroError>> {
        if !self.runtime_isolated_retry_eligible(account, prepared_payload, previous_endpoint) {
            return None;
        }
        Some(
            self.generate_at_endpoint_isolated(account, prepared_payload, EndpointKey::Runtime)
                .await,
        )
    }

    /// Sends a prepared attempt to exactly one endpoint without changing
    /// endpoint cache state or trying another endpoint after an error.
    #[cfg(feature = "crs")]
    pub async fn generate_at_endpoint_isolated(
        &self,
        account: &Account,
        prepared_payload: &KiroPayload,
        endpoint: EndpointKey,
    ) -> Result<KiroResponse, KiroError> {
        drop(self.admit_model(prepared_payload).await?);
        let endpoint = EndpointDefinition::for_account(endpoint, &self.overrides, account)?;
        if (account.credentials.auth_method == AuthMethod::ApiKey
            || account_api_region(account)?.starts_with("us-gov-"))
            && endpoint.key != EndpointKey::Runtime
        {
            return Err(KiroError {
                status: Some(400),
                endpoint: "isolated-endpoint-policy".into(),
                message: "This account requires the Kiro Runtime endpoint".into(),
            });
        }
        if self
            .cache
            .is_disabled_for(&account.id, EndpointPurpose::Generation, endpoint.key)
        {
            return Err(KiroError {
                status: Some(400),
                endpoint: "isolated-endpoint-policy".into(),
                message: "The isolated Kiro endpoint is temporarily disabled".into(),
            });
        }
        let mut frozen_account = account.clone();
        if endpoint.key == EndpointKey::Runtime
            && account.credentials.auth_method != AuthMethod::ApiKey
        {
            // Isolated retries must not discover a different profile or issue
            // any control-plane requests. The original prepared attempt
            // supplies the profile retained in this snapshot.
            let govcloud = account_api_region(account)?.starts_with("us-gov-");
            let profile = prepared_payload
                .profile_arn
                .as_deref()
                .map(str::trim)
                .filter(|profile| !profile.is_empty())
                .filter(|profile| {
                    !govcloud || profile.starts_with("arn:aws-us-gov:codewhisperer:us-gov-")
                })
                .ok_or_else(|| KiroError {
                    status: Some(400),
                    endpoint: "isolated-endpoint-policy".into(),
                    message: "The isolated Runtime attempt requires its prepared profile ARN"
                        .into(),
                })?;
            frozen_account.profile_arn = Some(profile.into());
        }
        self.send_generation(&frozen_account, prepared_payload, endpoint)
            .await
            .map_err(|mut error| {
                if error.status == Some(403) && (error.is_quota() || error.is_throttle()) {
                    error.status = Some(429);
                }
                error
            })
    }

    /// Resolves the profile ARN required by Kiro MCP requests. Existing ARN
    /// values and the fixed Social profile do not require a network request;
    /// missing IdC values are discovered once per access token.
    pub async fn resolve_profile_arn(&self, account: &Account) -> Result<String, KiroError> {
        if account.credentials.auth_method == AuthMethod::ApiKey {
            return Err(KiroError {
                status: Some(400),
                endpoint: "ListAvailableProfiles".into(),
                message: "Kiro API keys do not use profile ARNs".into(),
            });
        }
        let govcloud = account_api_region(account)?.starts_with("us-gov-");
        if let Some(profile_arn) = account
            .profile_arn
            .as_deref()
            .map(str::trim)
            .filter(|profile_arn| !profile_arn.is_empty())
            .filter(|profile_arn| {
                !govcloud || profile_arn.starts_with("arn:aws-us-gov:codewhisperer:us-gov-")
            })
        {
            return Ok(profile_arn.to_owned());
        }
        if account.credentials.auth_method == AuthMethod::Social && !govcloud {
            return Ok(KIRO_SOCIAL_PROFILE_ARN.into());
        }

        let fetch_key = (
            account.id.clone(),
            account.credentials.access_token.clone(),
            account_api_region(account)?,
        );
        {
            let mut cache = self
                .profile_cache
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(cached) = cache.get(&fetch_key) {
                if cached.expires > Instant::now() {
                    return Ok(cached.arn.clone());
                }
            }
            cache.pop(&fetch_key);
        }
        let fetch_id = Uuid::new_v4();
        let fetch = match self.profile_fetches.entry(fetch_key.clone()) {
            Entry::Occupied(entry) => entry.get().future.clone(),
            Entry::Vacant(entry) => {
                let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
                let future = async move {
                    result_receiver.await.unwrap_or_else(|_| {
                        Err(KiroError {
                            status: None,
                            endpoint: "ListAvailableProfiles".into(),
                            message: "profile ARN discovery task stopped unexpectedly".into(),
                        })
                    })
                }
                .boxed()
                .shared();
                entry.insert(InFlightProfileFetch {
                    id: fetch_id,
                    future: future.clone(),
                });

                let client = self.clone();
                let account = account.clone();
                let fetches = Arc::clone(&self.profile_fetches);
                tokio::spawn(async move {
                    let result = client.fetch_profile_arn_once(&account).await;
                    if let Ok(arn) = &result {
                        client
                            .profile_cache
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .put(
                                fetch_key.clone(),
                                CachedProfile {
                                    arn: arn.clone(),
                                    expires: Instant::now() + PROFILE_CACHE_TTL,
                                },
                            );
                    }
                    let _sent = result_sender.send(result);
                    fetches.remove_if(&fetch_key, |_, entry| entry.id == fetch_id);
                });
                future
            }
        };
        fetch.await
    }

    async fn fetch_profile_arn_once(&self, account: &Account) -> Result<String, KiroError> {
        tokio::time::timeout(
            Duration::from_secs(10),
            self.fetch_profile_arn_pages(account),
        )
        .await
        .unwrap_or_else(|_| {
            Err(KiroError {
                status: None,
                endpoint: "ListAvailableProfiles".into(),
                message: "profile discovery timed out after 10 seconds".into(),
            })
        })
    }

    async fn fetch_profile_arn_pages(&self, account: &Account) -> Result<String, KiroError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Profiles {
            #[serde(default)]
            profiles: Vec<Profile>,
            #[serde(default)]
            next_token: Option<String>,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Profile {
            #[serde(default)]
            arn: Option<String>,
        }

        let _permit = Arc::clone(&self.short_slots)
            .acquire_owned()
            .await
            .map_err(build_error)?;
        let urls = self.profile_discovery_urls(account)?;
        let govcloud = account_api_region(account)?.starts_with("us-gov-");
        let mut fallback_allowed = false;
        let mut last_error = None;
        for url in urls {
            let mut next_token: Option<String> = None;
            let mut seen_tokens = std::collections::HashSet::new();
            for page in 0..MAX_DISCOVERY_PAGES {
                let mut request = serde_json::json!({});
                if let Some(token) = &next_token {
                    request["nextToken"] = serde_json::json!(token);
                }
                let response = self
                    .short
                    .post(url.clone())
                    .headers(profile_headers(account)?)
                    .json(&request)
                    .timeout(Duration::from_secs(10))
                    .send()
                    .await
                    .map_err(|error| KiroError {
                        status: None,
                        endpoint: "ListAvailableProfiles".into(),
                        message: error.to_string(),
                    })?;
                let status = response.status();
                let body =
                    bounded_response_text(response, 1024 * 1024, "ListAvailableProfiles").await?;
                if status.is_success() {
                    let profiles =
                        serde_json::from_str::<Profiles>(&body).map_err(|error| KiroError {
                            status: Some(502),
                            endpoint: "ListAvailableProfiles".into(),
                            message: format!("invalid profile discovery response: {error}"),
                        })?;
                    if let Some(profile_arn) = profiles
                        .profiles
                        .into_iter()
                        .filter_map(|profile| profile.arn)
                        .map(|profile_arn| profile_arn.trim().to_owned())
                        .find(|profile_arn| {
                            profile_arn.starts_with("arn:")
                                && profile_arn.len() <= 2_048
                                && (!govcloud
                                    || profile_arn
                                        .starts_with("arn:aws-us-gov:codewhisperer:us-gov-"))
                        })
                    {
                        debug!(
                            account = %account.id,
                            endpoint = %url,
                            "Kiro profile ARN discovered"
                        );
                        return Ok(profile_arn);
                    }
                    if let Some(token) = profiles.next_token.filter(|token| !token.is_empty()) {
                        if page + 1 == MAX_DISCOVERY_PAGES || !seen_tokens.insert(token.clone()) {
                            return Err(KiroError { status: Some(502), endpoint: "ListAvailableProfiles".into(), message: "profile discovery exceeded its pagination limit or repeated a page token".into() });
                        }
                        next_token = Some(token);
                        continue;
                    }
                    fallback_allowed = true;
                    break;
                }

                let error = KiroError {
                    status: Some(status.as_u16()),
                    endpoint: "ListAvailableProfiles".into(),
                    message: nonempty_error_body(status.as_u16(), &body),
                };
                if status.as_u16() == 403
                    && !error.is_quota()
                    && !error.is_throttle()
                    && !text_is_auth_error(&error.message)
                {
                    // Builder ID accounts cannot list Enterprise profiles. Kiro's
                    // own clients use the fixed Builder ID profile in this case.
                    fallback_allowed = true;
                    last_error = Some(error);
                    break;
                }
                return Err(error);
            }
        }

        if fallback_allowed && !govcloud {
            debug!(account = %account.id, "using Kiro Builder ID profile ARN");
            return Ok(KIRO_BUILDER_ID_PROFILE_ARN.into());
        }
        Err(last_error.unwrap_or_else(|| KiroError {
            status: Some(502),
            endpoint: "ListAvailableProfiles".into(),
            message: "profile discovery returned no usable profile ARN".into(),
        }))
    }

    fn profile_discovery_urls(&self, account: &Account) -> Result<Vec<url::Url>, KiroError> {
        self.management_bases(account)?
            .iter()
            .map(|base| operation_url(base, "ListAvailableProfiles"))
            .collect()
    }

    fn management_bases(&self, account: &Account) -> Result<Vec<String>, KiroError> {
        let region = account_api_region(account)?;
        if let Some(endpoint) = self
            .overrides
            .management_url
            .as_deref()
            .or(self.overrides.amazonq_url.as_deref())
        {
            return Ok(vec![endpoint.replace("{region}", &region)]);
        }
        Ok(usage_api_regions(account)
            .iter()
            .map(|region| management_base(region))
            .collect())
    }

    /// Executes Kiro's JSON-RPC MCP web search. The nested MCP text block is
    /// JSON itself, so both envelope layers are validated before results are
    /// exposed to the response/continuation pipeline.
    pub async fn web_search(
        &self,
        account: &Account,
        query: &str,
    ) -> Result<WebSearchResults, KiroError> {
        self.web_search_with_redaction(account, query, &Default::default())
            .await
    }

    /// Keep secrets learned by the owning generation private when its restored
    /// tool query is sent back to Kiro's separately authenticated MCP endpoint.
    pub async fn web_search_with_redaction(
        &self,
        account: &Account,
        query: &str,
        redaction_session: &crosslane_translate::RedactionSession,
    ) -> Result<WebSearchResults, KiroError> {
        let query = query.trim();
        if query.is_empty() {
            return Err(KiroError {
                status: Some(400),
                endpoint: "MCP web_search".into(),
                message: "web search query must not be empty".into(),
            });
        }
        if query.chars().count() > 2_000 {
            return Err(KiroError {
                status: Some(400),
                endpoint: "MCP web_search".into(),
                message: "web search query exceeds 2000 characters".into(),
            });
        }
        let query = redaction_session.redact_text(query, &content_secrets(account));
        if query.chars().count() > 2_000 {
            return Err(KiroError {
                status: Some(400),
                endpoint: "MCP web_search".into(),
                message: "redacted web search query exceeds 2000 characters".into(),
            });
        }
        let _permit = Arc::clone(&self.short_slots)
            .acquire_owned()
            .await
            .map_err(build_error)?;
        let url = self.web_search_url(account)?;
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let mut request = serde_json::json!({
            "id":format!("web_search_tooluse_{}_{}", Uuid::new_v4().simple(), timestamp),
            "jsonrpc":"2.0",
            "method":"tools/call",
            "params":{"name":"web_search","arguments":{"query":query}}
        });
        let native =
            self.overrides.mcp_url.is_none() && self.upstream.web_search_endpoint.is_none();
        if native && account.credentials.auth_method != AuthMethod::ApiKey {
            let profile = account
                .profile_arn
                .as_deref()
                .filter(|profile| !profile.trim().is_empty())
                .ok_or_else(|| KiroError {
                    status: Some(400),
                    endpoint: "MCP web_search".into(),
                    message: "account profile ARN was not resolved before web search".into(),
                })?;
            request["profileArn"] = serde_json::json!(profile);
        }
        let response = self
            .mcp
            .post(url.clone())
            .headers(mcp_headers(account, native)?)
            .json(&request)
            .send()
            .await
            .map_err(|error| KiroError {
                status: None,
                endpoint: "MCP web_search".into(),
                message: error.to_string(),
            })?;
        let status = response.status();
        let body = bounded_response_text(response, 8 * 1024 * 1024, "MCP web_search").await?;
        if !status.is_success() {
            return Err(KiroError {
                status: Some(status.as_u16()),
                endpoint: "MCP web_search".into(),
                message: nonempty_error_body(status.as_u16(), &body),
            });
        }
        let envelope: serde_json::Value =
            serde_json::from_str(&body).map_err(|error| KiroError {
                status: Some(502),
                endpoint: "MCP web_search".into(),
                message: format!("invalid JSON-RPC response: {error}"),
            })?;
        if let Some(error) = envelope.get("error").filter(|error| !error.is_null()) {
            return Err(KiroError {
                status: Some(502),
                endpoint: "MCP web_search".into(),
                message: format!(
                    "JSON-RPC error: {}",
                    truncate_chars(&error.to_string(), 1_000)
                ),
            });
        }
        if envelope
            .pointer("/result/isError")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            return Err(KiroError {
                status: Some(502),
                endpoint: "MCP web_search".into(),
                message: "Kiro MCP web_search returned an error result".into(),
            });
        }
        let content = envelope
            .pointer("/result/content")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| KiroError {
                status: Some(502),
                endpoint: "MCP web_search".into(),
                message: "MCP response is missing result.content".into(),
            })?;
        let mut parsed = None;
        for block in content {
            if block.get("type").and_then(serde_json::Value::as_str) != Some("text") {
                continue;
            }
            let Some(text) = block.get("text").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if let Ok(results) = serde_json::from_str::<WebSearchResults>(text) {
                parsed = Some(results);
                break;
            }
        }
        let mut results = parsed.ok_or_else(|| KiroError {
            status: Some(502),
            endpoint: "MCP web_search".into(),
            message: "MCP response contains no valid web search JSON text block".into(),
        })?;
        if results.query.is_empty() {
            results.query = query.to_owned();
        }
        // Keep the local result usable by its owning generation. Its shared
        // session masks the restored query again on every later trip to Kiro.
        results.query = redaction_session.restore_text(&results.query);
        sanitize_web_search_results(&mut results);
        debug!(
            account = %account.id,
            endpoint = %url,
            query_chars = query.chars().count(),
            result_count = results.results.len(),
            total_results = results.total_results,
            "Kiro MCP web search completed"
        );
        Ok(results)
    }

    fn web_search_url(&self, account: &Account) -> Result<url::Url, KiroError> {
        let api_region = account_api_region(account)?;
        let region = api_region.as_str();
        if region.is_empty()
            || !region
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '-')
        {
            return Err(KiroError {
                status: None,
                endpoint: "MCP web_search".into(),
                message: "account has an invalid API region".into(),
            });
        }
        let template = self
            .overrides
            .mcp_url
            .as_deref()
            .or(self.upstream.web_search_endpoint.as_deref())
            .or(self.overrides.runtime_url.as_deref())
            .unwrap_or("https://runtime.{region}.kiro.dev/");
        let url = template.replace("{region}", region);
        let parsed = url::Url::parse(&url).map_err(build_error)?;
        if !matches!(parsed.scheme(), "http" | "https")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(KiroError {
                status: None,
                endpoint: "MCP web_search".into(),
                message: "web search endpoint must be an HTTP(S) URL without credentials".into(),
            });
        }
        Ok(parsed)
    }

    async fn send_generation(
        &self,
        account: &Account,
        payload: &KiroPayload,
        mut endpoint: EndpointDefinition,
    ) -> Result<KiroResponse, KiroError> {
        // Protocol capabilities are resolved before dispatch. A 400 describes
        // this request, not temporary account health: do not probe by removing
        // fields or learn time-limited capabilities from error message text.
        let mut payload = payload.clone();
        if endpoint.key == EndpointKey::Runtime {
            if account.credentials.auth_method == AuthMethod::ApiKey {
                // API-key traffic uses the existing CLI contract. The supplied
                // IDE report does not establish a replacement for that client.
                payload.conversation_state.agent_continuation_id = None;
                payload.profile_arn = None;
            } else {
                payload.profile_arn = Some(self.resolve_profile_arn(account).await?);
            }
            let mut routed_account = account.clone();
            routed_account.profile_arn.clone_from(&payload.profile_arn);
            endpoint =
                EndpointDefinition::for_account(endpoint.key, &self.overrides, &routed_account)?;
            if account.credentials.auth_method != AuthMethod::ApiKey {
                endpoint.origin = "AI_EDITOR";
                endpoint.amz_target = "KiroRuntimeService.GenerateAssistantResponse";
            }
        }
        set_payload_origin(&mut payload, endpoint.origin);
        let redaction_session = payload.redaction_session.clone().unwrap_or_default();
        payload.redaction_session = Some(redaction_session.clone());
        // Keep real context private for endpoint retries. Each outgoing copy is
        // masked; tools are restored and checkpoints made replayable locally.
        #[cfg(feature = "crs")]
        let prepared_payload = payload.clone();
        payload =
            crosslane_translate::prepare_redacted_kiro_payload(&payload, &content_secrets(account));
        validate_kiro_tool_history(&payload).map_err(|message| KiroError {
            status: Some(400),
            endpoint: endpoint.name.into(),
            message: format!(
                "Kiro payload reached the transport without prepared tool history: {message}"
            ),
        })?;
        let slot_wait_started = Instant::now();
        let permit = tokio::time::timeout(
            Duration::from_millis(self.upstream.stream_slot_wait_timeout_ms),
            Arc::clone(&self.stream_slots).acquire_owned(),
        )
        .await
        .map_err(|_| KiroError::stream_slot_timeout(self.upstream.stream_slot_wait_timeout_ms))?
        .map_err(build_error)?;
        let stream_slot_wait_ms = slot_wait_started.elapsed().as_millis() as u64;
        let mut wire = serde_json::to_value(&payload).map_err(build_error)?;
        if endpoint.key == EndpointKey::Runtime
            && account.credentials.auth_method != AuthMethod::ApiKey
        {
            wire["agentMode"] = serde_json::json!(match self.upstream.agent_mode {
                AgentMode::Auto | AgentMode::Vibe => "vibe",
                AgentMode::Spec => "spec",
            });
            wire["conversationState"]["agentTaskType"] =
                serde_json::json!(match self.upstream.agent_mode {
                    AgentMode::Auto | AgentMode::Vibe => "vibe",
                    AgentMode::Spec => "spectask",
                });
            wire["conversationState"]["rootConversationId"] =
                serde_json::json!(payload.conversation_state.conversation_id);
        }
        if endpoint.key == EndpointKey::Runtime {
            if let Some(history) = wire
                .pointer_mut("/conversationState/history")
                .and_then(serde_json::Value::as_array_mut)
            {
                for item in history {
                    if let Some(user) = item
                        .get_mut("userInputMessage")
                        .and_then(serde_json::Value::as_object_mut)
                    {
                        user.remove("modelId");
                    }
                }
            }
        }
        let model_permit = self.admit_model(&payload).await?;
        let response = self
            .stream
            .post(&endpoint.url)
            .headers(headers(account, &endpoint, self.upstream.agent_mode)?)
            .json(&wire)
            .send()
            .await
            .map_err(|error| KiroError {
                status: None,
                endpoint: endpoint.name.into(),
                message: error.to_string(),
            })?;
        drop(model_permit);
        if response.status().is_success() {
            let mut response = response;
            response.extensions_mut().insert(redaction_session.clone());
            let response = crate::response_redaction::restore_tool_response(
                response,
                redaction_session.clone(),
                payload.restore_summary_secrets,
            );
            return Ok(KiroResponse {
                endpoint,
                response,
                permit,
                stream_slot_wait_ms,
                thinking_enabled: payload.thinking_enabled(),
                redaction_session,
                #[cfg(feature = "crs")]
                prepared_payload,
            });
        }
        Err(response_error(response, &endpoint).await)
    }

    pub async fn collect_events(&self, response: Response) -> Result<Vec<KiroEvent>, KiroError> {
        let mut source = response.bytes_stream();
        let mut buffer = bytes::BytesMut::new();
        let mut decoder = EventStreamDecoder;
        let mut output = Vec::new();
        while let Some(chunk) = source.next().await {
            let chunk = chunk.map_err(build_error)?;
            buffer.extend_from_slice(&chunk);
            while let Some(event) = decoder.decode(&mut buffer).map_err(build_error)? {
                if let KiroEvent::Error { kind, message } = &event {
                    return Err(KiroError {
                        status: None,
                        endpoint: "event-stream".into(),
                        message: format!("{kind}: {message}"),
                    });
                }
                output.push(event);
            }
        }
        while let Some(event) = decoder.decode_eof(&mut buffer).map_err(build_error)? {
            if let KiroEvent::Error { kind, message } = &event {
                return Err(KiroError {
                    status: None,
                    endpoint: "event-stream".into(),
                    message: format!("{kind}: {message}"),
                });
            }
            output.push(event);
        }
        Ok(output)
    }

    pub fn event_stream(
        response: Response,
    ) -> impl Stream<Item = Result<Bytes, reqwest::Error>> + Send {
        response.bytes_stream()
    }

    pub async fn get_usage_limits(&self, account: &Account) -> Result<UsageLimits, KiroError> {
        let _permit = Arc::clone(&self.short_slots)
            .acquire_owned()
            .await
            .map_err(build_error)?;
        let urls = self.usage_limits_urls(account)?;
        self.get_usage_limits_from_urls(account, urls).await
    }

    async fn get_usage_limits_from_urls(
        &self,
        account: &Account,
        urls: Vec<url::Url>,
    ) -> Result<UsageLimits, KiroError> {
        let attempt_count = urls.len();
        let mut last_error = None;
        for (attempt, url) in urls.into_iter().enumerate() {
            let endpoint = usage_endpoint_label(&url);
            // RequestBuilder::header appends; keep auth and SDK singleton
            // headers in the shared map instead of adding them a second time.
            let response = self
                .short
                .get(url)
                .headers(profile_headers(account)?)
                .send()
                .await
                .map_err(|error| KiroError {
                    status: None,
                    endpoint: endpoint.clone(),
                    message: error.to_string(),
                })?;
            let status = response.status();
            let body = bounded_response_text(response, 1024 * 1024, &endpoint).await?;
            if status.is_success() {
                return serde_json::from_str::<UsageLimits>(&body).map_err(|error| KiroError {
                    status: Some(502),
                    endpoint,
                    message: format!("invalid usage limits response: {error}"),
                });
            }
            let error = KiroError {
                status: Some(status.as_u16()),
                endpoint: endpoint.clone(),
                message: nonempty_error_body(status.as_u16(), &body),
            };
            if status.as_u16() == 403 && attempt + 1 < attempt_count {
                debug!(
                    account = %account.id,
                    endpoint = %endpoint,
                    "Kiro usage endpoint rejected the account; trying the regional fallback"
                );
                last_error = Some(error);
                continue;
            }
            return Err(error);
        }
        Err(last_error.unwrap_or_else(|| KiroError {
            status: None,
            endpoint: "getUsageLimits".into(),
            message: "no usable Kiro usage endpoint".into(),
        }))
    }

    fn usage_limits_urls(&self, account: &Account) -> Result<Vec<url::Url>, KiroError> {
        self.management_bases(account)?
            .iter()
            .map(|base| usage_limits_url(base, account))
            .collect()
    }

    pub async fn list_models(&self, account: &Account) -> Result<Vec<ModelInfo>, KiroError> {
        // Kiro's management catalog requires an OAuth profile ARN, which API
        // keys do not have. Do not mark a valid generation key as unauthorized.
        if account.credentials.auth_method == AuthMethod::ApiKey {
            return Ok(crate::catalog::static_models_for_account(account));
        }
        let fetch_key = (account.id.clone(), account.credentials.access_token.clone());
        let fetch_id = Uuid::new_v4();
        let fetch = match self.model_fetches.entry(fetch_key.clone()) {
            Entry::Occupied(entry) => entry.get().future.clone(),
            Entry::Vacant(entry) => {
                let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
                let future = async move {
                    result_receiver.await.unwrap_or_else(|_| {
                        Err(KiroError {
                            status: None,
                            endpoint: "models".into(),
                            message: "model discovery task stopped unexpectedly".into(),
                        })
                    })
                }
                .boxed()
                .shared();
                entry.insert(InFlightModelFetch {
                    id: fetch_id,
                    future: future.clone(),
                });

                let client = self.clone();
                let account = account.clone();
                let fetches = Arc::clone(&self.model_fetches);
                tokio::spawn(async move {
                    let result = client.fetch_models_once(&account).await;
                    let _sent = result_sender.send(result);
                    fetches.remove_if(&fetch_key, |_, entry| entry.id == fetch_id);
                });
                future
            }
        };
        match fetch.await {
            Ok(models) => Ok(models),
            Err(error) => {
                let models = crate::catalog::static_models_for_account(account);
                if models.is_empty() {
                    return Err(error);
                }
                warn!(
                    account = %account.id,
                    endpoint = %error.endpoint,
                    status = error.status.unwrap_or_default(),
                    fallback_models = models.len(),
                    "dynamic model discovery failed; using the subscription-safe static catalog"
                );
                Ok(models)
            }
        }
    }

    async fn fetch_models_once(&self, account: &Account) -> Result<Vec<ModelInfo>, KiroError> {
        let endpoint_revision = self.cache.revision(&account.id);
        let native_default = (self.overrides.management_url.is_some()
            || (self.overrides.amazonq_url.is_none()
                && self.overrides.codewhisperer_url.is_none()))
        .then_some(EndpointKey::Runtime);
        let order = self.cache.order(
            account,
            self.upstream
                .preferred_endpoint
                .map(EndpointKey::from)
                .or(native_default),
            EndpointPurpose::Models,
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let mut last_error = None;
        'endpoints: for key in order {
            let endpoint = EndpointDefinition::for_account(key, &self.overrides, account)?;
            let mut collected = Vec::new();
            let mut next_token: Option<String> = None;
            let mut seen_tokens = std::collections::HashSet::new();
            for page in 0..MAX_DISCOVERY_PAGES {
                let request = if key == EndpointKey::Runtime {
                    let profile =
                        match tokio::time::timeout_at(deadline, self.resolve_profile_arn(account))
                            .await
                        {
                            Ok(Ok(profile)) => profile,
                            Ok(Err(error)) => {
                                last_error = Some(error);
                                continue 'endpoints;
                            }
                            Err(_) => {
                                return Err(KiroError {
                                    status: None,
                                    endpoint: endpoint.name.into(),
                                    message: "ListAvailableModels timed out resolving its profile"
                                        .into(),
                                })
                            }
                        };
                    let mut routed_account = account.clone();
                    routed_account.profile_arn = Some(profile.clone());
                    let region = account_api_region(&routed_account)?;
                    if self.overrides.management_url.is_none()
                        && !supports_native_management(&region)
                    {
                        // The legacy regional host uses REST. Do not send a
                        // KiroControlPlane RPC target to the Amazon Q root.
                        last_error = Some(KiroError {
                            status: None,
                            endpoint: endpoint.name.into(),
                            message: "native model catalog is not configured for this Kiro region"
                                .into(),
                        });
                        continue 'endpoints;
                    }
                    let url = self
                        .overrides
                        .management_url
                        .clone()
                        .unwrap_or_else(|| management_base(&region))
                        .replace("{region}", &region);
                    let mut body = serde_json::json!({"origin":"AI_EDITOR", "profileArn":profile});
                    if let Some(token) = &next_token {
                        body["nextToken"] = serde_json::json!(token);
                    }
                    self.short
                        .post(url)
                        .headers(native_rpc_headers(
                            account,
                            IdeService::ControlPlane,
                            "KiroControlPlaneBearerService.ListAvailableModels",
                        )?)
                        .json(&body)
                } else {
                    let mut url = models_url(&endpoint.url, account)?;
                    if let Some(token) = &next_token {
                        url.query_pairs_mut().append_pair("nextToken", token);
                    }
                    self.short.get(url).headers(metadata_headers(
                        account,
                        &endpoint,
                        self.upstream.agent_mode,
                    )?)
                };
                let _permit = tokio::time::timeout_at(
                    deadline,
                    Arc::clone(&self.short_slots).acquire_owned(),
                )
                .await
                .map_err(|_| KiroError {
                    status: None,
                    endpoint: endpoint.name.into(),
                    message: "ListAvailableModels timed out waiting for a short-request slot"
                        .into(),
                })?
                .map_err(build_error)?;
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                let response = match tokio::time::timeout_at(
                    deadline,
                    request.timeout(remaining).send(),
                )
                .await
                {
                    Ok(Ok(response)) => response,
                    Ok(Err(error)) => return Err(build_error(error)),
                    Err(_) => {
                        return Err(KiroError {
                            status: None,
                            endpoint: endpoint.name.into(),
                            message: "ListAvailableModels timed out after 15 seconds".into(),
                        });
                    }
                };
                if response.status().is_success() {
                    #[derive(Deserialize)]
                    #[serde(rename_all = "camelCase")]
                    struct Models {
                        #[serde(default)]
                        models: Vec<ModelInfo>,
                        #[serde(default)]
                        next_token: Option<String>,
                    }
                    let text =
                        bounded_response_text(response, 8 * 1024 * 1024, "ListAvailableModels")
                            .await?;
                    let response = serde_json::from_str::<Models>(&text).map_err(build_error)?;
                    collected.extend(response.models);
                    if let Some(token) = response.next_token.filter(|token| !token.is_empty()) {
                        if page + 1 == MAX_DISCOVERY_PAGES || !seen_tokens.insert(token.clone()) {
                            return Err(KiroError { status: Some(502), endpoint: endpoint.name.into(), message: "model discovery exceeded its pagination limit or repeated a page token".into() });
                        }
                        next_token = Some(token);
                        continue;
                    }
                    let models = collected;
                    if !models.is_empty() {
                        self.cache.mark_success_if_revision(
                            &account.id,
                            EndpointPurpose::Models,
                            key,
                            endpoint_revision,
                        );
                        return Ok(models);
                    }
                    last_error = Some(KiroError {
                        status: None,
                        endpoint: endpoint.name.into(),
                        message: "upstream returned an empty model list".into(),
                    });
                    continue 'endpoints;
                } else {
                    let mut error = response_error(response, &endpoint).await;
                    match error.status {
                        Some(401) => {
                            last_error = Some(error);
                            continue 'endpoints;
                        }
                        Some(403) if error.is_quota() || error.is_throttle() => {
                            error.status = Some(429);
                            return Err(error);
                        }
                        Some(403) => {
                            self.cache.mark_disabled_if_revision(
                                &account.id,
                                EndpointPurpose::Models,
                                key,
                                endpoint_revision,
                            );
                            last_error = Some(error);
                            continue 'endpoints;
                        }
                        Some(404 | 405) if key == EndpointKey::Runtime => {
                            self.cache.mark_disabled_if_revision(
                                &account.id,
                                EndpointPurpose::Models,
                                key,
                                endpoint_revision,
                            );
                            last_error = Some(error);
                            continue 'endpoints;
                        }
                        _ => return Err(error),
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| KiroError {
            status: None,
            endpoint: "models".into(),
            message: "upstream returned no models".into(),
        }))
    }

    pub async fn list_subscriptions(
        &self,
        account: &Account,
    ) -> Result<SubscriptionList, KiroError> {
        let _permit = Arc::clone(&self.short_slots)
            .acquire_owned()
            .await
            .map_err(build_error)?;
        let base = self
            .management_bases(account)?
            .into_iter()
            .next()
            .ok_or_else(|| KiroError {
                status: Some(400),
                endpoint: "subscriptions".into(),
                message: "no management endpoint".into(),
            })?;
        let endpoint = EndpointDefinition {
            key: EndpointKey::Amazonq,
            url: base.clone(),
            origin: "AI_EDITOR",
            amz_target: "",
            name: "KiroManagement",
        };
        let response = match self
            .short
            .post(operation_url(&base, "listAvailableSubscriptions")?)
            .headers(profile_headers(account)?)
            .json(&serde_json::json!({}))
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(
                    account = %account.id,
                    endpoint = endpoint.name,
                    %error,
                    "subscription discovery failed; returning an empty result"
                );
                return Ok(SubscriptionList::default());
            }
        };
        if !response.status().is_success() {
            tracing::warn!(
                account = %account.id,
                endpoint = endpoint.name,
                status = %response.status(),
                "subscription discovery was rejected; returning an empty result"
            );
            return Ok(SubscriptionList::default());
        }
        match response.json::<SubscriptionList>().await {
            Ok(subscriptions) => Ok(subscriptions),
            Err(error) => {
                tracing::warn!(
                    account = %account.id,
                    endpoint = endpoint.name,
                    %error,
                    "subscription response was invalid; returning an empty result"
                );
                Ok(SubscriptionList::default())
            }
        }
    }
}

fn set_payload_origin(payload: &mut KiroPayload, origin: &str) {
    payload
        .conversation_state
        .current_message
        .user_input_message
        .origin = origin.into();
    for message in &mut payload.conversation_state.history {
        if let Some(user) = &mut message.user_input_message {
            user.origin = origin.into();
        }
    }
}

fn content_secrets(account: &Account) -> [&str; 3] {
    [
        account.credentials.access_token.as_str(),
        account.credentials.refresh_token.as_deref().unwrap_or(""),
        account.credentials.client_secret.as_deref().unwrap_or(""),
    ]
}

fn headers(
    account: &Account,
    endpoint: &EndpointDefinition,
    mode: AgentMode,
) -> Result<reqwest::header::HeaderMap, KiroError> {
    if endpoint.key == EndpointKey::Runtime && account.credentials.auth_method != AuthMethod::ApiKey
    {
        return native_rpc_headers(
            account,
            IdeService::Runtime,
            "KiroRuntimeService.GenerateAssistantResponse",
        );
    }
    use reqwest::header::{
        HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT,
    };
    let mut headers = HeaderMap::new();
    let cli = matches!(endpoint.origin, "CLI" | "KIRO_CLI");
    let user_agent = if cli {
        cli_user_agent(CliService::Streaming)
    } else {
        ide_user_agent(&account.machine_id)
    };
    let amz_user_agent = if cli {
        cli_amz_user_agent(CliService::Streaming)
    } else {
        ide_amz_user_agent(&account.machine_id)
    };
    let mode = match mode {
        AgentMode::Auto if cli => "vibe",
        AgentMode::Auto => "spec",
        AgentMode::Vibe => "vibe",
        AgentMode::Spec => "spec",
    };
    let authorization = format!("Bearer {}", account.credentials.access_token);
    let pairs = [
        (
            CONTENT_TYPE,
            if cli {
                "application/x-amz-json-1.0"
            } else {
                "application/json"
            },
        ),
        (ACCEPT, "*/*"),
        (USER_AGENT, &user_agent),
        (AUTHORIZATION, &authorization),
    ];
    for (name, value) in pairs {
        headers.insert(name, HeaderValue::from_str(value).map_err(build_error)?);
    }
    headers.extend(token_type_headers(account));
    let invocation_id = Uuid::new_v4().to_string();
    if !endpoint.amz_target.is_empty() {
        headers.insert(
            reqwest::header::HeaderName::from_static("x-amz-target"),
            HeaderValue::from_static(endpoint.amz_target),
        );
    }
    for (name, value) in [
        ("x-amz-user-agent", amz_user_agent.as_str()),
        ("x-amzn-kiro-agent-mode", mode),
        ("x-amzn-codewhisperer-optout", "true"),
        ("amz-sdk-request", "attempt=1; max=3"),
        ("amz-sdk-invocation-id", invocation_id.as_str()),
    ] {
        headers.insert(
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(build_error)?,
            HeaderValue::from_str(value).map_err(build_error)?,
        );
    }
    debug!(account = %account.id, endpoint = endpoint.name, auth = ?account.credentials.auth_method, "built upstream headers");
    Ok(headers)
}

fn native_rpc_headers(
    account: &Account,
    service: IdeService,
    target: &'static str,
) -> Result<reqwest::header::HeaderMap, KiroError> {
    use reqwest::header::HeaderValue;
    let mut headers = profile_headers(account)?;
    headers.insert(
        "content-type",
        HeaderValue::from_static("application/x-amz-json-1.0"),
    );
    headers.insert("accept", HeaderValue::from_static("*/*"));
    headers.insert("x-amz-target", HeaderValue::from_static(target));
    headers.insert(
        "user-agent",
        HeaderValue::from_str(&ide_service_user_agent(service, &account.machine_id))
            .map_err(build_error)?,
    );
    headers.insert(
        "x-amz-user-agent",
        HeaderValue::from_str(&ide_service_amz_user_agent(service, &account.machine_id))
            .map_err(build_error)?,
    );
    if account.credentials.auth_method == AuthMethod::Idc {
        headers.insert("tokentype", HeaderValue::from_static("SSO_OIDC"));
    }
    headers.insert(
        "x-amzn-codewhisperer-optout",
        HeaderValue::from_static("true"),
    );
    if matches!(service, IdeService::Runtime) {
        headers.insert("x-kiro-attempt", HeaderValue::from_static("1;max=1"));
    }
    Ok(headers)
}

fn mcp_headers(account: &Account, native: bool) -> Result<reqwest::header::HeaderMap, KiroError> {
    if native {
        return native_rpc_headers(account, IdeService::Runtime, "KiroRuntimeService.InvokeMCP");
    }
    use reqwest::header::{
        HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT,
    };

    let mut headers = HeaderMap::new();
    let authorization = format!("Bearer {}", account.credentials.access_token);
    let user_agent = ide_user_agent(&account.machine_id);
    for (name, value) in [
        (CONTENT_TYPE, "application/json"),
        (ACCEPT, "application/json"),
        (USER_AGENT, user_agent.as_str()),
        (AUTHORIZATION, authorization.as_str()),
    ] {
        headers.insert(name, HeaderValue::from_str(value).map_err(build_error)?);
    }
    headers.insert(
        reqwest::header::HeaderName::from_static("x-amzn-codewhisperer-optout"),
        HeaderValue::from_static("true"),
    );
    headers.extend(token_type_headers(account));
    let profile_arn = account
        .profile_arn
        .as_deref()
        .map(str::trim)
        .filter(|profile_arn| !profile_arn.is_empty());
    if let Some(profile_arn) =
        profile_arn.filter(|_| account.credentials.auth_method != AuthMethod::ApiKey)
    {
        headers.insert(
            reqwest::header::HeaderName::from_static("x-amzn-kiro-profile-arn"),
            HeaderValue::from_str(profile_arn).map_err(build_error)?,
        );
    } else if account.credentials.auth_method != AuthMethod::ApiKey {
        return Err(KiroError {
            status: None,
            endpoint: "MCP web_search".into(),
            message: "account profile ARN was not resolved before web search".into(),
        });
    }
    headers.insert(
        reqwest::header::HeaderName::from_static("x-amz-user-agent"),
        HeaderValue::from_str(&ide_amz_user_agent(&account.machine_id)).map_err(build_error)?,
    );
    headers.insert(
        reqwest::header::HeaderName::from_static("amz-sdk-invocation-id"),
        HeaderValue::from_str(&Uuid::new_v4().to_string()).map_err(build_error)?,
    );
    headers.insert(
        reqwest::header::HeaderName::from_static("amz-sdk-request"),
        HeaderValue::from_static("attempt=1; max=3"),
    );
    Ok(headers)
}

fn profile_headers(account: &Account) -> Result<reqwest::header::HeaderMap, KiroError> {
    use reqwest::header::{
        HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT,
    };

    let mut headers = HeaderMap::new();
    let authorization = format!("Bearer {}", account.credentials.access_token);
    let user_agent = ide_service_user_agent(IdeService::Management, &account.machine_id);
    let amz_user_agent = ide_service_amz_user_agent(IdeService::Management, &account.machine_id);
    for (name, value) in [
        (CONTENT_TYPE, "application/json"),
        (ACCEPT, "application/json"),
        (USER_AGENT, user_agent.as_str()),
        (AUTHORIZATION, authorization.as_str()),
    ] {
        headers.insert(name, HeaderValue::from_str(value).map_err(build_error)?);
    }
    headers.extend(token_type_headers(account));
    for (name, value) in [
        ("x-amz-user-agent", amz_user_agent),
        ("amz-sdk-invocation-id", Uuid::new_v4().to_string()),
        ("amz-sdk-request", "attempt=1; max=1".into()),
    ] {
        headers.insert(
            reqwest::header::HeaderName::from_static(name),
            HeaderValue::from_str(&value).map_err(build_error)?,
        );
    }
    Ok(headers)
}

fn metadata_headers(
    account: &Account,
    endpoint: &EndpointDefinition,
    mode: AgentMode,
) -> Result<reqwest::header::HeaderMap, KiroError> {
    let mut output = headers(account, endpoint, mode)?;
    output.remove("x-amz-target");
    Ok(output)
}

async fn response_error(response: Response, endpoint: &EndpointDefinition) -> KiroError {
    let status = response.status();
    let retry_after = response
        .headers()
        .get("x-amzn-kiro-ratelimit-retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .or_else(|| {
            response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .and_then(|seconds| seconds.checked_mul(1_000))
        });
    let text = bounded_response_text(response, 1024 * 1024, endpoint.name)
        .await
        .unwrap_or_else(|_| status.to_string());
    let text = if text.trim().is_empty() {
        format!(
            "upstream returned HTTP {} without an error message",
            status.as_u16()
        )
    } else {
        text
    };
    let mut structured = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .filter(|value| value.is_object());
    if structured.is_none() && retry_after.is_some() {
        structured = Some(serde_json::json!({"message":truncate_chars(&text, 500)}));
    }
    let text = if let Some(mut value) = structured {
        let delay = retry_after.or_else(|| {
            value
                .get("retryAfterMilliseconds")
                .and_then(serde_json::Value::as_u64)
        });
        if value.to_string().chars().count() > 1_500 {
            // Truncating raw JSON can discard its reason or retry hint. Bound
            // diagnostic fields first so classification still sees valid JSON.
            let mut bounded = serde_json::json!({});
            for field in ["message", "Message", "__type", "_type", "reason"] {
                if let Some(text) = value.get(field).and_then(serde_json::Value::as_str) {
                    let limit = if field.eq_ignore_ascii_case("message") {
                        250
                    } else {
                        100
                    };
                    let text: String = text
                        .chars()
                        .take(limit)
                        .map(|ch| if ch.is_control() { ' ' } else { ch })
                        .collect();
                    bounded[field] = serde_json::json!(text);
                }
            }
            value = bounded;
        }
        if let Some(delay) = delay {
            value["retryAfterMilliseconds"] = serde_json::json!(delay.min(300_000));
        }
        value.to_string()
    } else {
        text
    };
    KiroError {
        status: Some(status.as_u16()),
        endpoint: endpoint.name.into(),
        message: text.chars().take(2_000).collect(),
    }
}

fn nonempty_error_body(status: u16, body: &str) -> String {
    if body.trim().is_empty() {
        format!("upstream returned HTTP {status} without an error message")
    } else {
        truncate_chars(body, 2_000)
    }
}

fn operation_url(endpoint: &str, operation: &str) -> Result<url::Url, KiroError> {
    let mut url = url::Url::parse(endpoint).map_err(build_error)?;
    let current_path = url.path().trim_end_matches('/');
    let base_path = current_path
        .strip_suffix("/generateAssistantResponse")
        .unwrap_or(current_path);
    let path = if base_path.is_empty() {
        format!("/{operation}")
    } else {
        format!("{base_path}/{operation}")
    };
    url.set_path(&path);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn usage_limits_url(endpoint: &str, account: &Account) -> Result<url::Url, KiroError> {
    let mut url = operation_url(endpoint, "getUsageLimits")?;
    let mut query = url.query_pairs_mut();
    query.append_pair("origin", "AI_EDITOR");
    query.append_pair("resourceType", "AGENTIC_REQUEST");
    query.append_pair("isEmailRequired", "true");
    if let Some(profile_arn) = usage_profile_arn(account) {
        query.append_pair("profileArn", profile_arn);
    }
    drop(query);
    Ok(url)
}

fn models_url(endpoint: &str, account: &Account) -> Result<url::Url, KiroError> {
    let mut url = operation_url(endpoint, "ListAvailableModels")?;
    let mut query = url.query_pairs_mut();
    // ListAvailableModels is a Kiro IDE REST operation even when it is served
    // from the q.* host. Enterprise Identity Center profiles are authorized by
    // profile ARN, not by the legacy Amazon Q CLI origin.
    query.append_pair("origin", "AI_EDITOR");
    query.append_pair("maxResults", "50");
    if let Some(profile_arn) = usage_profile_arn(account) {
        query.append_pair("profileArn", profile_arn);
    }
    drop(query);
    Ok(url)
}

fn usage_profile_arn(account: &Account) -> Option<&str> {
    if account.credentials.auth_method == AuthMethod::ApiKey {
        return None;
    }
    account
        .profile_arn
        .as_deref()
        .map(str::trim)
        .filter(|profile_arn| !profile_arn.is_empty())
        .filter(|profile_arn| {
            !account.credentials.region.trim().starts_with("us-gov-")
                || profile_arn.starts_with("arn:aws-us-gov:codewhisperer:us-gov-")
        })
        .filter(|profile_arn| {
            account.credentials.auth_method != AuthMethod::Idc
                || *profile_arn != KIRO_BUILDER_ID_PROFILE_ARN
        })
}

fn usage_api_regions(account: &Account) -> Vec<String> {
    let profile_region = account.profile_arn.as_deref().and_then(profile_arn_region);
    let primary = profile_region
        .map(str::to_owned)
        .unwrap_or_else(|| canonical_api_region(&account.credentials.region).to_owned());
    if account.credentials.region.trim().starts_with("us-gov-") {
        return vec![account.credentials.region.trim().to_owned()];
    }
    if primary.starts_with("us-gov-") || account.credentials.auth_method == AuthMethod::ApiKey {
        return vec![account_api_region(account).unwrap_or(primary)];
    }
    let fallback = if primary == "us-east-1" {
        "eu-central-1"
    } else {
        "us-east-1"
    };
    let mut regions = vec![primary];
    if regions[0] != fallback {
        regions.push(fallback.to_owned());
    }
    regions
}

fn token_type_headers(account: &Account) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    if account.credentials.auth_method == AuthMethod::ApiKey {
        headers.insert(
            "tokentype",
            reqwest::header::HeaderValue::from_static("API_KEY"),
        );
    }
    headers
}

fn canonical_api_region(region: &str) -> &'static str {
    if region.trim().starts_with("eu-") {
        "eu-central-1"
    } else {
        "us-east-1"
    }
}

fn profile_arn_region(profile_arn: &str) -> Option<&str> {
    let mut parts = profile_arn.split(':');
    let valid_prefix = parts.next() == Some("arn")
        && parts
            .next()
            .is_some_and(|partition| partition.starts_with("aws"))
        && parts.next() == Some("codewhisperer");
    let region = parts.next()?.trim();
    (valid_prefix
        && !region.is_empty()
        && region
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-'))
    .then_some(region)
}

fn usage_endpoint_label(url: &url::Url) -> String {
    match url.host_str() {
        Some(host) => format!("getUsageLimits ({host})"),
        None => "getUsageLimits".into(),
    }
}

fn truncate_chars(value: &str, maximum: usize) -> String {
    value.chars().take(maximum).collect()
}

async fn bounded_response_text(
    response: Response,
    maximum: usize,
    endpoint: &str,
) -> Result<String, KiroError> {
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        return Err(KiroError {
            status: Some(502),
            endpoint: endpoint.to_owned(),
            message: format!("response exceeds the {maximum} byte safety limit"),
        });
    }
    let mut source = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = source.next().await {
        let chunk = chunk.map_err(|error| KiroError {
            status: None,
            endpoint: endpoint.to_owned(),
            message: error.to_string(),
        })?;
        if body.len().saturating_add(chunk.len()) > maximum {
            return Err(KiroError {
                status: Some(502),
                endpoint: endpoint.to_owned(),
                message: format!("response exceeds the {maximum} byte safety limit"),
            });
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).map_err(|error| KiroError {
        status: Some(502),
        endpoint: endpoint.to_owned(),
        message: format!("response is not valid UTF-8: {error}"),
    })
}

fn sanitize_web_search_results(results: &mut WebSearchResults) {
    const MAX_RESULTS: usize = 50;
    results.query = truncate_chars(&results.query, 2_000);
    results.results.retain(|result| {
        result.url.chars().count() <= 8_192
            && url::Url::parse(&result.url)
                .ok()
                .is_some_and(|url| matches!(url.scheme(), "http" | "https"))
    });
    results.results.truncate(MAX_RESULTS);
    for result in &mut results.results {
        if result.title.trim().is_empty() {
            result.title = url::Url::parse(&result.url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .unwrap_or_else(|| result.url.clone());
        }
        result.title = truncate_chars(&result.title, 1_000);
        result.snippet = truncate_chars(&result.snippet, 32_000);
    }
}

fn build_error(error: impl std::fmt::Display) -> KiroError {
    KiroError {
        status: None,
        endpoint: "transport".into(),
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests;
