use std::collections::{BTreeSet, HashSet};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine;
use bytes::BytesMut;
use crosslane_core::provider::{ProviderId, ProviderModel, ProviderProtocol};
use crosslane_kiro::{EventStreamDecoder, KiroError, KiroEvent, KiroResponse};
use crosslane_pool::{AccountLease, PoolError};
use crosslane_translate::error::{is_context_length_error, log_model};
use crosslane_translate::model::{
    map_model_for_provider, resolve_dynamic_model, ModelMappingContext, ModelRoute,
};
use crosslane_translate::{
    apply_context_management_edits, claude_loaded_tools, claude_pending_server_tool_uses,
    claude_to_kiro, compact_trigger_tokens, compaction_summary_payload, error_envelope,
    estimate_context_management_input_tokens, has_context_management_edits, matches_type_family,
    normalize_compaction_boundary, openai_to_kiro, resume_tool_search_payload,
    resume_web_search_payload, sanitize_error_message, sanitize_kiro_tool_history,
    tool_search_continue_payload_batch, validate_claude, validate_claude_generation,
    validate_kiro_tool_history, validate_openai, web_search_continue_payload_batch,
    ClaudeContextEditStats, ClaudeRequest, ClaudeToolSearchBudget, ClaudeToolSearchCatalog,
    ClaudeWebSearchTrace, ErrorFormat, KiroCompactionPlan, KiroPayload, OpenAiRequest,
    TranslationOptions, ValidationError,
};
use futures::StreamExt;
use rand::Rng;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;

use crate::meter::{now_secs, AuthenticatedApiKey, CreditReservation, MeterError, UsageRecord};
use crate::state::AppState;
use crate::stats::{RequestDiagnostics, RequestLog, UpstreamAttemptLog};
use crosslane_core::client_context::ClientContext;

use super::prompt_cache::PromptCacheProfile;
use super::response::{
    ClaudeServerEvent, CompactionIterationUsage, DecodedResponse, OpenAiToolIdentity,
    StopSequenceFilter, ThinkingContentFilter, ToolLeakFilter,
};
use super::stream::{self, StreamContext, StreamProtocol};
use super::usage::{fallback_credits, fill_missing_usage};
use super::ServiceHttpState;
use super::{logged_user_agent, request_trace_id};

const MAX_STATS_MODEL_CHARS: usize = 128;
const MAX_ATTEMPT_LOG_SUMMARY_CHARS: usize = 4_096;
const UNKNOWN_STATS_MODEL: &str = "unknown";
const MAX_EMPTY_TOOL_RESULT_RETRIES: u8 = 1;
/// Credit reservation fallback only; never an implicit generation limit.
pub(super) const DEFAULT_OUTPUT_TOKEN_ESTIMATE: u32 = 8_192;
const MIN_COMPACTION_BACKGROUND_GRACE: Duration = Duration::from_millis(250);
const MAX_COMPACTION_BACKGROUND_GRACE: Duration = Duration::from_secs(5);
const COMPACTION_CLEANUP_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EmptyToolResultDisposition {
    Accept,
    Retry,
    Fail,
}

pub(super) fn current_message_has_tool_results(payload: &KiroPayload) -> bool {
    payload
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context
        .as_ref()
        .is_some_and(|context| !context.tool_results.is_empty())
}

/// A transport success without any assistant output is not a completed turn.
/// Tool continuations additionally require text, a tool call, or a terminal
/// reason; ordinary reasoning-only output remains valid and is never replayed.
pub(super) fn empty_tool_result_disposition(
    payload: &KiroPayload,
    decoded: &DecodedResponse,
    output_exhausted: bool,
    retries: u8,
) -> EmptyToolResultDisposition {
    let invalid_empty_continuation = decoded.text.trim().is_empty()
        && decoded.tools.is_empty()
        && ((decoded.reasoning.trim().is_empty()
            && decoded.reasoning_signature.is_none()
            && decoded.redacted_thinking.is_empty()
            && decoded.citations.is_empty())
            || current_message_has_tool_results(payload))
        && decoded.stop_reason.is_none()
        && decoded.stop_sequence.is_none()
        && !output_exhausted;
    if !invalid_empty_continuation {
        EmptyToolResultDisposition::Accept
    } else if retries < MAX_EMPTY_TOOL_RESULT_RETRIES {
        EmptyToolResultDisposition::Retry
    } else {
        EmptyToolResultDisposition::Fail
    }
}

fn stable_conversation_id(
    headers: &HeaderMap,
    api_key_id: Option<&str>,
    explicit: Option<&str>,
    fallback: Option<&str>,
) -> Option<String> {
    let hint = explicit
        .map(str::trim)
        .filter(|hint| !hint.is_empty())
        .or_else(|| {
            [
                "x-claude-code-session-id",
                "session-id",
                "thread-id",
                "x-opencode-session",
                "x-session-affinity",
                "x-client-request-id",
                "x-conversation-id",
                "session_id",
            ]
            .into_iter()
            .find_map(|name| {
                headers
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .map(str::trim)
                    .filter(|hint| !hint.is_empty() && hint.len() <= 256)
            })
        })
        .or_else(|| fallback.map(str::trim).filter(|hint| !hint.is_empty()))?;
    let mut digest = Sha256::new();
    // This versioned namespace is part of stable upstream session identity.
    // Keep the pre-rename bytes so upgrades do not split existing conversations.
    digest.update(b"kiro-proxy-conversation-v1\0");
    digest.update(api_key_id.unwrap_or("anonymous").as_bytes());
    digest.update(b"\0");
    digest.update(hint.as_bytes());
    let digest = digest.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // UUIDv8 reserves the payload bits for application-defined stable IDs.
    // Keeping the standard variant/version layout matches Kiro IDE wire IDs
    // while retaining deterministic, API-key-isolated session affinity.
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Some(Uuid::from_bytes(bytes).to_string())
}

fn conversation_fingerprint<T: serde::Serialize>(messages: &[T]) -> Option<String> {
    if messages.is_empty() {
        return None;
    }
    // The first client message is immutable when later turns are appended.
    // Hashing the first two made the fallback ID change between turn one and
    // turn two because the first request contains only one message.
    let encoded = serde_json::to_vec(&messages[..1]).ok()?;
    let mut digest = Sha256::new();
    // Paired with the stable session namespace above, not a display brand.
    digest.update(b"kiro-proxy-history-v1\0");
    digest.update(encoded);
    Some(
        digest.finalize()[..16]
            .iter()
            .fold(String::new(), |mut output, byte| {
                use std::fmt::Write as _;
                let _ = write!(output, "{byte:02x}");
                output
            }),
    )
}

fn compaction_replay_scope(
    service_id: &str,
    api_key_id: Option<&str>,
    conversation_fingerprint: Option<&str>,
) -> Option<String> {
    // Request-scoped affinity headers may legitimately change every turn.
    // Replay instead uses the immutable first-message fingerprint plus the
    // authenticated key namespace; the full-prefix digest is the correctness
    // check before any checkpoint is inserted.
    stable_conversation_id(
        &HeaderMap::new(),
        api_key_id,
        None,
        conversation_fingerprint,
    )
    .map(|conversation_id| format!("{service_id}:{conversation_id}"))
}

fn metadata_conversation_hint(metadata: Option<&Value>) -> Option<&str> {
    let metadata = metadata?.as_object()?;
    ["session_id", "conversation_id", "thread_id"]
        .into_iter()
        .find_map(|name| metadata.get(name).and_then(Value::as_str))
        .map(str::trim)
        .filter(|hint| !hint.is_empty() && hint.len() <= 256)
}

fn client_conversation_id(
    service_id: &str,
    api_key_id: Option<&str>,
    context: &ClientContext,
    headers: &HeaderMap,
    explicit: Option<&str>,
    fallback: Option<&str>,
) -> Option<String> {
    if context.is_unknown() {
        return stable_conversation_id(headers, api_key_id, explicit, fallback);
    }
    // A recognized session/thread outranks request-scoped affinity headers.
    // Explicit client conversation/cache keys retain their role within that
    // scope, while forks and distinct devices cannot collapse onto one UUID.
    let legacy = context
        .conversation_ref()
        .map(str::to_owned)
        .or_else(|| stable_conversation_id(headers, api_key_id, explicit, fallback))?;
    let explicit = stable_conversation_id(&HeaderMap::new(), None, explicit, None);
    let hint = serde_json::to_string(&(
        "crosslane-client-conversation-v1",
        service_id,
        context.user_ref.as_deref(),
        context.session_ref.as_deref(),
        legacy,
        explicit,
    ))
    .ok()?;
    stable_conversation_id(&HeaderMap::new(), api_key_id, Some(&hint), None)
}

fn client_replay_scope(
    service_id: &str,
    api_key_id: Option<&str>,
    context: &ClientContext,
    fingerprint: Option<&str>,
) -> Option<String> {
    if context.is_unknown() {
        return compaction_replay_scope(service_id, api_key_id, fingerprint);
    }
    // The full history-prefix digest remains mandatory in CompactionReplay.
    // The fingerprint also keeps unrelated histories in one client session apart.
    let hint = serde_json::to_string(&(
        "crosslane-client-replay-v1",
        context.user_ref.as_deref(),
        context.session_ref.as_deref(),
        context.conversation_ref(),
        fingerprint?,
    ))
    .ok()?;
    compaction_replay_scope(service_id, api_key_id, Some(&hint))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompactionReason {
    ClientTrigger,
    MappedWindowOverflow,
    ResolvedWindowOverflow,
    UpstreamWindowOverflow,
}

impl CompactionReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::ClientTrigger => "client_trigger",
            Self::MappedWindowOverflow => "mapped_overflow",
            Self::ResolvedWindowOverflow => "resolved_overflow",
            Self::UpstreamWindowOverflow => "upstream_overflow",
        }
    }
}

#[derive(Debug, Clone)]
struct CompactionDecision {
    reasons: Vec<CompactionReason>,
    model: String,
    trigger_tokens: u64,
    target_tokens: u64,
    maximum_tokens: u64,
}

impl CompactionDecision {
    fn reason_names(&self) -> String {
        self.reasons
            .iter()
            .map(|reason| reason.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }
}

#[derive(Clone)]
enum CompactionArtifact {
    Semantic {
        source_payload: KiroPayload,
        plan: KiroCompactionPlan,
        summary: String,
        deferred_parts: Option<Vec<String>>,
        summary_deadline: Option<Instant>,
        usage: CompactionIterationUsage,
    },
}

struct CompactionRun {
    payload: KiroPayload,
    stats: crosslane_translate::ContextCompactionStats,
    artifact: Option<CompactionArtifact>,
    mode: &'static str,
    summary_model: Option<String>,
    summary_input_tokens: Option<u64>,
    fallback_reason: Option<&'static str>,
    iteration_usage: Option<CompactionIterationUsage>,
}

#[derive(Clone)]
struct CompactionRequest<'a> {
    trace_id: &'a str,
    client_context: Option<&'a ClientContext>,
    key_id: Option<&'a str>,
    account_ids: Arc<HashSet<String>>,
    allowed_models: &'a [String],
    service_id: Option<&'a str>,
    source_payload: &'a KiroPayload,
    decision: &'a CompactionDecision,
    summary_model: &'a str,
    summary_timeout_ms: u64,
    preserve_recent_turns: usize,
    explicit_client_compaction: bool,
}

mod entrypoints;

use entrypoints::{attempt_diagnostics, read_bounded_body, record_failed_request};
pub use entrypoints::{claude_messages, health, openai_chat, readiness, root};

mod chat;
mod hosted_web;
mod provider_balance;
mod provider_gateway;
mod request;

use request::{handle_claude, handle_openai};
#[cfg(test)]
use request::{is_public_address, validate_remote_media_type, RemoteAttachmentKind};

const COMPACTION_USAGE_PATH: &str = "/internal/compact";

struct GeneratedCompactionSummary {
    content: String,
    deferred_parts: Option<Vec<String>>,
    usage: CompactionIterationUsage,
}

struct CompactionSummaryFailure {
    message: String,
    usage: Option<CompactionIterationUsage>,
}

impl From<String> for CompactionSummaryFailure {
    fn from(message: String) -> Self {
        Self {
            message,
            usage: None,
        }
    }
}

mod compaction;

#[cfg(test)]
use compaction::{
    await_compaction_summary_task, await_compaction_summary_task_with_policy,
    compaction_operation_target, parse_compaction_summary,
};
pub(super) use compaction::{
    compact_history_for_limits, compact_tool_search_continuation,
    tool_search_budget_with_compaction,
};
use compaction::{
    initial_compaction_decision, reapply_compaction, resolved_compaction_decision, run_compaction,
    upstream_overflow_compaction_decision,
};

#[derive(Debug, Default)]
struct RequestLogContext {
    provider_id: String,
    account_id: String,
    account_name: String,
    endpoint: String,
    mapped_model: String,
    kiro_model: String,
    model_path: Vec<String>,
    model_mapping_rule: Option<String>,
    attempts: Vec<UpstreamAttemptLog>,
    #[cfg(feature = "crs")]
    confirmed_usage: Option<crosslane_kiro::UsageInfo>,
}

struct DispatchFailure {
    error: KiroError,
    context: RequestLogContext,
}

struct UpstreamExecution {
    lease: AccountLease,
    response: KiroResponse,
    upstream_access_token: String,
    mapped_model: String,
    kiro_model: String,
    model_path: Vec<String>,
    model_mapping_rule: Option<String>,
    attempts: Vec<UpstreamAttemptLog>,
    payload: crosslane_translate::KiroPayload,
}

/// A selected account and its resolved wire model, before any generation.
/// Keep the lease only on the no-compaction path; summaries need their own slot.
struct PreparedUpstream {
    lease: AccountLease,
    mapped_model: String,
    kiro_model: String,
    model_path: Vec<String>,
    model_mapping_rule: Option<String>,
    attempts: Vec<UpstreamAttemptLog>,
    payload: KiroPayload,
}

enum ExecuteError {
    Pool(PoolError),
    Upstream(KiroError),
    Dispatch(Box<DispatchFailure>),
    Meter(MeterError),
    ContextLimit(ContextLimitError),
    UnsupportedWebSearch(String),
    ToolNameMismatch,
    ModelNotAllowed {
        provider_id: String,
        model: String,
    },
    CrossProviderRoute {
        source_provider: String,
        target_provider: String,
        model: String,
    },
}

pub(crate) fn model_is_allowed(allowed_models: &[String], provider_id: &str, model: &str) -> bool {
    if allowed_models.is_empty() {
        return true;
    }
    let qualified = format!("{provider_id}/{model}");
    allowed_models.iter().any(|pattern| {
        crosslane_translate::model::model_glob_matches(pattern, model)
            || crosslane_translate::model::model_glob_matches(pattern, &qualified)
    })
}

fn filter_kiro_models_for_key(
    mut models: Vec<crosslane_kiro::ModelInfo>,
    authenticated_key: Option<&AuthenticatedApiKey>,
) -> Vec<crosslane_kiro::ModelInfo> {
    if let Some(key) = authenticated_key {
        models.retain(|model| model_is_allowed(&key.allowed_models, "kiro", &model.model_id));
    }
    models
}

fn resolve_kiro_authorization_model(
    requested: &str,
    default_model: &str,
    available: &[String],
) -> String {
    resolve_dynamic_model(requested, available)
        .or_else(|| {
            if default_model.trim().is_empty() {
                None
            } else {
                resolve_dynamic_model(default_model, available)
            }
        })
        .unwrap_or_else(|| requested.to_owned())
}

pub(super) struct ContextLimitError {
    pub(super) model: String,
    pub(super) input_tokens: u64,
    pub(super) maximum: u64,
}

mod execution;

pub(super) use execution::{
    apply_remaining_output_budget, execute_filtered_kiro_web_search, execute_kiro_web_search,
    find_model_fallback, prepare_kiro_payload, resolve_static_model, set_payload_model,
    validate_internal_continuation,
};
#[cfg(test)]
use execution::{
    build_model_path, ensure_web_search_profile_arn, push_nonstream_event, retry_attempt_count,
};
use execution::{
    credits, execute_upstream, nonstream_claude, nonstream_openai, openai_tool_identities,
    prepare_upstream, prepend_attempt_logs, prepend_execute_error_attempts, request_log,
    usage_record,
};

pub async fn count_tokens(State(service): State<ServiceHttpState>, request: Request) -> Response {
    let state = Arc::clone(&service.app);
    let path = request.uri().path().to_string();
    let trace_id = request_trace_id(&request);
    let user_agent = logged_user_agent(request.headers());
    let started = Instant::now();
    let default_provider = state
        .config
        .current()
        .default_provider_for_service(&service.service)
        .to_owned();
    let _connection_guard = match state.connections.try_acquire() {
        Some(guard) => guard,
        None => {
            let error =
                ApiError::overloaded(ErrorFormat::Claude).with_provider_id(&default_provider);
            record_failed_request(&state, &trace_id, &path, &user_agent, "", started, &error);
            return error.with_request_id(&trace_id).into_response();
        }
    };
    let _admission_guard = match state.admission.try_acquire() {
        Some(guard) => guard,
        None => {
            let error =
                ApiError::overloaded(ErrorFormat::Claude).with_provider_id(&default_provider);
            record_failed_request(&state, &trace_id, &path, &user_agent, "", started, &error);
            return error.with_request_id(&trace_id).into_response();
        }
    };
    let mut client_context = ClientContext::default();
    let result = async {
        let (headers, body, _body_reservations) =
            read_bounded_body(&state, request, ErrorFormat::Claude)
                .await
                .map_err(|error| error.with_provider_id_if_empty(&default_provider))?;
        client_context = super::client_context::extract(&headers, &body);
        let provider_hint = provider_gateway::request_provider_hint(
            &state.config.current(),
            &service.service,
            &body,
        );
        let authenticated_key = authenticate(
            &state,
            &service.allowed_api_key_ids,
            &headers,
            ErrorFormat::Claude,
        )
        .map_err(|error| error.with_provider_id_if_empty(&provider_hint))?;
        enforce_claude_user_agent(&service, &headers, authenticated_key.as_ref())
            .map_err(|error| error.with_provider_id_if_empty(&provider_hint))?;
        let plan = provider_balance::plan(
            &service,
            authenticated_key.as_ref(),
            &body,
            ProviderProtocol::ClaudeMessages,
            ErrorFormat::Claude,
        )
        .await?
        .for_token_count();
        provider_balance::execute(
            &state.provider_scheduler,
            &state.stats,
            &service.service.id,
            &trace_id,
            &path,
            plan,
            ErrorFormat::Claude,
            |route| {
                let service = service.clone();
                let trace_id = trace_id.clone();
                let path = path.clone();
                let headers = headers.clone();
                let key = authenticated_key.clone();
                let connection = _connection_guard.clone();
                let admission = _admission_guard.clone();
                let context = client_context.clone();
                async move {
                    count_tokens_route(
                        service, trace_id, path, headers, route, key, connection, admission,
                        context,
                    )
                    .await
                    .map(|response| (response, ()))
                }
            },
        )
        .await
        .map(|(response, ())| response)
    }
    .await;
    super::client_context::log(&trace_id, &client_context);
    match result {
        Ok(response) => response,
        Err(error) => {
            let error = error.with_client_context(&client_context);
            record_failed_request(&state, &trace_id, &path, &user_agent, "", started, &error);
            error.with_request_id(&trace_id).into_response()
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn count_tokens_route(
    service: ServiceHttpState,
    trace_id: String,
    path: String,
    headers: HeaderMap,
    provider_route: provider_gateway::SelectedProviderRoute,
    authenticated_key: Option<AuthenticatedApiKey>,
    _connection_guard: crate::state::AdmissionGuard,
    _admission_guard: crate::state::AdmissionGuard,
    client_context: ClientContext,
) -> Result<Response, ApiError> {
    let state = Arc::clone(&service.app);
    let started = Instant::now();
    let user_agent = logged_user_agent(&headers);
    let key_id = authenticated_key.as_ref().map(|key| key.id.clone());
    if provider_route.provider_kind != "kiro" {
        return provider_gateway::execute_provider(
            service,
            provider_route,
            ProviderProtocol::ClaudeMessages,
            path.clone(),
            headers,
            trace_id.clone(),
            authenticated_key,
            _connection_guard,
            _admission_guard,
            ErrorFormat::Claude,
            true,
            client_context.clone(),
        )
        .await;
    }
    let automatic = provider_route.automatic;
    let lock_kiro_mapping = provider_route.lock_kiro_mapping;
    let locked_original_model = provider_route.original_model.clone();
    let locked_upstream_model = provider_route.upstream_model.clone();
    let locked_mapping_name = provider_route.mapping_rule.clone();
    let body = provider_route.body;
    tracing::debug!(
        trace_id = %trace_id,
        protocol = "claude_count_tokens",
        api_key_id = key_id.as_deref().unwrap_or("anonymous"),
        body_bytes = body.len(),
        "client authentication completed"
    );
    let mut value: Value = serde_json::from_slice(&body).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "Invalid JSON in request body",
            ErrorFormat::Claude,
        )
    })?;
    let object = value.as_object_mut().ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "request body must be a JSON object",
            ErrorFormat::Claude,
        )
    })?;
    object.entry("max_tokens").or_insert_with(|| json!(1));
    let mut original_request: ClaudeRequest = serde_json::from_value(value).map_err(|error| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("Invalid request body: {error}"),
            ErrorFormat::Claude,
        )
    })?;
    if lock_kiro_mapping {
        original_request.model.clone_from(&locked_original_model);
    }
    let conversation_fingerprint = conversation_fingerprint(&original_request.messages);
    let conversation_id = client_conversation_id(
        &service.service.id,
        key_id.as_deref(),
        &client_context,
        &headers,
        original_request.conversation_id.as_deref(),
        metadata_conversation_hint(original_request.metadata.as_ref())
            .or(conversation_fingerprint.as_deref()),
    );
    let replay_scope = client_replay_scope(
        &service.service.id,
        key_id.as_deref(),
        &client_context,
        conversation_fingerprint.as_deref(),
    );
    let replay = replay_scope.as_deref().and_then(|scope| {
        state
            .compaction_replay
            .find(scope, &original_request.messages)
    });
    let has_context_edits =
        has_context_management_edits(original_request.context_management.as_ref());
    // Validate the semantically effective history before touching remote
    // media. A completed compaction boundary can make stale history
    // irrelevant to protocol validation, while `original_input_tokens`
    // still needs to count that pre-boundary history.
    let mut request = original_request.clone();
    let replay_applied = replay
        .as_ref()
        .and_then(|replay| replay.apply_to(&mut request.messages));
    let compaction_normalization = normalize_compaction_boundary(&mut request);
    let boundary_applied = compaction_normalization.boundary_applied;
    crosslane_translate::validate_claude_count(&request).map_err(claude_validation_error)?;
    // Counting must include the real bytes of every attachment in the
    // pre-edit request. Generation can avoid fetching history that will
    // be cleared, but the count endpoint promises both original and
    // effective token totals.
    let _attachment_guards =
        request::hydrate_claude_attachments(&state, &mut original_request).await?;
    request.clone_from(&original_request);
    if let Some(replay) = replay.as_ref() {
        let _ = replay.apply_to(&mut request.messages);
    }
    normalize_compaction_boundary(&mut request);
    crosslane_translate::validate_claude_count(&request).map_err(claude_validation_error)?;
    tracing::info!(
        trace_id = %trace_id,
        protocol = "claude_count_tokens",
        model = %log_model(&request.model),
        message_count = request.messages.len(),
        tool_count = request.tools.len(),
        server_compaction_replayed = replay_applied.is_some(),
        "client token-count request validated"
    );
    let config = state.config.current();
    let claude_code = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_claude_user_agent);
    let client_compaction =
        claude_code && crosslane_translate::prepare_claude_code_compaction(&mut request);
    let original_client_compaction =
        claude_code && crosslane_translate::prepare_claude_code_compaction(&mut original_request);
    let tools_disabled = request::apply_claude_tool_policy(&mut request, &config.features);
    request::apply_claude_tool_policy(&mut original_request, &config.features);
    if !tools_disabled
        && !config.features.enable_tool_search
        && request.tools.iter().any(|tool| {
            tool.defer_loading
                || tool
                    .r#type
                    .as_deref()
                    .is_some_and(crosslane_translate::is_tool_search_type)
        })
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Anthropic Tool Search is disabled by proxy configuration",
            ErrorFormat::Claude,
        ));
    }
    let route = if lock_kiro_mapping {
        ModelRoute {
            original: locked_original_model,
            mapped: locked_upstream_model,
            rule: locked_mapping_name,
            rule_index: None,
        }
    } else {
        map_model_for_provider(
            &request.model,
            &config.model_mapping,
            ModelMappingContext {
                provider_id: "kiro",
                service_id: Some(&service.service.id),
                api_key_id: key_id.as_deref(),
                remaining_percent: None,
            },
            "",
        )
    };
    let (cached_models, _) = state.models.get(config.models.cache_ttl_ms);
    let authorization_models = if cached_models.is_empty() {
        fallback_models(&config)
    } else {
        cached_models
    };
    let available_models = authorization_models
        .iter()
        .map(|model| model.model_id.clone())
        .collect::<Vec<_>>();
    let authorization_model = resolve_kiro_authorization_model(
        &route.mapped,
        if automatic {
            ""
        } else {
            &config.features.default_model_id
        },
        &available_models,
    );
    provider_gateway::ensure_model_enabled(&config, &route.mapped, ErrorFormat::Claude)?;
    provider_gateway::ensure_model_enabled(&config, &authorization_model, ErrorFormat::Claude)?;
    provider_gateway::authorize_model(
        authenticated_key.as_ref(),
        "kiro",
        &authorization_model,
        ErrorFormat::Claude,
    )?;
    let mut normal = TranslationOptions::new(route.mapped.clone(), "AI_EDITOR");
    normal.enhance_system_prompt = config.features.enhance_system_prompt && !client_compaction;
    normal.enable_prompt_cache = config.features.enable_prompt_cache;
    normal.separate_compaction_instruction = client_compaction;
    normal.conversation_id = conversation_id;
    normal.additional_model_request_fields_schema = state
        .resolved_model_info(&route.mapped)
        .and_then(|model| model.additional_model_request_fields_schema);
    let mut original_payload = claude_to_kiro(&original_request, &normal);
    if original_client_compaction {
        crosslane_translate::prepare_compaction_source_history(&mut original_payload);
    }
    state.prepare_model_request(&mut original_payload);
    let original_input_tokens = state
        .tokenizer
        .estimate_kiro_payload(&original_payload)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                error,
                ErrorFormat::Claude,
            )
        })?;
    let mut effective_request = request.clone();
    let context_edit_stats = apply_context_management_edits(
        &mut effective_request,
        u64::try_from(original_input_tokens).unwrap_or(u64::MAX),
    );
    let mut effective_payload = claude_to_kiro(&effective_request, &normal);
    if client_compaction {
        crosslane_translate::prepare_compaction_source_history(&mut effective_payload);
    }
    state.prepare_model_request(&mut effective_payload);
    let input_tokens = state
        .tokenizer
        .estimate_kiro_payload(&effective_payload)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                error,
                ErrorFormat::Claude,
            )
        })?;
    let mut response = json!({"input_tokens":input_tokens});
    if has_context_edits {
        response["context_management"] = json!({
            "original_input_tokens":original_input_tokens
        });
    }
    state.stats.record(RequestLog {
        timestamp: now_secs(),
        trace_id: trace_id.clone(),
        request_id: format!("req_{}", Uuid::new_v4().simple()),
        path: path.clone(),
        user_agent: user_agent.clone(),
        provider_id: "kiro".into(),
        model: route.mapped.clone(),
        original_model: request.model.clone(),
        kiro_model: route.mapped.clone(),
        account_id: String::new(),
        account_name: String::new(),
        endpoint: "local-tokenizer".into(),
        model_path: vec![request.model.clone(), route.mapped.clone()],
        model_mapping_rule: route.rule.clone(),
        attempts: Vec::new(),
        duration_ms: started.elapsed().as_millis() as u64,
        status: 200,
        input_tokens: input_tokens as u64,
        output_tokens: 0,
        credits: 0.0,
        error: None,
        diagnostics: RequestDiagnostics {
            client_context: Some(client_context.clone()),
            client_status: 200,
            payload_bytes: body.len(),
            ..RequestDiagnostics::default()
        },
    });
    tracing::info!(
        trace_id = %trace_id,
        protocol = "claude_count_tokens",
        model = %log_model(&request.model),
        input_tokens,
        original_input_tokens,
        compaction_boundary_applied = boundary_applied,
        server_compaction_replayed = replay_applied.is_some(),
        removed_noop_compaction_blocks = compaction_normalization.removed_noop_blocks,
        removed_noop_compaction_messages = compaction_normalization.removed_noop_messages,
        cleared_tool_results = context_edit_stats.cleared_tool_results,
        cleared_tool_inputs = context_edit_stats.cleared_tool_inputs,
        duration_ms = started.elapsed().as_millis() as u64,
        "client token-count response completed"
    );
    Ok::<_, ApiError>(Json(response).into_response())
}

pub async fn models(State(service): State<ServiceHttpState>, request: Request) -> Response {
    let state = Arc::clone(&service.app);
    let trace_id = request_trace_id(&request);
    let path = request.uri().path().to_owned();
    let raw_query = request.uri().query().map(str::to_owned);
    let headers = request.headers().clone();
    let user_agent = logged_user_agent(&headers);
    let started = Instant::now();
    let protocol = model_list_protocol(&headers);
    let format = protocol.error_format();
    let result = async {
        let authenticated = authenticate(&state, &service.allowed_api_key_ids, &headers, format)?;
        // Model discovery shares one URL across protocols, so User-Agent is
        // content negotiation here rather than an admission requirement.
        let query = match protocol {
            ModelListProtocol::OpenAi => None,
            ModelListProtocol::Anthropic => Some(parse_model_list_query(
                raw_query.as_deref(),
                ANTHROPIC_MODEL_LIST_DEFAULT_LIMIT,
                format,
            )?),
            ModelListProtocol::Hybrid => Some(parse_model_list_query(
                raw_query.as_deref(),
                usize::MAX,
                format,
            )?),
        };
        let current_service = service.current_service();
        let account_ids = state
            .pool()
            .matching_account_ids(|account| current_service.includes_account(account))
            .await;
        let config = state.config.current();
        let effective_providers = config.effective_providers();
        let service_allowed = config.allowed_providers_for_service(&current_service);
        let key_allowed = authenticated
            .as_ref()
            .map(|key| {
                if key.allowed_providers.is_empty() {
                    vec!["kiro"]
                } else {
                    key.allowed_providers.iter().map(String::as_str).collect()
                }
            })
            .unwrap_or_else(|| service_allowed.clone());
        let allowed = effective_providers
            .iter()
            .filter(|provider| {
                provider.enabled
                    && service_allowed.contains(&provider.id.as_str())
                    && key_allowed.contains(&provider.id.as_str())
            })
            .collect::<Vec<_>>();
        let legacy_kiro_only =
            allowed.len() == 1 && allowed[0].id == "kiro" && allowed[0].kind == "kiro";
        if !legacy_kiro_only {
            if allowed.is_empty() {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "no provider is allowed for this service and API key",
                    format,
                ));
            }
            let mut models = Vec::new();
            let mut errors = std::collections::BTreeMap::new();
            for provider in &allowed {
                let id = ProviderId::parse(provider.id.clone()).map_err(|error| {
                    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string(), format)
                })?;
                if provider.kind == "kiro" {
                    let scoped = kiro_models_for_service(&state, &account_ids).await;
                    models.extend(crate::providers::kiro_provider_models(&id, scoped));
                } else if let Some(runtime) = state.providers.copilot(&id).await {
                    match runtime.models_for_service(&current_service, false).await {
                        Ok(discovered) => models.extend(discovered),
                        Err(error) => {
                            errors.insert(provider.id.clone(), error.to_string());
                        }
                    }
                } else {
                    // Unavailable or unsupported adapters retain their existing
                    // discovery errors; active Copilot runtimes use the scoped path.
                    let (_, failures) = state.providers.models(&[id], false).await;
                    if failures.is_empty() {
                        errors.insert(
                            provider.id.clone(),
                            "provider does not support service-scoped model discovery".into(),
                        );
                    } else {
                        errors.extend(failures);
                    }
                }
            }
            models.sort_by(|left, right| {
                left.provider_id
                    .cmp(&right.provider_id)
                    .then_with(|| left.id.cmp(&right.id))
            });
            models.retain(|model| !state.config.current().is_model_disabled(&model.id));
            if let Some(key) = authenticated
                .as_ref()
                .filter(|key| !key.allowed_models.is_empty())
            {
                models.retain(|model| {
                    let qualified = format!("{}/{}", model.provider_id, model.id);
                    key.allowed_models.iter().any(|pattern| {
                        crosslane_translate::model::model_glob_matches(pattern, &model.id)
                            || crosslane_translate::model::model_glob_matches(pattern, &qualified)
                    })
                });
            }
            if models.is_empty() && !errors.is_empty() {
                return Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!(
                        "model discovery failed: {}",
                        errors
                            .iter()
                            .map(|(provider, error)| format!("{provider}: {error}"))
                            .collect::<Vec<_>>()
                            .join("; ")
                    ),
                    format,
                ));
            }
            for (provider, error) in errors {
                tracing::warn!(%provider, %error, "provider model discovery failed");
            }
            return provider_model_list(models, allowed.len() > 1, protocol, query.as_ref());
        }
        let models = kiro_models_for_service(&state, &account_ids).await;
        let mut models = filter_kiro_models_for_key(models, authenticated.as_ref());
        models.retain(|model| !state.config.current().is_model_disabled(&model.model_id));
        model_list(models, protocol, query.as_ref())
    }
    .await;
    result.unwrap_or_else(|error| {
        let default_provider = state
            .config
            .current()
            .default_provider_for_service(&service.service)
            .to_owned();
        let error = error.with_provider_id_if_empty(&default_provider);
        record_failed_request(&state, &trace_id, &path, &user_agent, "", started, &error);
        error.into_response()
    })
}

/// Both Kiro-only and mixed-provider lists use the same account-scoped catalog
/// and refresh. Static fallback still requires an enabled in-scope account.
async fn kiro_models_for_service(
    state: &Arc<AppState>,
    account_ids: &HashSet<String>,
) -> Vec<crosslane_kiro::ModelInfo> {
    let config = state.config.current();
    if !config.models.dynamic_discovery {
        let (has_enabled_accounts, _) = service_catalog_status(state, account_ids, &[]).await;
        // Discovery-disabled installations intentionally ignore stale dynamic
        // account catalogs, but the static list still requires a scoped account.
        return if has_enabled_accounts {
            fallback_models(&config)
        } else {
            Vec::new()
        };
    }
    let (cached, fresh) = state.models.get(config.models.cache_ttl_ms);
    let (has_enabled_accounts, scoped_catalog_ready) =
        service_catalog_status(state, account_ids, &cached).await;
    if has_enabled_accounts && (!fresh || !scoped_catalog_ready) && state.models.begin_refresh() {
        let refresh_state = Arc::clone(state);
        let refresh_account_ids = account_ids.clone();
        tokio::spawn(async move {
            if let Err(error) =
                crate::tasks::refresh_models_scoped(&refresh_state, &refresh_account_ids).await
            {
                tracing::warn!(%error, "on-demand model discovery failed");
            }
        });
    }
    let models = if cached.is_empty() {
        fallback_models(&config)
    } else {
        cached
    };
    service_models(state, account_ids, models).await
}

async fn service_catalog_status(
    state: &Arc<AppState>,
    account_ids: &HashSet<String>,
    models: &[crosslane_kiro::ModelInfo],
) -> (bool, bool) {
    let model_ids = models
        .iter()
        .map(|model| model.model_id.as_str())
        .collect::<HashSet<_>>();
    let pool = state.pool();
    let scoped_accounts = pool
        .snapshot()
        .await
        .into_iter()
        .filter(|account| account.enabled && account_ids.contains(&account.id))
        .collect::<Vec<_>>();
    if scoped_accounts.is_empty() {
        return (false, false);
    }
    for account in scoped_accounts {
        let Some(runtime) = pool.get(&account.id).await else {
            return (true, false);
        };
        if !runtime.has_model_cache().await
            || runtime
                .supported_models()
                .await
                .iter()
                .any(|model_id| !model_ids.contains(model_id.as_str()))
        {
            return (true, false);
        }
    }
    (true, true)
}

async fn service_models(
    state: &Arc<AppState>,
    account_ids: &HashSet<String>,
    mut models: Vec<crosslane_kiro::ModelInfo>,
) -> Vec<crosslane_kiro::ModelInfo> {
    let pool = state.pool();
    let pool_accounts = pool.snapshot().await;
    let enabled_ids = pool_accounts
        .into_iter()
        .filter(|account| account.enabled && account_ids.contains(&account.id))
        .map(|account| account.id)
        .collect::<Vec<_>>();
    if enabled_ids.is_empty() {
        return Vec::new();
    }
    let mut supported = HashSet::new();
    let mut has_scoped_catalog = false;
    for account_id in enabled_ids {
        if let Some(account) = pool.get(&account_id).await {
            if account.has_model_cache().await {
                has_scoped_catalog = true;
                supported.extend(account.supported_models().await);
            }
        }
    }
    // Before discovery has populated any scoped account, use only the static
    // fallback rather than a dynamic catalog learned from another service.
    if !has_scoped_catalog {
        return fallback_models(&state.config.current());
    }
    models.retain(|model| supported.contains(&model.model_id));
    models
}
const ANTHROPIC_MODEL_LIST_DEFAULT_LIMIT: usize = 20;
const ANTHROPIC_MODEL_LIST_MAX_LIMIT: usize = 1_000;
const UNKNOWN_MODEL_CREATED_AT: &str = "1970-01-01T00:00:00Z";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModelListProtocol {
    OpenAi,
    Anthropic,
    Hybrid,
}

impl ModelListProtocol {
    fn error_format(self) -> ErrorFormat {
        match self {
            Self::Anthropic => ErrorFormat::Claude,
            Self::OpenAi | Self::Hybrid => ErrorFormat::OpenAi,
        }
    }
}

fn model_list_protocol(headers: &HeaderMap) -> ModelListProtocol {
    let agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    // This endpoint is shared. A positively identified client takes priority;
    // otherwise Anthropic's version header is an explicit protocol signal.
    if is_claude_user_agent(agent) {
        ModelListProtocol::Anthropic
    } else if is_codex_user_agent(agent) {
        ModelListProtocol::OpenAi
    } else if headers.contains_key("anthropic-version") {
        ModelListProtocol::Anthropic
    } else {
        ModelListProtocol::Hybrid
    }
}

#[derive(Clone, Debug)]
struct AnthropicModelListQuery {
    after_id: Option<String>,
    before_id: Option<String>,
    limit: usize,
}

impl Default for AnthropicModelListQuery {
    fn default() -> Self {
        Self {
            after_id: None,
            before_id: None,
            limit: ANTHROPIC_MODEL_LIST_DEFAULT_LIMIT,
        }
    }
}

#[derive(serde::Serialize)]
struct OpenAiModelListResponse {
    object: &'static str,
    data: Vec<OpenAiModelResponse>,
}

#[derive(serde::Serialize)]
struct OpenAiModelResponse {
    id: String,
    object: &'static str,
    created: i64,
    owned_by: String,
}

#[derive(Clone, serde::Serialize)]
struct AnthropicModelResponse {
    r#type: &'static str,
    id: String,
    capabilities: Value,
    created_at: &'static str,
    display_name: String,
    max_input_tokens: Option<u64>,
    max_tokens: Option<u64>,
}

#[derive(serde::Serialize)]
struct AnthropicModelListResponse {
    data: Vec<AnthropicModelResponse>,
    first_id: Option<String>,
    has_more: bool,
    last_id: Option<String>,
}

#[derive(Clone, serde::Serialize)]
struct HybridModelResponse {
    id: String,
    object: &'static str,
    created: i64,
    owned_by: String,
    r#type: &'static str,
    capabilities: Value,
    created_at: &'static str,
    display_name: String,
    max_input_tokens: Option<u64>,
    max_tokens: Option<u64>,
}

#[derive(serde::Serialize)]
struct HybridModelListResponse {
    object: &'static str,
    data: Vec<HybridModelResponse>,
    first_id: Option<String>,
    has_more: bool,
    last_id: Option<String>,
}

fn parse_model_list_query(
    raw_query: Option<&str>,
    default_limit: usize,
    format: ErrorFormat,
) -> Result<AnthropicModelListQuery, ApiError> {
    let mut query = AnthropicModelListQuery {
        limit: default_limit,
        ..AnthropicModelListQuery::default()
    };
    for (name, value) in url::form_urlencoded::parse(raw_query.unwrap_or_default().as_bytes()) {
        match name.as_ref() {
            "after_id" => {
                if query.after_id.is_some() || value.is_empty() {
                    return Err(invalid_model_list_query(
                        "after_id must be a non-empty model ID and may appear only once",
                        format,
                    ));
                }
                query.after_id = Some(value.into_owned());
            }
            "before_id" => {
                if query.before_id.is_some() || value.is_empty() {
                    return Err(invalid_model_list_query(
                        "before_id must be a non-empty model ID and may appear only once",
                        format,
                    ));
                }
                query.before_id = Some(value.into_owned());
            }
            "limit" => {
                let limit = value.parse::<usize>().map_err(|_| {
                    invalid_model_list_query("limit must be an integer from 1 to 1000", format)
                })?;
                if !(1..=ANTHROPIC_MODEL_LIST_MAX_LIMIT).contains(&limit) {
                    return Err(invalid_model_list_query(
                        "limit must be an integer from 1 to 1000",
                        format,
                    ));
                }
                query.limit = limit;
            }
            _ => {}
        }
    }
    if query.after_id.is_some() && query.before_id.is_some() {
        return Err(invalid_model_list_query(
            "after_id and before_id cannot be used together",
            format,
        ));
    }
    Ok(query)
}

fn invalid_model_list_query(message: &str, format: ErrorFormat) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, message, format)
}

fn model_list(
    models: Vec<crosslane_kiro::ModelInfo>,
    protocol: ModelListProtocol,
    query: Option<&AnthropicModelListQuery>,
) -> Result<Response, ApiError> {
    match protocol {
        ModelListProtocol::OpenAi => Ok(openai_model_list(models)),
        ModelListProtocol::Anthropic => {
            anthropic_model_list(models, query.cloned().unwrap_or_default())
        }
        ModelListProtocol::Hybrid => hybrid_model_list(
            models,
            query.cloned().unwrap_or(AnthropicModelListQuery {
                limit: usize::MAX,
                ..AnthropicModelListQuery::default()
            }),
        ),
    }
}

fn provider_model_list(
    models: Vec<ProviderModel>,
    qualify: bool,
    protocol: ModelListProtocol,
    query: Option<&AnthropicModelListQuery>,
) -> Result<Response, ApiError> {
    let models = models
        .into_iter()
        .filter(|model| match protocol {
            ModelListProtocol::Anthropic => {
                model.protocols.contains(&ProviderProtocol::ClaudeMessages)
            }
            ModelListProtocol::OpenAi => model.protocols.iter().any(|protocol| {
                matches!(
                    protocol,
                    ProviderProtocol::OpenAiChat | ProviderProtocol::OpenAiResponses
                )
            }),
            ModelListProtocol::Hybrid => true,
        })
        .map(|model| {
            let local_id =
                if model.provider_id.as_str() == "kiro" && protocol != ModelListProtocol::OpenAi {
                    crosslane_translate::model::claude_discovery_model_id(&model.id)
                } else {
                    model.id.clone()
                };
            let id = if qualify {
                format!("{}/{}", model.provider_id, local_id)
            } else {
                local_id
            };
            (model, id)
        })
        .collect::<Vec<_>>();
    match protocol {
        ModelListProtocol::OpenAi => {
            let created = now_secs();
            Ok(Json(OpenAiModelListResponse {
                object: "list",
                data: models
                    .into_iter()
                    .map(|(model, id)| OpenAiModelResponse {
                        id,
                        object: "model",
                        created,
                        owned_by: model.provider_id.to_string(),
                    })
                    .collect(),
            })
            .into_response())
        }
        ModelListProtocol::Anthropic => {
            let data = models
                .into_iter()
                .map(|(model, id)| AnthropicModelResponse {
                    r#type: "model",
                    display_name: if model.display_name.trim().is_empty() {
                        id.clone()
                    } else {
                        model.display_name
                    },
                    id,
                    capabilities: model.capabilities,
                    created_at: UNKNOWN_MODEL_CREATED_AT,
                    max_input_tokens: model.max_input_tokens,
                    max_tokens: model.max_output_tokens,
                })
                .collect::<Vec<_>>();
            let query = query.cloned().unwrap_or_default();
            let (start, end, has_more) =
                model_page_bounds(&data, &query, ErrorFormat::Claude, |model| {
                    model.id.as_str()
                })?;
            let page = data[start..end].to_vec();
            Ok(Json(AnthropicModelListResponse {
                first_id: page.first().map(|model| model.id.clone()),
                last_id: page.last().map(|model| model.id.clone()),
                data: page,
                has_more,
            })
            .into_response())
        }
        ModelListProtocol::Hybrid => {
            let created = now_secs();
            let data = models
                .into_iter()
                .map(|(model, id)| HybridModelResponse {
                    id: id.clone(),
                    object: "model",
                    created,
                    owned_by: model.provider_id.to_string(),
                    r#type: "model",
                    capabilities: model.capabilities,
                    created_at: UNKNOWN_MODEL_CREATED_AT,
                    display_name: if model.display_name.trim().is_empty() {
                        id
                    } else {
                        model.display_name
                    },
                    max_input_tokens: model.max_input_tokens,
                    max_tokens: model.max_output_tokens,
                })
                .collect::<Vec<_>>();
            let query = query.cloned().unwrap_or(AnthropicModelListQuery {
                limit: usize::MAX,
                ..AnthropicModelListQuery::default()
            });
            let (start, end, has_more) =
                model_page_bounds(&data, &query, ErrorFormat::OpenAi, |model| {
                    model.id.as_str()
                })?;
            let page = data[start..end].to_vec();
            Ok(Json(HybridModelListResponse {
                object: "list",
                first_id: page.first().map(|model| model.id.clone()),
                last_id: page.last().map(|model| model.id.clone()),
                data: page,
                has_more,
            })
            .into_response())
        }
    }
}

fn openai_model_list(models: Vec<crosslane_kiro::ModelInfo>) -> Response {
    let created = now_secs();
    let data = models
        .into_iter()
        .map(|model| OpenAiModelResponse {
            id: model.model_id,
            object: "model",
            created,
            owned_by: "kiro".into(),
        })
        .collect();
    Json(OpenAiModelListResponse {
        object: "list",
        data,
    })
    .into_response()
}

fn anthropic_model_list(
    models: Vec<crosslane_kiro::ModelInfo>,
    query: AnthropicModelListQuery,
) -> Result<Response, ApiError> {
    let data = models
        .into_iter()
        .map(|model| {
            let id = crosslane_translate::model::claude_discovery_model_id(&model.model_id);
            let (max_input_tokens, max_tokens) = model
                .token_limits
                .map(|limits| (limits.max_input_tokens, limits.max_output_tokens))
                .unwrap_or_default();
            AnthropicModelResponse {
                r#type: "model",
                display_name: if model.model_name.trim().is_empty() {
                    id.clone()
                } else {
                    model.model_name
                },
                id,
                capabilities: Value::Null,
                created_at: UNKNOWN_MODEL_CREATED_AT,
                max_input_tokens: max_input_tokens.map(u64::from),
                max_tokens: max_tokens.map(u64::from),
            }
        })
        .collect::<Vec<_>>();

    let (start, end, has_more) = model_page_bounds(&data, &query, ErrorFormat::Claude, |model| {
        model.id.as_str()
    })?;
    let page = data[start..end].to_vec();
    let first_id = page.first().map(|model| model.id.clone());
    let last_id = page.last().map(|model| model.id.clone());
    Ok(Json(AnthropicModelListResponse {
        data: page,
        first_id,
        has_more,
        last_id,
    })
    .into_response())
}

fn hybrid_model_list(
    models: Vec<crosslane_kiro::ModelInfo>,
    query: AnthropicModelListQuery,
) -> Result<Response, ApiError> {
    let created = now_secs();
    let data = models
        .into_iter()
        .map(|model| {
            let id = crosslane_translate::model::claude_discovery_model_id(&model.model_id);
            let (max_input_tokens, max_tokens) = model
                .token_limits
                .map(|limits| (limits.max_input_tokens, limits.max_output_tokens))
                .unwrap_or_default();
            HybridModelResponse {
                object: "model",
                created,
                owned_by: "kiro".into(),
                r#type: "model",
                capabilities: Value::Null,
                created_at: UNKNOWN_MODEL_CREATED_AT,
                display_name: if model.model_name.trim().is_empty() {
                    id.clone()
                } else {
                    model.model_name
                },
                id,
                max_input_tokens: max_input_tokens.map(u64::from),
                max_tokens: max_tokens.map(u64::from),
            }
        })
        .collect::<Vec<_>>();
    let (start, end, has_more) = model_page_bounds(&data, &query, ErrorFormat::OpenAi, |model| {
        model.id.as_str()
    })?;
    let page = data[start..end].to_vec();
    let first_id = page.first().map(|model| model.id.clone());
    let last_id = page.last().map(|model| model.id.clone());
    Ok(Json(HybridModelListResponse {
        object: "list",
        data: page,
        first_id,
        has_more,
        last_id,
    })
    .into_response())
}

fn model_page_bounds<T>(
    data: &[T],
    query: &AnthropicModelListQuery,
    format: ErrorFormat,
    id: impl Fn(&T) -> &str,
) -> Result<(usize, usize, bool), ApiError> {
    if let Some(after_id) = query.after_id.as_deref() {
        let start = model_cursor_index(data, after_id, "after_id", format, &id)? + 1;
        let end = start.saturating_add(query.limit).min(data.len());
        Ok((start, end, end < data.len()))
    } else if let Some(before_id) = query.before_id.as_deref() {
        let end = model_cursor_index(data, before_id, "before_id", format, &id)?;
        let start = end.saturating_sub(query.limit);
        Ok((start, end, start > 0))
    } else {
        let end = query.limit.min(data.len());
        Ok((0, end, end < data.len()))
    }
}

fn model_cursor_index<T>(
    data: &[T],
    cursor: &str,
    parameter: &str,
    format: ErrorFormat,
    id: impl Fn(&T) -> &str,
) -> Result<usize, ApiError> {
    data.iter()
        .position(|model| id(model) == cursor)
        .ok_or_else(|| {
            invalid_model_list_query(
                &format!("invalid pagination cursor for '{parameter}'"),
                format,
            )
        })
}

pub async fn event_logging(State(service): State<ServiceHttpState>, request: Request) -> Response {
    let trace_id = request_trace_id(&request);
    let path = request.uri().path().to_owned();
    let headers = request.headers();
    let user_agent = logged_user_agent(headers);
    let started = Instant::now();
    match authenticate(
        &service.app,
        &service.allowed_api_key_ids,
        headers,
        ErrorFormat::OpenAi,
    ) {
        Ok(_) => Json(json!({"status":"ok"})).into_response(),
        Err(error) => {
            let default_provider = service
                .app
                .config
                .current()
                .default_provider_for_service(&service.service)
                .to_owned();
            let error = error.with_provider_id_if_empty(&default_provider);
            record_failed_request(
                &service.app,
                &trace_id,
                &path,
                &user_agent,
                "",
                started,
                &error,
            );
            error.into_response()
        }
    }
}

fn recorded_route_error(service: ServiceHttpState, request: Request, error: ApiError) -> Response {
    let trace_id = request_trace_id(&request);
    let user_agent = logged_user_agent(request.headers());
    let default_provider = service
        .app
        .config
        .current()
        .default_provider_for_service(&service.service)
        .to_owned();
    let error = error.with_provider_id_if_empty(&default_provider);
    record_failed_request(
        &service.app,
        &trace_id,
        request.uri().path(),
        &user_agent,
        "",
        Instant::now(),
        &error,
    );
    error.into_response()
}

pub async fn claude_method_not_allowed(
    State(service): State<ServiceHttpState>,
    request: Request,
) -> Response {
    recorded_route_error(
        service,
        request,
        ApiError::method_not_allowed(ErrorFormat::Claude, "POST"),
    )
}

pub async fn openai_method_not_allowed(
    State(service): State<ServiceHttpState>,
    request: Request,
) -> Response {
    recorded_route_error(
        service,
        request,
        ApiError::method_not_allowed(ErrorFormat::OpenAi, "POST"),
    )
}

pub async fn openai_models_method_not_allowed(
    State(service): State<ServiceHttpState>,
    request: Request,
) -> Response {
    recorded_route_error(
        service,
        request,
        ApiError::method_not_allowed(ErrorFormat::OpenAi, "GET"),
    )
}

pub async fn not_found(State(service): State<ServiceHttpState>, request: Request) -> Response {
    let format = if request.uri().path().contains("messages") {
        ErrorFormat::Claude
    } else {
        ErrorFormat::OpenAi
    };
    recorded_route_error(
        service,
        request,
        ApiError::new(StatusCode::NOT_FOUND, "not found", format),
    )
}

pub(crate) fn fallback_models(
    config: &crosslane_core::config::Config,
) -> Vec<crosslane_kiro::ModelInfo> {
    let mut models = crosslane_kiro::static_models();
    let mut model_ids = models
        .iter()
        .map(|model| model.model_id.clone())
        .collect::<HashSet<_>>();
    let configured_ids = config
        .model_mapping
        .iter()
        .flat_map(|rule| rule.target_models.iter().cloned())
        .collect::<Vec<_>>();
    if !config.features.default_model_id.trim().is_empty() {
        let model_id = config.features.default_model_id.clone();
        if model_ids.insert(model_id.clone()) {
            models.push(configured_model_info(model_id));
        }
    }
    for model_id in configured_ids {
        if model_ids.insert(model_id.clone()) {
            models.push(configured_model_info(model_id));
        }
    }
    models
}

fn configured_model_info(model_id: String) -> crosslane_kiro::ModelInfo {
    crosslane_kiro::ModelInfo {
        model_name: model_id.clone(),
        model_id,
        description: "Configured Kiro upstream model".into(),
        rate_multiplier: None,
        token_limits: None,
        additional_model_request_fields_schema: None,
    }
}

fn claude_web_tool_names(request: &ClaudeRequest) -> std::collections::HashMap<String, String> {
    let mut names = crosslane_translate::claude_tool_name_map(request);
    names.extend(request.tools.iter().filter_map(|tool| {
        let kind = tool.r#type.as_deref()?;
        if matches_type_family(kind, "web_search") {
            Some(("web_search".into(), kind.into()))
        } else if matches_type_family(kind, "web_fetch") {
            Some(("web_fetch".into(), kind.into()))
        } else {
            None
        }
    }));
    names
}

fn authenticate(
    state: &Arc<AppState>,
    allowed_api_key_ids: &HashSet<String>,
    headers: &HeaderMap,
    format: ErrorFormat,
) -> Result<Option<AuthenticatedApiKey>, ApiError> {
    let presented = headers
        .get("x-api-key")
        .or_else(|| headers.get("anthropic-api-key"))
        .and_then(|value| value.to_str().ok())
        .or_else(|| {
            headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
        });
    let key_id = state.meter.authenticate(presented).map_err(|_| {
        let mut error = ApiError::new(StatusCode::UNAUTHORIZED, "invalid API key", format);
        error.authenticate = true;
        error.suppress_model_stats = true;
        error
    })?;
    if !allowed_api_key_ids.is_empty()
        && key_id
            .as_ref()
            .is_none_or(|key| !allowed_api_key_ids.contains(&key.id))
    {
        let mut error = ApiError::new(StatusCode::UNAUTHORIZED, "invalid API key", format);
        error.authenticate = true;
        error.suppress_model_stats = true;
        return Err(error);
    }
    Ok(key_id)
}

fn enforce_claude_user_agent(
    service: &ServiceHttpState,
    headers: &HeaderMap,
    authenticated_key: Option<&AuthenticatedApiKey>,
) -> Result<(), ApiError> {
    enforce_client_user_agent(service, headers, ErrorFormat::Claude, authenticated_key)
}

fn enforce_codex_user_agent(
    service: &ServiceHttpState,
    headers: &HeaderMap,
    authenticated_key: Option<&AuthenticatedApiKey>,
) -> Result<(), ApiError> {
    enforce_client_user_agent(service, headers, ErrorFormat::OpenAi, authenticated_key)
}

fn enforce_client_user_agent(
    service: &ServiceHttpState,
    headers: &HeaderMap,
    format: ErrorFormat,
    authenticated_key: Option<&AuthenticatedApiKey>,
) -> Result<(), ApiError> {
    let config = service.app.config.current();
    let service_bypass = config
        .proxy_service
        .iter()
        .find(|current| current.id == service.service.id)
        .map(|current| current.skip_user_agent_check)
        .unwrap_or(service.service.skip_user_agent_check);
    let policy = crosslane_core::config::resolve_user_agent_check_policy(
        config.server.enforce_user_agent_check,
        service_bypass,
        authenticated_key.is_some_and(|key| key.skip_user_agent_check),
    );
    if !policy.enforced() {
        return Ok(());
    }
    let value = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let (valid, client) = match format {
        ErrorFormat::Claude => (is_claude_user_agent(value), "Claude Code"),
        ErrorFormat::OpenAi => (is_codex_user_agent(value), "Codex"),
    };
    if valid {
        Ok(())
    } else {
        let mut error = ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("Access denied. 本协议仅限 {client} 客户端使用，禁止通过其他方式接入。"),
            format,
        );
        error.suppress_model_stats = true;
        Err(error)
    }
}

fn is_claude_user_agent(value: &str) -> bool {
    // Gateway discovery uses a different product token from Messages requests.
    // Match the whole product/version, never an embedded Claude brand string.
    if let Some(version) = value.strip_prefix("claude-code/") {
        return !version.is_empty()
            && version.starts_with(|c: char| c.is_ascii_digit())
            && version
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'+'));
    }
    value.starts_with("claude-cli/") && value.contains(" (external,") && value.ends_with(')')
}

fn is_codex_user_agent(value: &str) -> bool {
    // Match the product token, never a substring or the optional originator
    // header alone. Codex CLI, exec, editor and desktop use different products.
    let Some((product, rest)) = value.split_once('/') else {
        return false;
    };
    let branded_codex = product
        .strip_prefix("Codex ")
        .is_some_and(|suffix| !suffix.trim().is_empty());
    if !branded_codex
        && !matches!(
            product,
            "codex_cli_rs"
                | "codex_exec"
                | "codex_vscode"
                | "codex-tui"
                | "codex_cli"
                | "codex"
                | "Codex"
                | "codex_desktop"
                | "codex_app"
        )
    {
        return false;
    }
    let version = rest.split_whitespace().next().unwrap_or_default();
    version.as_bytes().first().is_some_and(u8::is_ascii_digit)
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'_'))
}

async fn enforce_context(
    state: &Arc<AppState>,
    input_tokens: u64,
    compact: bool,
    model: &str,
    format: ErrorFormat,
    payload: &KiroPayload,
) -> Result<(), ApiError> {
    if let Err(limit) = check_context_limit(state, input_tokens, compact, model) {
        let mut error = ApiError::new(
            StatusCode::BAD_REQUEST,
            format!(
                "prompt is too long for model '{}': {} tokens > {}",
                limit.model, limit.input_tokens, limit.maximum
            ),
            format,
        );
        error.log_context.mapped_model = limit.model.clone();
        error.log_context.kiro_model = limit.model.clone();
        error.log_context.model_path = vec![limit.model];
        Err(error
            .with_context_diagnostics(state, payload, limit.maximum)
            .await)
    } else {
        Ok(())
    }
}

fn serialized_payload_bytes(
    payload: &crosslane_translate::KiroPayload,
    format: ErrorFormat,
) -> Result<usize, ApiError> {
    serde_json::to_vec(payload)
        .map(|payload| payload.len())
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to serialize upstream request: {error}"),
                format,
            )
        })
}

fn claude_validation_error(error: ValidationError) -> ApiError {
    // Claude Code reserves HTTP 413/request_too_large for its 32 MiB request
    // body limit and replaces the server message with a generic attachment
    // warning. Tool counts, schemas, and proxy working-set budgets are
    // semantic request validation errors, so keep their actionable messages
    // visible with HTTP 400.
    ApiError::new(
        StatusCode::BAD_REQUEST,
        error.to_string(),
        ErrorFormat::Claude,
    )
}

fn loaded_tool_count(payload: &crosslane_translate::KiroPayload) -> usize {
    payload
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context
        .as_ref()
        .map_or(0, |context| {
            context
                .tools
                .iter()
                .filter(|tool| tool.specification().is_some())
                .count()
        })
}

fn loaded_tool_bytes(payload: &crosslane_translate::KiroPayload) -> usize {
    payload
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context
        .as_ref()
        .and_then(|context| serde_json::to_vec(&context.tools).ok())
        .map_or(0, |tools| tools.len())
}

pub(super) fn loaded_tool_names(payload: &crosslane_translate::KiroPayload) -> HashSet<String> {
    payload
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context
        .as_ref()
        .into_iter()
        .flat_map(|context| context.tools.iter())
        .filter_map(|tool| tool.specification().map(|tool| tool.name.clone()))
        .collect()
}

pub(super) async fn remaining_tool_search_budget(
    state: &Arc<AppState>,
    payload: &crosslane_translate::KiroPayload,
    compact: bool,
) -> Result<crosslane_translate::ClaudeToolSearchBudget, String> {
    let config = state.config.current();
    let current_tools = loaded_tool_count(payload);
    let current_tool_tokens = state.tokenizer.estimate_kiro_tools(payload).await? as u64;
    let current_input_tokens = state.tokenizer.estimate_kiro_payload(payload).await? as u64;
    let current_payload_bytes = serde_json::to_vec(payload)
        .map_err(|error| format!("failed to serialize Tool Search payload: {error}"))?
        .len();
    let model = &payload
        .conversation_state
        .current_message
        .user_input_message
        .model_id;

    // JSON/schema-heavy text generally tokenizes below three bytes per token.
    // Using three here is deliberately conservative; the exact translated
    // request is checked again before dispatch.
    let token_bytes = u64::from(config.context.max_tool_input_tokens)
        .saturating_sub(current_tool_tokens)
        .saturating_mul(3);
    let context_bytes = context_maximum(state, compact, model)
        .saturating_sub(current_input_tokens)
        .saturating_mul(3);
    let payload_bytes = config
        .context
        .max_upstream_payload_bytes
        .saturating_sub(current_payload_bytes) as u64;
    Ok(crosslane_translate::ClaudeToolSearchBudget {
        max_tools: config
            .context
            .max_loaded_tools
            .min(crosslane_translate::validate::MAX_TOOLS)
            .saturating_sub(current_tools),
        max_bytes: usize::try_from(token_bytes.min(context_bytes).min(payload_bytes))
            .unwrap_or(usize::MAX),
    })
}

fn enforce_payload_budget(
    state: &Arc<AppState>,
    tool_tokens: u64,
    payload_bytes: usize,
    loaded_tools: usize,
    enforce_tool_search_budget: bool,
    format: ErrorFormat,
) -> Result<(), ApiError> {
    let context = &state.config.current().context;
    enforce_payload_budget_limits(
        context,
        tool_tokens,
        payload_bytes,
        loaded_tools,
        enforce_tool_search_budget,
        format,
    )
}

fn enforce_payload_budget_limits(
    context: &crosslane_core::config::ContextConfig,
    tool_tokens: u64,
    payload_bytes: usize,
    loaded_tools: usize,
    enforce_tool_search_budget: bool,
    format: ErrorFormat,
) -> Result<(), ApiError> {
    let budget_status = if format == ErrorFormat::Claude {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::PAYLOAD_TOO_LARGE
    };
    let max_loaded_tools = context
        .max_loaded_tools
        .min(crosslane_translate::validate::MAX_TOOLS);
    if loaded_tools > max_loaded_tools {
        return Err(ApiError::new(
            budget_status,
            format!(
                "too many loaded tools: {loaded_tools} > {max_loaded_tools}; defer more tools or reduce Tool Search references"
            ),
            format,
        ));
    }
    // `max_tool_input_tokens` is a working-set guard for deferred Tool Search.
    // Applying it to an ordinary request creates a second, much smaller
    // context limit even though these tool definitions are already included
    // in `estimate_kiro_payload` and checked against the model input window.
    if enforce_tool_search_budget && tool_tokens > u64::from(context.max_tool_input_tokens) {
        return Err(ApiError::new(
            budget_status,
            format!(
                "loaded Tool Search working set is too large: {tool_tokens} estimated tokens > {}; reduce always-loaded tools or their schemas",
                context.max_tool_input_tokens
            ),
            format,
        ));
    }
    if payload_bytes > context.max_upstream_payload_bytes {
        return Err(ApiError::new(
            budget_status,
            format!(
                "translated upstream payload is too large: {payload_bytes} bytes > {}; reduce the conversation, attached documents, or loaded tool schemas",
                context.max_upstream_payload_bytes
            ),
            format,
        ));
    }
    Ok(())
}

pub(super) fn check_context_limit(
    state: &Arc<AppState>,
    input_tokens: u64,
    compact: bool,
    model: &str,
) -> Result<(), ContextLimitError> {
    let maximum = context_maximum(state, compact, model);
    if input_tokens > maximum {
        Err(ContextLimitError {
            model: model.to_owned(),
            input_tokens,
            maximum,
        })
    } else {
        Ok(())
    }
}

fn context_maximum(state: &Arc<AppState>, compact: bool, model: &str) -> u64 {
    let context = &state.config.current().context;
    let ratio = if compact {
        context.compact_safe_input_ratio
    } else {
        context.safe_input_ratio
    };
    let model_maximum = model_token_limit(state, model, true).unwrap_or(context.max_input_tokens);
    (f64::from(model_maximum) * ratio) as u64
}

/// Apply the same Responses history policy whenever an attempt resolves a new
/// model. An unusable fallback must not discard history from the next attempt.
pub(in crate::http) async fn truncate_context_if_requested(
    state: &Arc<AppState>,
    payload: &mut KiroPayload,
    input_tokens: u64,
    compact: bool,
    model: &str,
) -> Result<u64, String> {
    let maximum = context_maximum(state, compact, model);
    if input_tokens <= maximum
        || !payload
            .model_request_intent
            .as_ref()
            .is_some_and(|intent| intent.automatic_history_truncation)
    {
        return Ok(input_tokens);
    }
    let mut candidate = payload.clone();
    let tokens = state
        .tokenizer
        .truncate_kiro_history(&mut candidate, maximum as usize)
        .await? as u64;
    if tokens <= maximum {
        *payload = candidate;
    }
    Ok(tokens)
}

fn compact_target_tokens(state: &Arc<AppState>, model: &str, trigger: u64) -> u64 {
    // Leave meaningful room for subsequent tool turns rather than compacting
    // to just below the trigger and immediately repeating the operation.
    compact_target_from_maximum(trigger)
        .min(context_maximum(state, true, model))
        .max(1)
}

fn compact_target_from_maximum(maximum: u64) -> u64 {
    maximum
        .saturating_mul(3)
        .checked_div(4)
        .unwrap_or(maximum)
        .max(1)
}

fn model_token_limit(state: &Arc<AppState>, model: &str, input: bool) -> Option<u32> {
    state
        .resolved_model_info(model)
        .and_then(|model| model.token_limits)
        .and_then(|limits| {
            if input {
                limits.max_input_tokens
            } else {
                limits.max_output_tokens
            }
        })
}

pub(super) fn estimated_credits(
    input_tokens: u64,
    max_tokens: u32,
    config: &crosslane_core::config::PoolConfig,
) -> f64 {
    // Kiro does not publish a deterministic token-to-credit formula. This is
    // intentionally a *reservation* heuristic; settlement replaces it with
    // server-reported credits whenever available. Operators can tune the
    // coefficient/cap if upstream pricing or their traffic mix changes.
    let estimated_tokens = input_tokens as f64
        + f64::from(max_tokens).min(f64::from(config.credit_estimate_output_token_cap));
    estimated_tokens / 1_000.0 * config.credit_estimate_per_1k_tokens
}

fn meter_error(error: MeterError, format: ErrorFormat) -> ApiError {
    let mut api = match error {
        MeterError::Unauthorized => {
            ApiError::new(StatusCode::UNAUTHORIZED, error.to_string(), format)
        }
        MeterError::LimitExceeded => {
            ApiError::new(StatusCode::TOO_MANY_REQUESTS, error.to_string(), format)
        }
        MeterError::DailyLimitExceeded => {
            ApiError::new(StatusCode::UNAUTHORIZED, error.to_string(), format)
        }
        MeterError::Persist(_) => {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string(), format)
        }
    };
    api.error_stage = "metering";
    api
}

fn reserve_credits(
    state: &Arc<AppState>,
    key_id: Option<&str>,
    estimate: f64,
    format: ErrorFormat,
) -> Result<CreditReservation, ApiError> {
    match state.meter.reserve(key_id, estimate) {
        Ok(reservation) => Ok(reservation),
        Err(MeterError::DailyLimitExceeded) => {
            Err(meter_error(MeterError::DailyLimitExceeded, format))
        }
        Err(error) => Err(meter_error(error, format)),
    }
}

fn upstream_error(error: ExecuteError, format: ErrorFormat) -> ApiError {
    match error {
        ExecuteError::ToolNameMismatch => ApiError::tool_name_mismatch(format),
        ExecuteError::UnsupportedWebSearch(reason) => {
            let mut error = hosted_web::unsupported_search(reason);
            error.format = format;
            error
        }
        ExecuteError::Pool(PoolError::QueueFull | PoolError::QueueTimeout) => {
            ApiError::retryable("Service busy, please retry", format)
        }
        ExecuteError::Pool(PoolError::NoAvailableAccount(_)) => {
            ApiError::retryable("Service temporarily unavailable, please retry", format)
        }
        ExecuteError::Pool(PoolError::CreditsExhausted) => {
            let mut error = ApiError::new(
                StatusCode::UNAUTHORIZED,
                "Credit quota exhausted; no compatible account currently has usable credit",
                format,
            );
            error.account_error = true;
            error.error_stage = "upstream_dispatch";
            error
        },
        ExecuteError::Meter(error) => meter_error(error, format),
        ExecuteError::ContextLimit(limit) => {
            let mut error = ApiError::new(
                StatusCode::BAD_REQUEST,
                format!(
                    "prompt is too long for resolved model '{}': {} tokens > {}",
                    limit.model, limit.input_tokens, limit.maximum
                ),
                format,
            );
            error.log_context.mapped_model = limit.model.clone();
            error.log_context.kiro_model = limit.model.clone();
            error.log_context.model_path = vec![limit.model];
            error
        }
        ExecuteError::ModelNotAllowed { provider_id, model } => ApiError::new(
            StatusCode::FORBIDDEN,
            format!("model {provider_id}/{model} is not allowed for this API key"),
            format,
        )
        .with_provider_id(&provider_id),
        ExecuteError::CrossProviderRoute {
            source_provider,
            target_provider,
            model,
        } => ApiError::new(
            StatusCode::BAD_REQUEST,
            format!(
                "internal {source_provider} request cannot route model {model} to provider {target_provider}"
            ),
            format,
        )
        .with_provider_id(&source_provider),
        ExecuteError::Upstream(error) => {
            upstream_api_error(error, RequestLogContext::default(), format)
        }
        ExecuteError::Dispatch(failure) => {
            upstream_api_error(failure.error, failure.context, format)
        }
    }
}

fn upstream_api_error(
    error: KiroError,
    context: RequestLogContext,
    format: ErrorFormat,
) -> ApiError {
    #[cfg(feature = "crs")]
    if error.message == super::crs::CONTENT_FILTERED_ERROR {
        let mut output = ApiError::new(StatusCode::BAD_REQUEST, error.message, format);
        output.log_context = Box::new(context);
        output.error_code = "upstream_content_filtered";
        output.error_stage = "upstream_stream";
        return output;
    }
    if error.is_local_overload() {
        let mut output = ApiError::new(StatusCode::SERVICE_UNAVAILABLE, error.message, format);
        output.retry_after = true;
        output.log_context = Box::new(context);
        output.error_code = "server_overloaded";
        output.error_stage = "admission";
        return output;
    }
    let model_resolution_error = error.endpoint == "model-resolution";
    let model_disabled = error.endpoint == "model-policy";
    let upstream_status = error.status;
    let upstream_throttle = error.is_throttle();
    let upstream_auth = error.is_auth();
    let upstream_quota = error.is_quota();
    let retryable = error.is_retriable() || upstream_auth || upstream_quota;
    let model_unavailable = error.is_model_temporarily_unavailable();
    let request_rejection = error.is_request_rejection();
    let account_error = !request_rejection
        && !model_unavailable
        && (error.is_auth()
            || error.is_quota()
            || error.is_throttle()
            || matches!(error.status, Some(500..=599)));
    let status = if model_resolution_error || model_disabled {
        StatusCode::BAD_REQUEST
    } else {
        // Preserve the supplier's HTTP status independently of error prose.
        // Transport failures have no upstream HTTP response and use local 502.
        error
            .status
            .and_then(|status| StatusCode::from_u16(status).ok())
            .unwrap_or(StatusCode::BAD_GATEWAY)
    };
    let message = if error.message.trim().is_empty() {
        "Upstream service error, please retry later".to_owned()
    } else {
        error.message
    };
    let mut output = ApiError::new(status, message, format);
    output.retry_after = !model_resolution_error && !model_disabled && retryable;
    output.log_context = Box::new(context);
    output.upstream_status = upstream_status;
    output.error_stage = "upstream_dispatch";
    output.account_error = account_error;
    if model_disabled {
        output.error_code = "model_disabled";
        output.error_stage = "model_policy";
        output.upstream_status = None;
        output.account_error = false;
    } else if model_resolution_error {
        output.error_code = "model_not_available";
        output.error_stage = "model_resolution";
        output.account_error = false;
    } else if model_unavailable {
        output.error_code = "upstream_model_unavailable";
    } else if upstream_auth {
        output.error_code = "upstream_authentication_failed";
    } else if upstream_quota && !upstream_throttle {
        output.error_code = "upstream_quota_exhausted";
    } else if upstream_status == Some(429) || upstream_throttle {
        output.error_code = "upstream_rate_limited";
    } else if upstream_status == Some(413) {
        output.error_code = "request_payload_exceeded";
    } else if !request_rejection
        || (upstream_status == Some(400)
            && !upstream_bad_request_is_actionable(&output.message, format))
    {
        output.error_code = "upstream_unavailable";
    }
    output
}

fn upstream_bad_request_is_actionable(message: &str, format: ErrorFormat) -> bool {
    // A broad upstream marker such as "ValidationException" can coexist with
    // a server, auth, or account failure. If the client-safe message is a
    // service error, do not label it as a bad client request.
    if cfg!(feature = "crs")
        && format == ErrorFormat::Claude
        && matches!(
            sanitize_error_message(message).as_str(),
            "Upstream service error, please retry later"
                | "Service temporarily unavailable, please retry"
                | "service unavailable"
        )
    {
        return false;
    }
    let lower = message.to_ascii_lowercase();
    is_context_length_error(message)
        || [
            "invalid request",
            "validationexception",
            "validation error",
            "malformed request",
            "unsupported model",
            "too many tools",
            "too many loaded tools",
            "tool definitions",
            "tool schema",
            "tool search working set",
            "loaded tools are too large",
            "payload too large",
            "payload is too large",
            "request too large",
            "request entity too large",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
}

struct ApiError {
    status: StatusCode,
    /// A wrapping stream can commit HTTP 200 before a later candidate fails.
    client_status: Option<u16>,
    message: Box<str>,
    format: ErrorFormat,
    allow: Option<&'static str>,
    authenticate: bool,
    retry_after: bool,
    suppress_model_stats: bool,
    request_id: Option<Box<str>>,
    log_context: Box<RequestLogContext>,
    error_code: &'static str,
    error_stage: &'static str,
    upstream_status: Option<u16>,
    account_error: bool,
    context_overflow: Option<Box<crate::stats::ContextOverflowDiagnostics>>,
    client_context: Option<Box<ClientContext>>,
    #[cfg(feature = "crs")]
    compaction_retried: bool,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>, format: ErrorFormat) -> Self {
        let message = message.into();
        let (error_code, error_stage) = classify_api_error(status, &message);
        Self {
            status,
            client_status: None,
            message: message.into_boxed_str(),
            format,
            allow: None,
            authenticate: false,
            retry_after: false,
            suppress_model_stats: false,
            request_id: None,
            log_context: Box::default(),
            error_code,
            error_stage,
            upstream_status: None,
            account_error: false,
            context_overflow: None,
            client_context: None,
            #[cfg(feature = "crs")]
            compaction_retried: false,
        }
    }

    fn overloaded(format: ErrorFormat) -> Self {
        let mut error = Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "server is overloaded, please retry",
            format,
        );
        error.retry_after = true;
        error
    }

    fn retryable(message: impl Into<String>, format: ErrorFormat) -> Self {
        let mut error = Self::new(StatusCode::SERVICE_UNAVAILABLE, message, format);
        error.retry_after = true;
        error
    }

    fn response_assembly(message: impl Into<String>, format: ErrorFormat) -> Self {
        let mut error = Self::new(StatusCode::INTERNAL_SERVER_ERROR, message, format);
        error.error_stage = "response_assembly";
        error
    }

    fn tool_name_mismatch(format: ErrorFormat) -> Self {
        let mut error = Self::new(
            StatusCode::BAD_GATEWAY,
            super::tool_names::TOOL_NAME_MISMATCH_MESSAGE,
            format,
        );
        error.error_code = super::tool_names::TOOL_NAME_MISMATCH_CODE;
        error.error_stage = "tool_name_validation";
        error
    }

    fn method_not_allowed(format: ErrorFormat, allow: &'static str) -> Self {
        let mut error = Self::new(StatusCode::METHOD_NOT_ALLOWED, "method not allowed", format);
        error.allow = Some(allow);
        error
    }

    fn with_request_id(mut self, request_id: &str) -> Self {
        self.request_id = Some(request_id.into());
        self
    }

    fn with_client_context(mut self, context: &ClientContext) -> Self {
        self.client_context = Some(Box::new(context.clone()));
        self
    }

    fn with_provider_id(mut self, provider_id: &str) -> Self {
        self.log_context.provider_id = provider_id.to_owned();
        self
    }

    fn with_provider_id_if_empty(mut self, provider_id: &str) -> Self {
        if self.log_context.provider_id.is_empty() {
            self.log_context.provider_id = provider_id.to_owned();
        }
        self
    }

    async fn with_context_diagnostics(
        mut self,
        state: &Arc<AppState>,
        payload: &KiroPayload,
        maximum_input_tokens: u64,
    ) -> Self {
        // Diagnostics are best-effort and must never replace the original
        // context error if the tokenizer worker is unavailable.
        if let Ok(tokens) = state.tokenizer.context_token_breakdown(payload).await {
            let overflow_scope = if tokens.minimum_input_tokens <= maximum_input_tokens {
                "history"
            } else if tokens.current_message_tokens > maximum_input_tokens {
                "current_message"
            } else {
                "required_context"
            }
            .to_owned();
            self.context_overflow = Some(Box::new(crate::stats::ContextOverflowDiagnostics {
                tokens,
                maximum_input_tokens,
                overflow_scope,
            }));
        }
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status;
        let mut envelope = error_envelope(
            self.format,
            status.as_u16(),
            &self.message,
            self.request_id.as_deref(),
        );
        if self.error_code == super::tool_names::TOOL_NAME_MISMATCH_CODE {
            envelope["error"]["code"] = json!(self.error_code);
            envelope["error"]["stage"] = json!(self.error_stage);
            envelope["error"]["retryable"] = json!(false);
        }
        if let Some(context) = &self.context_overflow {
            envelope["error"]["context"] = json!(context);
            if context.tokens.minimum_input_tokens > context.maximum_input_tokens {
                envelope["error"]["hint"] =
                    json!(super::context_recovery::LEGACY_INPUT_OVERFLOW_HINT);
            }
            if self.format == ErrorFormat::Claude
                && self.error_code == "context_length_exceeded"
                && self.upstream_status.is_none()
            {
                if let Some(recovery) = super::context_recovery::input_overflow_recovery(context) {
                    envelope["error"]["code"] = json!(self.error_code);
                    envelope["error"]["message"] =
                        json!(format!("{} {}", recovery.detail, recovery.hint));
                    envelope["error"]["detail"] = json!(recovery.detail);
                    envelope["error"]["hint"] = json!(recovery.hint);
                    envelope["error"]["retryable"] = json!(false);
                }
            }
        }
        #[cfg(feature = "crs")]
        if self.format == ErrorFormat::Claude {
            envelope["error"]["status"] = json!(status.as_u16());
            envelope["error"]["stage"] = json!(self.error_stage);
            if let Some(reason) = super::crs::context_rejection_reason(&self.message) {
                envelope["error"]["upstream_reason"] = json!(reason);
            }
            if self.compaction_retried {
                envelope["error"]["recovery"] = json!("compaction_retry_failed");
            }
            super::crs::enrich_error_envelope(
                &mut envelope,
                self.error_code,
                self.upstream_status,
                self.request_id.as_deref(),
            );
        }
        let mut response = (status, Json(envelope)).into_response();
        if let Some(request_id) = self.request_id.as_deref() {
            if let Ok(value) = HeaderValue::from_str(request_id) {
                response
                    .headers_mut()
                    .insert(header::HeaderName::from_static("request-id"), value);
            }
        }
        if let Some(allow) = self.allow {
            response
                .headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static(allow));
        }
        if self.authenticate {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        if self.retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        }
        for (name, value) in [
            ("x-crosslane-error-code", self.error_code),
            ("x-crosslane-error-stage", self.error_stage),
            (
                "x-crosslane-account-error",
                if self.account_error { "true" } else { "false" },
            ),
        ] {
            if let (Ok(name), Ok(value)) = (
                header::HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                response.headers_mut().insert(name, value);
            }
        }
        if let Some(status) = self.upstream_status {
            if let Ok(value) = HeaderValue::from_str(&status.to_string()) {
                response.headers_mut().insert(
                    header::HeaderName::from_static("x-crosslane-upstream-status"),
                    value,
                );
            }
        }
        response
    }
}

fn classify_api_error(status: StatusCode, message: &str) -> (&'static str, &'static str) {
    let lower = message.to_ascii_lowercase();
    if is_context_length_error(message) {
        return ("context_length_exceeded", "context_validation");
    }
    if (lower.contains("not supported") || lower.contains("unsupported"))
        && (lower.contains("tool")
            || lower.contains("strict")
            || lower.contains("allowed_callers")
            || lower.contains("eager"))
    {
        return ("unsupported_tool_protocol", "request_validation");
    }
    if status == StatusCode::PAYLOAD_TOO_LARGE && lower.contains("request body") {
        return ("request_body_too_large", "request_body");
    }
    let capacity_error =
        lower.contains("too many") || lower.contains("too large") || lower.contains("exceed");
    if capacity_error && (lower.contains("catalog") || lower.contains("deferred tool")) {
        return ("tool_catalog_too_large", "request_budget");
    }
    if is_tool_budget_error(&lower) {
        return ("tool_budget_exceeded", "request_budget");
    }
    if is_payload_budget_error(&lower) {
        return ("request_payload_exceeded", "request_budget");
    }
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        return ("request_payload_exceeded", "request_budget");
    }
    if status == StatusCode::BAD_REQUEST && lower.contains("tool") {
        return ("invalid_tool_protocol", "request_validation");
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return ("upstream_rate_limited", "upstream_dispatch");
    }
    if status == StatusCode::INTERNAL_SERVER_ERROR {
        return ("proxy_internal_error", "internal");
    }
    if matches!(
        status,
        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE
    ) {
        return ("upstream_unavailable", "upstream_dispatch");
    }
    ("invalid_request", "request_validation")
}

fn is_tool_budget_error(lower: &str) -> bool {
    lower.contains("too many tools")
        || lower.contains("too many loaded tools")
        || lower.contains("tool definitions are too large")
        || lower.contains("tool definitions exceed")
        || lower.contains("tool definition exceeds")
        || lower.contains("tool documentation exceeds")
        || lower.contains("tool search working set is too large")
        || lower.contains("loaded tools are too large")
        || lower.contains("tool schema payload too large")
}

fn is_payload_budget_error(lower: &str) -> bool {
    lower.contains("payload too large")
        || lower.contains("payload is too large")
        || lower.contains("request too large")
        || lower.contains("request entity too large")
}

#[cfg(test)]
mod model_tests;
