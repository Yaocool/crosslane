use std::collections::HashSet;
use std::convert::Infallible;
use std::error::Error as StdError;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use bytes::{Bytes, BytesMut};
use crosslane_core::config::ThinkingOutputFormat;
use crosslane_kiro::{EventStreamDecoder, KiroEvent, KiroResponse};
use crosslane_pool::AccountLease;
use crosslane_translate::{
    auto_continue_payload, tool_search_continue_payload_batch, web_search_continue_payload_batch,
    ClaudeContextEditStats, ClaudeServerToolEmission, ClaudeToolSearchBudget,
    ClaudeToolSearchCatalog, ClaudeToolSearchTrace, ClaudeWebSearchTrace, KiroPayload, KiroToolUse,
    WebSearchReplayCodec, WebSearchReplayError,
};
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_util::codec::Decoder;

use crate::meter::{now_secs, CreditReservation, UsageRecord};
use crate::state::AppState;
use crate::stats::{RequestDiagnostics, RequestLog, UpstreamAttemptLog};

use super::prompt_cache::{PromptCachePlan, PromptCacheProfile};
use super::response::{
    custom_tool_input, kiro_citation_key, kiro_visible_references, repair_json,
    web_search_citations, CompactionIterationUsage, DecodedResponse, OpenAiToolIdentity,
    StopSequenceFilter, ThinkingContentFilter, ToolLeakFilter,
};
use super::usage::{fallback_credits, fill_missing_usage, produced_output};

const WEB_SEARCH_REPLAY_FAILURE_MESSAGE: &str = "failed to protect web-search replay data";

#[derive(Clone)]
pub struct KeepaliveHub {
    sender: broadcast::Sender<()>,
}

impl KeepaliveHub {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(16);
        let ticker = sender.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(8));
            interval.tick().await;
            loop {
                interval.tick().await;
                let _result = ticker.send(());
            }
        });
        Self { sender }
    }

    fn subscribe(&self) -> broadcast::Receiver<()> {
        self.sender.subscribe()
    }
}

pub enum StreamProtocol {
    Claude,
    OpenAi,
}

impl StreamProtocol {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::OpenAi => "openai",
        }
    }
}

pub struct StreamContext {
    pub state: Arc<AppState>,
    /// Fixed effective account pool for the originating proxy service.
    pub account_ids: Arc<HashSet<String>>,
    pub lease: AccountLease,
    /// Access token used to create the currently consumed upstream response.
    pub upstream_access_token: String,
    pub reservation: CreditReservation,
    pub trace_id: String,
    pub request_id: String,
    pub user_agent: String,
    pub path: String,
    pub service_id: String,
    pub model: String,
    pub mapped_model: String,
    pub original_model: String,
    pub api_key_id: Option<String>,
    pub allowed_models: Vec<String>,
    /// A provider-aware route has already selected the Kiro target. Streaming
    /// retries must preserve that choice instead of drawing the mapping again.
    pub lock_model_mapping: bool,
    /// Automatic provider routing keeps the exact model throughout the stream.
    pub allow_model_fallback: bool,
    pub kiro_model: String,
    pub model_path: Vec<String>,
    pub model_mapping_rule: Option<String>,
    pub attempts: Vec<UpstreamAttemptLog>,
    pub input_tokens: u64,
    pub compact: bool,
    pub compaction_summary: Option<String>,
    pub(crate) compaction_replay:
        Option<(String, super::compaction_replay::CompactionReplaySource)>,
    /// Claude Code 2.1.260 retains compaction starts but ignores their deltas.
    pub compaction_in_start: bool,
    pub compaction_iteration: Option<CompactionIterationUsage>,
    /// Effective input size before proxy-triggered model-mapping compaction.
    pub auto_compaction_original_input_tokens: Option<u64>,
    /// Client-requested context edits that were actually applied locally.
    pub context_edit_stats: ClaudeContextEditStats,
    pub estimated_credits: f64,
    pub max_tokens: Option<u32>,
    pub stop_sequences: Vec<String>,
    pub started: Instant,
    pub prompt_cache: Option<PromptCacheProfile>,
    pub payload: KiroPayload,
    pub auto_continue_rounds: u32,
    pub buffer_tool_calls: bool,
    pub enable_tool_leak_filter: bool,
    /// Actual per-request decision. Beta headers only advertise capability and
    /// remain present when Claude Code sends `thinking.type = "disabled"`.
    pub thinking_enabled: bool,
    pub thinking_output_format: ThinkingOutputFormat,
    pub include_usage_chunk: bool,
    /// Kiro canonical web tool name -> original Claude server tool type.
    pub web_tool_names: std::collections::HashMap<String, String>,
    /// Deferred Claude tools retained outside the Kiro payload.
    pub tool_search: Option<Arc<ClaudeToolSearchCatalog>>,
    /// Aggregate Tool Search execution budget and already consumed operations.
    pub max_tool_search_operations: u32,
    pub tool_search_operations: u32,
    /// Zero disables proxy-executed Kiro MCP web search for this request.
    pub web_search_max_rounds: u32,
    /// True when the request explicitly supplied web_search.max_uses.
    pub web_search_client_limit: bool,
    /// Result-only blocks that complete pending server calls from prior turns.
    pub resumed_tool_searches: Vec<ClaudeToolSearchTrace>,
    pub resumed_web_searches: Vec<ClaudeWebSearchTrace>,
    pub resumed_server_events: Vec<super::response::ClaudeServerEvent>,
    pub diagnostics: RequestDiagnostics,
    /// Kiro-normalized tool name -> original OpenAI tool type and name.
    pub openai_tools: std::collections::HashMap<String, OpenAiToolIdentity>,
    /// Keeps decoded remote attachments charged to the global body budget for
    /// the full lifetime of streaming continuations.
    pub _attachment_guards: Vec<crate::state::BodyGuard>,
    pub _connection_guard: crate::state::AdmissionGuard,
    pub _admission_guard: crate::state::AdmissionGuard,
}

fn claude_initial_events(
    claude: &mut ClaudeState,
    compaction_summary: Option<&str>,
    resumed_server_events: &[super::response::ClaudeServerEvent],
    searches: &[ClaudeToolSearchTrace],
    web_searches: &[ClaudeWebSearchTrace],
) -> Result<Vec<String>, WebSearchReplayError> {
    let mut output = Vec::new();
    if let Some(summary) = compaction_summary {
        output.extend(claude.compaction(summary));
    }
    for event in resumed_server_events {
        output.extend(match event {
            super::response::ClaudeServerEvent::ToolSearch { index, .. } => searches
                .get(*index)
                .map(|trace| claude.tool_search(trace))
                .unwrap_or_default(),
            super::response::ClaudeServerEvent::WebSearch { index, .. } => {
                match web_searches.get(*index) {
                    Some(trace) => claude.web_search(trace)?,
                    None => Vec::new(),
                }
            }
        });
    }
    Ok(output)
}

fn prepend_pending_initial(pending: &mut Vec<String>, mut output: Vec<String>) -> Vec<String> {
    if output.is_empty() || pending.is_empty() {
        return output;
    }
    let mut combined = std::mem::take(pending);
    combined.append(&mut output);
    combined
}

fn build_claude_state(context: &StreamContext, prompt_cache: &PromptCachePlan) -> ClaudeState {
    let mut claude = ClaudeState::new(
        context.request_id.clone(),
        context.model.clone(),
        context.input_tokens,
        context.state.web_search_replay.clone(),
    );
    claude.openai_include_usage = context.include_usage_chunk;
    claude.auto_compaction_original_input_tokens = context.auto_compaction_original_input_tokens;
    claude.compaction_iteration = context.compaction_iteration;
    claude.compaction_in_start = context.compaction_in_start;
    claude
        .context_edit_stats
        .clone_from(&context.context_edit_stats);
    claude.set_prompt_cache_plan(prompt_cache);
    claude
}

#[derive(Debug, Clone)]
struct UpstreamStreamMetrics {
    started: Instant,
    last_chunk_at: Option<Instant>,
    chunks: u64,
    bytes: u64,
    events: u64,
}

impl UpstreamStreamMetrics {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            last_chunk_at: None,
            chunks: 0,
            bytes: 0,
            events: 0,
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    fn observe_chunk(&mut self, length: usize) {
        self.last_chunk_at = Some(Instant::now());
        self.chunks = self.chunks.saturating_add(1);
        self.bytes = self.bytes.saturating_add(length as u64);
    }

    fn observe_event(&mut self) {
        self.events = self.events.saturating_add(1);
    }
}

#[derive(Debug, Clone, Default)]
struct StreamFailureDiagnostics {
    kind: &'static str,
    transport_class: &'static str,
    transport_timeout: bool,
    transport_decode: bool,
    transport_body: bool,
    transport_connect: bool,
    source_chain: String,
    stream_elapsed_ms: u64,
    upstream_idle_ms: u64,
    chunk_seen: bool,
    chunks: u64,
    bytes: u64,
    events: u64,
    buffered_bytes: usize,
    configured_read_timeout_ms: u64,
}

impl StreamFailureDiagnostics {
    fn from_http_body(
        error: &reqwest::Error,
        metrics: &UpstreamStreamMetrics,
        buffered_bytes: usize,
        configured_read_timeout_ms: u64,
    ) -> Self {
        let transport_class = if error.is_timeout() {
            "timeout"
        } else if error.is_connect() {
            "connect"
        } else if error.is_decode() {
            "decode"
        } else if error.is_body() {
            "body"
        } else {
            "other"
        };
        Self::new(
            "http_body_read",
            transport_class,
            error,
            metrics,
            buffered_bytes,
            configured_read_timeout_ms,
            Some(error),
        )
    }

    fn from_event_stream(
        kind: &'static str,
        error: &(dyn StdError + 'static),
        metrics: &UpstreamStreamMetrics,
        buffered_bytes: usize,
        configured_read_timeout_ms: u64,
    ) -> Self {
        Self::new(
            kind,
            "not_applicable",
            error,
            metrics,
            buffered_bytes,
            configured_read_timeout_ms,
            None,
        )
    }

    fn from_upstream_event(
        metrics: &UpstreamStreamMetrics,
        buffered_bytes: usize,
        configured_read_timeout_ms: u64,
    ) -> Self {
        Self::from_metrics(
            "upstream_error_event",
            "not_applicable",
            String::new(),
            metrics,
            buffered_bytes,
            configured_read_timeout_ms,
        )
    }

    fn new(
        kind: &'static str,
        transport_class: &'static str,
        error: &(dyn StdError + 'static),
        metrics: &UpstreamStreamMetrics,
        buffered_bytes: usize,
        configured_read_timeout_ms: u64,
        reqwest_error: Option<&reqwest::Error>,
    ) -> Self {
        let mut diagnostics = Self::from_metrics(
            kind,
            transport_class,
            format_error_chain(error),
            metrics,
            buffered_bytes,
            configured_read_timeout_ms,
        );
        if let Some(error) = reqwest_error {
            diagnostics.transport_timeout = error.is_timeout();
            diagnostics.transport_decode = error.is_decode();
            diagnostics.transport_body = error.is_body();
            diagnostics.transport_connect = error.is_connect();
        }
        diagnostics
    }

    fn from_metrics(
        kind: &'static str,
        transport_class: &'static str,
        source_chain: String,
        metrics: &UpstreamStreamMetrics,
        buffered_bytes: usize,
        configured_read_timeout_ms: u64,
    ) -> Self {
        let now = Instant::now();
        let stream_elapsed_ms = elapsed_ms(metrics.started, now);
        let chunk_seen = metrics.last_chunk_at.is_some();
        let upstream_idle_ms = metrics
            .last_chunk_at
            .map_or(stream_elapsed_ms, |last_chunk| elapsed_ms(last_chunk, now));
        Self {
            kind,
            transport_class,
            source_chain,
            stream_elapsed_ms,
            upstream_idle_ms,
            chunk_seen,
            chunks: metrics.chunks,
            bytes: metrics.bytes,
            events: metrics.events,
            buffered_bytes,
            configured_read_timeout_ms,
            ..Self::default()
        }
    }
}

fn elapsed_ms(started: Instant, finished: Instant) -> u64 {
    finished.duration_since(started).as_millis() as u64
}

fn format_error_chain(error: &(dyn StdError + 'static)) -> String {
    const MAX_SOURCES: usize = 8;
    const MAX_CHARS: usize = 2_048;

    let mut messages = Vec::new();
    let mut current = Some(error);
    while let Some(error) = current.take().filter(|_| messages.len() < MAX_SOURCES) {
        let message = error.to_string().replace(['\r', '\n'], " ");
        if messages.last() != Some(&message) {
            messages.push(message);
        }
        current = error.source();
    }
    let chain = messages.join(" <- ");
    if chain.chars().count() <= MAX_CHARS {
        chain
    } else {
        let mut truncated = chain.chars().take(MAX_CHARS).collect::<String>();
        truncated.push_str("...");
        truncated
    }
}

mod response_loop;
pub use response_loop::response;

fn accumulate_usage(total: &mut crosslane_kiro::UsageInfo, addition: &crosslane_kiro::UsageInfo) {
    total.input_tokens = total.input_tokens.saturating_add(addition.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(addition.output_tokens);
    total.credits += addition.credits;
    total.cache_read_tokens = total
        .cache_read_tokens
        .saturating_add(addition.cache_read_tokens);
    total.cache_write_tokens = total
        .cache_write_tokens
        .saturating_add(addition.cache_write_tokens);
    total.reasoning_tokens = total
        .reasoning_tokens
        .saturating_add(addition.reasoning_tokens);
}

fn stream_event(
    protocol: &StreamProtocol,
    claude: &mut ClaudeState,
    event: &KiroEvent,
    created: i64,
    model: &str,
    thinking_format: ThinkingOutputFormat,
    openai_tools: &std::collections::HashMap<String, OpenAiToolIdentity>,
) -> Vec<String> {
    match protocol {
        StreamProtocol::Claude => claude.event(event),
        StreamProtocol::OpenAi => {
            openai_event(event, claude, created, model, thinking_format, openai_tools)
        }
    }
}

fn client_visible_event(
    event: &KiroEvent,
    stop_filter: &mut StopSequenceFilter,
) -> Option<KiroEvent> {
    match event {
        KiroEvent::AssistantResponse { content } => {
            let content = stop_filter.push(content);
            (!content.is_empty()).then_some(KiroEvent::AssistantResponse { content })
        }
        _ => Some(event.clone()),
    }
}

fn apply_stream_stop(decoded: &mut DecodedResponse, stop_filter: &StopSequenceFilter) {
    let Some(sequence) = stop_filter.matched().map(str::to_owned) else {
        return;
    };
    let visible_text = decoded
        .text
        .get(..stop_filter.visible_bytes())
        .unwrap_or(&decoded.text)
        .to_owned();
    decoded.stop_at_sequence(visible_text, sequence);
}

fn event_kind(event: &KiroEvent) -> &'static str {
    match event {
        KiroEvent::AssistantResponse { .. } => "assistant_response",
        KiroEvent::ToolUse { .. } => "tool_use",
        KiroEvent::Reasoning { .. } => "reasoning",
        KiroEvent::Citations { .. } => "citations",
        KiroEvent::MessageMetadata { .. } => "message_metadata",
        KiroEvent::Usage { .. } => "usage",
        KiroEvent::Error { .. } => "error",
        KiroEvent::Other { .. } => "other",
    }
}

fn event_is_tool_search(event: &KiroEvent, catalog: Option<&ClaudeToolSearchCatalog>) -> bool {
    let (Some(catalog), KiroEvent::ToolUse { name, .. }) = (catalog, event) else {
        return false;
    };
    catalog.is_search_tool(name)
}

fn event_is_web_search(event: &KiroEvent, max_rounds: u32) -> bool {
    max_rounds > 0 && matches!(event, KiroEvent::ToolUse { name, .. } if name == "web_search")
}

fn restore_web_tool_name(event: &mut KiroEvent, names: &std::collections::HashMap<String, String>) {
    if let KiroEvent::ToolUse { name, .. } = event {
        if let Some(original) = names.get(name) {
            name.clone_from(original);
        }
    }
}

fn restore_tool_event_name(
    event: &mut KiroEvent,
    names: &mut std::collections::HashMap<String, String>,
) {
    // Kiro can send the name only in the initial fragment. Resolve later
    // fragments by call id before deciding whether they must be buffered or
    // handled as custom/server tools. Keep the upstream name here, before any
    // client alias restoration, so a client alias cannot become a server tool.
    if let KiroEvent::ToolUse { id, name, .. } = event {
        if name.is_empty() {
            if let Some(original) = names.get(id) {
                name.clone_from(original);
            }
        } else {
            names.entry(id.clone()).or_insert_with(|| name.clone());
        }
    }
}

fn should_buffer_tool_event(
    event: &KiroEvent,
    configured: bool,
    identities: &std::collections::HashMap<String, OpenAiToolIdentity>,
) -> bool {
    match event {
        KiroEvent::ToolUse { name, .. } => {
            configured
                || identities
                    .get(name)
                    .is_some_and(|identity| identity.kind == "custom")
        }
        _ => false,
    }
}

fn openai_event(
    event: &KiroEvent,
    state: &mut ClaudeState,
    created: i64,
    model: &str,
    thinking_format: ThinkingOutputFormat,
    tool_identities: &std::collections::HashMap<String, OpenAiToolIdentity>,
) -> Vec<String> {
    let delta = match event {
        KiroEvent::AssistantResponse { content } => {
            state.visible_text.push_str(content);
            let prefix = if state.openai_thinking_open {
                state.openai_thinking_open = false;
                "</thinking>"
            } else {
                ""
            };
            json!({"content":format!("{prefix}{content}")})
        }
        KiroEvent::Reasoning {
            content,
            signature: _,
            redacted_content: _,
        } => match thinking_format {
            ThinkingOutputFormat::Openai => {
                let mut delta = json!({});
                if !content.is_empty() {
                    delta["reasoning_content"] = json!(content);
                }
                delta
            }
            ThinkingOutputFormat::Claude => {
                let prefix = if state.openai_thinking_open {
                    ""
                } else {
                    state.openai_thinking_open = true;
                    "<thinking>"
                };
                json!({"content":format!("{prefix}{content}")})
            }
        },
        // Chat Completions has no citation event shape, so preserve the source
        // information as ordinary visible text instead of dropping it.
        KiroEvent::Citations { citations } => {
            let fresh = citations
                .iter()
                .filter(|citation| {
                    state
                        .emitted_citation_links
                        .insert(kiro_citation_key(citation))
                })
                .cloned()
                .collect::<Vec<_>>();
            let references = kiro_visible_references(&fresh);
            if references.is_empty() {
                return Vec::new();
            }
            state.visible_text.push_str(&references);
            let prefix = if state.openai_thinking_open {
                state.openai_thinking_open = false;
                "</thinking>"
            } else {
                ""
            };
            json!({"content":format!("{prefix}{references}")})
        }
        KiroEvent::ToolUse {
            id,
            name,
            input_delta,
            stop,
        } => {
            let next = state.tool_indices.len();
            let index = *state.tool_indices.entry(id.clone()).or_insert(next);
            let identity = tool_identities.get(name);
            let original_name = identity.map_or(name.as_str(), |identity| identity.name.as_str());
            if identity.is_some_and(|identity| identity.kind == "custom") {
                // Custom input is free-form; never add a JSON function delta.
                state.tools_with_input.insert(id.clone());
                let input = if *stop {
                    custom_tool_input(input_delta)
                } else {
                    input_delta.clone()
                };
                json!({"tool_calls":[{
                    "index":index,"id":id,"type":"custom",
                    "custom":{"name":original_name,"input":input}
                }]})
            } else {
                let arguments =
                    if input_delta.trim().is_empty() && !state.tools_with_input.contains(id) {
                        if *stop {
                            "{}"
                        } else {
                            ""
                        }
                    } else {
                        input_delta.as_str()
                    };
                if !arguments.trim().is_empty() {
                    state.tools_with_input.insert(id.clone());
                }
                json!({"tool_calls":[{
                    "index":index,"id":id,"type":"function",
                    "function":{"name":original_name,"arguments":arguments}
                }]})
            }
        }
        _ => return Vec::new(),
    };
    let mut chunk = json!({
        "id":state.request_id,"object":"chat.completion.chunk",
        "created":created,"model":model,"choices":[{"index":0,"delta":delta,"finish_reason":Value::Null}]
    });
    if state.openai_include_usage {
        chunk["usage"] = Value::Null;
    }
    vec![format!("data: {chunk}\n\n")]
}

#[derive(Clone)]
struct ClaudeState {
    request_id: String,
    model: String,
    input_tokens: u64,
    auto_compaction_original_input_tokens: Option<u64>,
    compaction_iteration: Option<CompactionIterationUsage>,
    compaction_in_start: bool,
    context_edit_stats: ClaudeContextEditStats,
    message_started: bool,
    block: Option<(usize, &'static str)>,
    next_index: usize,
    tool_indices: std::collections::HashMap<String, usize>,
    tools_with_input: HashSet<String>,
    openai_thinking_open: bool,
    openai_include_usage: bool,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
    visible_text: String,
    emitted_citation_links: HashSet<String>,
    web_search_replay: WebSearchReplayCodec,
}

impl ClaudeState {
    fn new(
        request_id: String,
        model: String,
        input_tokens: u64,
        web_search_replay: WebSearchReplayCodec,
    ) -> Self {
        Self {
            request_id,
            model,
            input_tokens,
            auto_compaction_original_input_tokens: None,
            compaction_iteration: None,
            compaction_in_start: false,
            context_edit_stats: ClaudeContextEditStats::default(),
            message_started: false,
            block: None,
            next_index: 0,
            tool_indices: std::collections::HashMap::new(),
            tools_with_input: HashSet::new(),
            openai_thinking_open: false,
            openai_include_usage: false,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            visible_text: String::new(),
            emitted_citation_links: HashSet::new(),
            web_search_replay,
        }
    }

    fn set_prompt_cache_plan(&mut self, _plan: &PromptCachePlan) {
        // Streaming starts before Kiro reports usage. Do not publish the local
        // cache estimate as fact; the final message_delta carries real counters.
        self.cache_creation_input_tokens = 0;
        self.cache_read_input_tokens = 0;
    }

    fn ensure_message(&mut self, output: &mut Vec<String>) {
        if !self.message_started {
            let uncached_input_tokens = self
                .input_tokens
                .saturating_sub(self.cache_creation_input_tokens)
                .saturating_sub(self.cache_read_input_tokens);
            output.push(sse(&json!({
                "type":"message_start","message":{
                    "id":self.request_id,"type":"message","role":"assistant","content":[],
                    "model":self.model,"stop_reason":Value::Null,"stop_sequence":Value::Null,
                    "usage":{
                        "input_tokens":uncached_input_tokens,
                        "output_tokens":0,
                        "cache_creation_input_tokens":self.cache_creation_input_tokens,
                        "cache_read_input_tokens":self.cache_read_input_tokens
                    }
                }
            })));
            self.message_started = true;
        }
    }

    fn switch_block(
        &mut self,
        output: &mut Vec<String>,
        kind: &'static str,
        initial: Value,
    ) -> usize {
        self.ensure_message(output);
        if let Some((index, current)) = self.block {
            if current == kind && kind != "tool_use" {
                return index;
            }
            output.push(sse(&json!({"type":"content_block_stop","index":index})));
        }
        let index = self.next_index;
        self.next_index += 1;
        self.block = Some((index, kind));
        output.push(sse(&json!({
            "type":"content_block_start","index":index,"content_block":initial
        })));
        index
    }

    fn event(&mut self, event: &KiroEvent) -> Vec<String> {
        let mut output = Vec::new();
        match event {
            KiroEvent::AssistantResponse { content } => {
                self.visible_text.push_str(content);
                let index =
                    self.switch_block(&mut output, "text", json!({"type":"text","text":""}));
                output.push(sse(&json!({
                    "type":"content_block_delta","index":index,
                    "delta":{"type":"text_delta","text":content}
                })));
            }
            KiroEvent::Reasoning {
                content,
                signature,
                redacted_content,
            } => {
                if !content.is_empty() || signature.is_some() {
                    let index = self.switch_block(
                        &mut output,
                        "thinking",
                        json!({"type":"thinking","thinking":"","signature":""}),
                    );
                    if !content.is_empty() {
                        output.push(sse(&json!({
                            "type":"content_block_delta","index":index,
                            "delta":{"type":"thinking_delta","thinking":content}
                        })));
                    }
                    if let Some(signature) = signature {
                        output.push(sse(&json!({
                            "type":"content_block_delta","index":index,
                            "delta":{"type":"signature_delta","signature":signature}
                        })));
                    }
                }
                if let Some(data) = redacted_content {
                    let index = self.switch_block(
                        &mut output,
                        "redacted_thinking",
                        json!({"type":"redacted_thinking","data":data}),
                    );
                    output.push(sse(&json!({"type":"content_block_stop","index":index})));
                    self.block = None;
                }
            }
            KiroEvent::Citations { citations } => {
                let fresh = citations
                    .iter()
                    .filter(|citation| {
                        self.emitted_citation_links
                            .insert(kiro_citation_key(citation))
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                let fallback = kiro_visible_references(&fresh);
                if !fallback.is_empty() {
                    self.visible_text.push_str(&fallback);
                    let index =
                        self.switch_block(&mut output, "text", json!({"type":"text","text":""}));
                    output.push(sse(&json!({
                        "type":"content_block_delta","index":index,
                        "delta":{"type":"text_delta","text":fallback}
                    })));
                }
            }
            KiroEvent::ToolUse {
                id,
                name,
                input_delta,
                stop,
            } => {
                let index = if let Some(index) = self.tool_indices.get(id).copied() {
                    index
                } else {
                    let index = self.switch_block(
                        &mut output,
                        "tool_use",
                        json!({
                            "type":"tool_use","id":id,"name":name,"input":{}
                        }),
                    );
                    self.tool_indices.insert(id.clone(), index);
                    index
                };
                // Until JSON starts, whitespace-only fragments still mean no
                // arguments. Afterwards preserve every byte, including spaces
                // inside strings. An empty call keeps its initial input: {}.
                if !input_delta.trim().is_empty() {
                    self.tools_with_input.insert(id.clone());
                }
                if !input_delta.is_empty() && self.tools_with_input.contains(id) {
                    output.push(sse(&json!({
                        "type":"content_block_delta","index":index,
                        "delta":{"type":"input_json_delta","partial_json":input_delta}
                    })));
                }
                if *stop && self.block == Some((index, "tool_use")) {
                    output.push(sse(&json!({"type":"content_block_stop","index":index})));
                    self.block = None;
                }
            }
            _ => {}
        }
        output
    }

    fn compaction(&mut self, content: &str) -> Vec<String> {
        let mut output = Vec::new();
        let index = self.switch_block(
            &mut output,
            "compaction",
            json!({"type":"compaction","content":self.compaction_in_start.then_some(content)}),
        );
        // Claude Code's own accumulator ignores compaction_delta (verified on
        // 2.1.260). Publish the complete start block for that client. Do not also
        // send a delta: TS SDKs append it, while Python SDKs replace the content.
        // All other clients retain Anthropic's standard null-start/full-delta.
        if !self.compaction_in_start {
            output.push(sse(&json!({
                "type":"content_block_delta","index":index,
                "delta":{"type":"compaction_delta","content":content}
            })));
        }
        output.push(sse(&json!({"type":"content_block_stop","index":index})));
        self.block = None;
        output
    }

    fn tool_search(&mut self, search: &ClaudeToolSearchTrace) -> Vec<String> {
        let mut output = Vec::new();
        if search.emission != ClaudeServerToolEmission::ResultOnly {
            let index = self.switch_block(
                &mut output,
                "server_tool_use",
                json!({
                    "type":"server_tool_use",
                    "id":search.id,
                    "name":search.name,
                    "input":{}
                }),
            );
            output.push(sse(&json!({
                "type":"content_block_delta",
                "index":index,
                "delta":{"type":"input_json_delta","partial_json":search.input.to_string()}
            })));
            output.push(sse(&json!({"type":"content_block_stop","index":index})));
            self.block = None;
        }

        if search.emission == ClaudeServerToolEmission::Pending {
            return output;
        }

        let result = if let Some(error) = &search.error {
            json!({
                "type":"tool_search_tool_result_error",
                "error_code":error.code,
                "error_message":error.message
            })
        } else {
            json!({
                "type":"tool_search_tool_search_result",
                "tool_references":search.references.iter().map(|name| json!({
                    "type":"tool_reference","tool_name":name
                })).collect::<Vec<_>>()
            })
        };
        let index = self.switch_block(
            &mut output,
            "tool_search_tool_result",
            json!({
                "type":"tool_search_tool_result",
                "tool_use_id":search.id,
                "content":result
            }),
        );
        output.push(sse(&json!({"type":"content_block_stop","index":index})));
        self.block = None;
        output
    }

    fn web_search(
        &mut self,
        search: &ClaudeWebSearchTrace,
    ) -> Result<Vec<String>, WebSearchReplayError> {
        let mut output = Vec::new();
        if search.emission != ClaudeServerToolEmission::ResultOnly {
            let index = self.switch_block(
                &mut output,
                "server_tool_use",
                json!({
                    "type":"server_tool_use",
                    "id":search.id,
                    "name":"web_search",
                    "input":{}
                }),
            );
            output.push(sse(&json!({
                "type":"content_block_delta",
                "index":index,
                "delta":{"type":"input_json_delta","partial_json":search.input.to_string()}
            })));
            output.push(sse(&json!({"type":"content_block_stop","index":index})));
            self.block = None;
        }

        if search.emission == ClaudeServerToolEmission::Pending {
            return Ok(output);
        }

        let result = if let Some(error) = &search.error {
            json!({
                "type":"web_search_tool_result_error",
                "error_code":error.code
            })
        } else {
            Value::Array(
                search
                    .results
                    .iter()
                    .map(|result| {
                        Ok(json!({
                            "type":"web_search_result",
                            "url":result.url,
                            "title":result.title,
                            "page_age":Value::Null,
                            "encrypted_content":self.web_search_replay.try_encrypt(result)?
                        }))
                    })
                    .collect::<Result<Vec<_>, WebSearchReplayError>>()?,
            )
        };
        let index = self.switch_block(
            &mut output,
            "web_search_tool_result",
            json!({
                "type":"web_search_tool_result",
                "tool_use_id":search.id,
                "content":result,
                "caller":{"type":"direct"}
            }),
        );
        output.push(sse(&json!({"type":"content_block_stop","index":index})));
        self.block = None;
        Ok(output)
    }

    fn citations(
        &mut self,
        searches: &[ClaudeWebSearchTrace],
        answer_text: &str,
    ) -> Result<Vec<String>, WebSearchReplayError> {
        // Citation deltas must belong to an open text block. If a round ended
        // immediately after a server result there is no answer text to cite.
        if !matches!(self.block, Some((_, "text"))) {
            return Ok(Vec::new());
        }
        let citations = web_search_citations(searches, answer_text, &self.web_search_replay)?;
        if citations.is_empty() {
            return Ok(Vec::new());
        }
        let mut output = Vec::new();
        let index = self.switch_block(
            &mut output,
            "text",
            json!({"type":"text","text":"","citations":[]}),
        );
        for citation in citations {
            output.push(sse(&json!({
                "type":"content_block_delta",
                "index":index,
                "delta":{"type":"citations_delta","citation":citation}
            })));
        }
        Ok(output)
    }
}

#[allow(clippy::too_many_arguments)]
fn stream_finish(
    protocol: &StreamProtocol,
    claude: &mut ClaudeState,
    decoded: &DecodedResponse,
    created: i64,
    model: &str,
    max_tokens: Option<u32>,
    current_round_output_tokens: u64,
    thinking_format: ThinkingOutputFormat,
    include_usage_chunk: bool,
) -> Vec<String> {
    match protocol {
        StreamProtocol::Claude => {
            let mut output = Vec::new();
            claude.ensure_message(&mut output);
            if let Some((index, _kind)) = claude.block.take() {
                output.push(sse(&json!({"type":"content_block_stop","index":index})));
            }
            let stop = if let Some(reason) = decoded.stop_reason.as_deref() {
                reason
            } else if !decoded.tools.is_empty() {
                "tool_use"
            } else if max_tokens
                .is_some_and(|maximum| current_round_output_tokens >= u64::from(maximum))
            {
                "max_tokens"
            } else {
                "end_turn"
            };
            let uncached_input_tokens = decoded
                .usage
                .input_tokens
                .saturating_sub(decoded.usage.cache_read_tokens)
                .saturating_sub(decoded.usage.cache_write_tokens);
            let mut usage = json!({
                "input_tokens":uncached_input_tokens,
                "output_tokens":decoded.usage.output_tokens,
                "cache_creation_input_tokens":decoded.usage.cache_write_tokens,
                "cache_read_input_tokens":decoded.usage.cache_read_tokens
            });
            if !decoded.web_searches.is_empty() {
                usage["server_tool_use"] = json!({
                    "web_search_requests":decoded.web_searches.iter()
                        .filter(|search| search.executed)
                        .count()
                });
            }
            if let Some(compaction) = claude.compaction_iteration {
                usage["iterations"] = json!([
                    {
                        "type":"compaction",
                        "input_tokens":compaction.input_tokens,
                        "output_tokens":compaction.output_tokens
                    },
                    {
                        "type":"message",
                        "input_tokens":decoded.usage.input_tokens,
                        "output_tokens":decoded.usage.output_tokens
                    }
                ]);
            }
            let mut event = json!({
                "type":"message_delta","delta":{
                    "stop_reason":stop,
                    "stop_sequence":decoded.stop_sequence.as_deref()
                },
                "usage":usage
            });
            let mut applied_edits = claude.context_edit_stats.applied_edits();
            if let Some(original_input_tokens) = claude.auto_compaction_original_input_tokens {
                applied_edits.push(json!({
                        "type":"compact_20260112",
                        "reason":"model_mapping_overflow",
                        "original_input_tokens":original_input_tokens,
                        "compacted_input_tokens":claude.input_tokens
                }));
            }
            if !applied_edits.is_empty() {
                event["context_management"] = json!({"applied_edits":applied_edits});
            }
            output.push(sse(&event));
            output.push(sse(&json!({"type":"message_stop"})));
            output
        }
        StreamProtocol::OpenAi => {
            let mut output = Vec::new();
            // An unbuffered no-argument call can reach EOF without a stop
            // event. Complete its JSON before publishing a successful finish.
            let empty_calls = decoded
                .tools
                .values()
                .filter_map(|tool| {
                    if tool.input != "{}" || claude.tools_with_input.contains(&tool.id) {
                        return None;
                    }
                    let index = claude.tool_indices.get(&tool.id).copied()?;
                    claude.tools_with_input.insert(tool.id.clone());
                    Some(json!({"index":index,"function":{"arguments":"{}"}}))
                })
                .collect::<Vec<_>>();
            if !empty_calls.is_empty() {
                let mut chunk = json!({
                    "id":claude.request_id,"object":"chat.completion.chunk",
                    "created":created,"model":model,
                    "choices":[{"index":0,"delta":{"tool_calls":empty_calls},"finish_reason":Value::Null}]
                });
                if include_usage_chunk {
                    chunk["usage"] = Value::Null;
                }
                output.push(format!("data: {chunk}\n\n"));
            }
            if thinking_format == ThinkingOutputFormat::Claude && claude.openai_thinking_open {
                claude.openai_thinking_open = false;
                let mut chunk = json!({
                    "id":claude.request_id,"object":"chat.completion.chunk",
                    "created":created,"model":model,
                    "choices":[{"index":0,"delta":{"content":"</thinking>"},"finish_reason":Value::Null}]
                });
                if include_usage_chunk {
                    chunk["usage"] = Value::Null;
                }
                output.push(format!("data: {chunk}\n\n"));
            }
            let finish_reason = if !decoded.tools.is_empty() {
                "tool_calls"
            } else if decoded.stop_reason.as_deref() == Some("max_tokens")
                || max_tokens
                    .is_some_and(|maximum| current_round_output_tokens >= u64::from(maximum))
            {
                "length"
            } else {
                "stop"
            };
            let mut final_chunk = json!({
                "id":claude.request_id,"object":"chat.completion.chunk",
                "created":created,"model":model,"choices":[{"index":0,"delta":{},
                    "finish_reason":finish_reason}]
            });
            if include_usage_chunk {
                final_chunk["usage"] = Value::Null;
            }
            output.push(format!("data: {final_chunk}\n\n"));
            if include_usage_chunk {
                output.push(format!(
                    "data: {}\n\n",
                    json!({
                        "id":claude.request_id,"object":"chat.completion.chunk",
                        "created":created,"model":model,"choices":[],
                        "usage":{"prompt_tokens":decoded.usage.input_tokens,
                            "completion_tokens":decoded.usage.output_tokens,
                            "total_tokens":decoded.usage.input_tokens+decoded.usage.output_tokens,
                            "prompt_tokens_details":{
                                "cached_tokens":decoded.usage.cache_read_tokens
                            },
                            "completion_tokens_details":{
                                "reasoning_tokens":decoded.usage.reasoning_tokens
                            }}
                    })
                ));
            }
            output.push("data: [DONE]\n\n".into());
            output
        }
    }
}

fn stream_error(protocol: &StreamProtocol, request_id: &str, message: &str) -> String {
    classified_stream_error(protocol, message, request_id, None)
}

fn web_search_replay_failure(
    trace_id: &str,
    request_id: &str,
    user_agent: &str,
    error: &WebSearchReplayError,
) -> String {
    tracing::error!(
        trace_id,
        request_id,
        user_agent,
        %error,
        "failed to protect web-search replay data"
    );
    WEB_SEARCH_REPLAY_FAILURE_MESSAGE.to_owned()
}

fn classified_stream_error(
    protocol: &StreamProtocol,
    message: &str,
    request_id: &str,
    diagnostics: Option<&StreamFailureDiagnostics>,
) -> String {
    let details = classify_stream_failure(message, diagnostics);
    stream_error_response(protocol, message, details, request_id)
}

fn stream_error_response(
    protocol: &StreamProtocol,
    message: &str,
    details: StreamFailureDetails,
    request_id: &str,
) -> String {
    let safe = crosslane_translate::sanitize_error_message(message);
    let code = details.error_code;
    #[cfg(feature = "crs")]
    if matches!(protocol, StreamProtocol::Claude) {
        return sse(&super::crs::stream_error_envelope(
            message,
            code,
            details.error_stage,
            details.upstream_status,
            request_id,
        ));
    }
    match protocol {
        StreamProtocol::Claude => sse(&json!({
            "type":"error",
            "error":{"type":"api_error","message":safe,"code":code},
            "request_id":request_id,
        })),
        StreamProtocol::OpenAi => format!(
            "data: {}\n\n",
            json!({
                "error":{"type":"server_error","message":safe,"code":code},
                "request_id":request_id,
            })
        ),
    }
}

fn sse(value: &Value) -> String {
    format!(
        "event: {}\ndata: {}\n\n",
        value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message"),
        value
    )
}

async fn finish_accounting(
    mut context: StreamContext,
    endpoint: String,
    mut decoded: DecodedResponse,
    failure: Option<String>,
    stream_failure: Option<StreamFailureDiagnostics>,
    payload: &KiroPayload,
) {
    context.diagnostics.tool_search_rounds = decoded.tool_searches.len();
    context.diagnostics.tool_search_matches = decoded
        .tool_searches
        .iter()
        .map(|search| search.matched_count)
        .sum();
    context.diagnostics.search_requested_limit = decoded
        .tool_searches
        .iter()
        .map(|search| search.requested_limit)
        .max()
        .unwrap_or_default();
    context.diagnostics.search_returned_count = decoded
        .tool_searches
        .iter()
        .map(|search| search.references.len())
        .sum();
    context.diagnostics.search_budget_truncated = decoded
        .tool_searches
        .iter()
        .any(|search| search.budget_truncated);
    context.diagnostics.web_search_rounds = decoded
        .web_searches
        .iter()
        .filter(|search| search.executed)
        .count();
    context.diagnostics.web_search_results = decoded
        .web_searches
        .iter()
        .map(|search| search.results.len())
        .sum();
    // The HTTP status is already committed as 200 once an SSE stream starts;
    // retain the semantic failure in RequestLog.status and these stable fields.
    context.diagnostics.client_status = 200;
    let failure_details = failure
        .as_deref()
        .map(|message| classify_stream_failure(message, stream_failure.as_ref()));
    if let Some(details) = failure_details {
        context.diagnostics.upstream_status = details.upstream_status;
        context.diagnostics.error_code = details.error_code.into();
        context.diagnostics.error_stage = details.error_stage.into();
        context.diagnostics.failure_scope = details.scope.as_str().into();
        context.diagnostics.account_error = details.account_error();
    } else {
        context.diagnostics.upstream_status = Some(200);
    }
    if failure.is_none() {
        context
            .state
            .pool()
            .record_success(&context.lease.account_id())
            .await;
    }
    let produced_output = produced_output(&decoded);
    fill_missing_usage(&context.state, &mut decoded, payload).await;
    context.state.prompt_cache.apply(
        &context.lease.account_id(),
        context.prompt_cache.as_ref(),
        &mut decoded.usage,
    );
    let credits = if failure.is_some() && !produced_output {
        0.0
    } else if decoded.usage.credits > 0.0 {
        decoded.usage.credits
    } else {
        fallback_credits(
            &context.state,
            &context.kiro_model,
            decoded.usage.input_tokens,
            decoded.usage.output_tokens,
        )
    };
    context.lease.settle_credits(credits).await;
    context.diagnostics.credits_source = if decoded.usage.credits > 0.0 {
        "server"
    } else {
        "estimated"
    }
    .into();
    if let Err(error) = context
        .reservation
        .settle(UsageRecord {
            timestamp: now_secs(),
            provider_id: "kiro".into(),
            account_id: None,
            model: context.mapped_model.clone(),
            original_model: Some(context.original_model.clone()),
            kiro_model: Some(context.kiro_model.clone()),
            input_tokens: decoded.usage.input_tokens,
            output_tokens: decoded.usage.output_tokens,
            credits,
            cache_read_tokens: Some(decoded.usage.cache_read_tokens),
            cache_write_tokens: Some(decoded.usage.cache_write_tokens),
            reasoning_tokens: Some(decoded.usage.reasoning_tokens),
            token_usage_source: if decoded.usage.credits > 0.0 {
                "server"
            } else {
                "estimated"
            }
            .into(),
            provider_billing: None,
            path: context.path.clone(),
        })
        .await
    {
        tracing::error!(
            event = "proxy.stream.completed",
            trace_id = %context.trace_id,
            request_id = %context.request_id,
            provider_id = "kiro",
            user_agent = %context.user_agent,
            account_id = %context.lease.account_id(),
            %error,
            "failed to persist stream usage"
        );
    }
    let status = failure_details.map_or(200, |details| {
        if details.scope == StreamFailureScope::Proxy {
            500
        } else {
            502
        }
    });
    let duration_ms = context.started.elapsed().as_millis() as u64;
    let account = context.lease.account().await;
    let account_name = account.display_name().to_owned();
    if let Some(error) = failure.as_deref() {
        let details = failure_details
            .unwrap_or_else(|| classify_stream_failure(error, stream_failure.as_ref()));
        let failure_diagnostics = stream_failure.unwrap_or_default();
        let stream_failure_kind = if failure_diagnostics.kind.is_empty() {
            "none"
        } else {
            failure_diagnostics.kind
        };
        let transport_error_class = if failure_diagnostics.transport_class.is_empty() {
            "none"
        } else {
            failure_diagnostics.transport_class
        };
        tracing::error!(
            trace_id = %context.trace_id,
            request_id = %context.request_id,
            provider_id = "kiro",
            user_agent = %context.user_agent,
            account_id = %context.lease.account_id(),
            account_name,
            endpoint,
            model_path = %context.model_path.join(" -> "),
            mapping_rule = context.model_mapping_rule.as_deref().unwrap_or("none"),
            status,
            input_tokens = decoded.usage.input_tokens,
            output_tokens = decoded.usage.output_tokens,
            credits,
            duration_ms,
            error_code = details.error_code,
            error_stage = details.error_stage,
            failure_scope = details.scope.as_str(),
            account_error = details.account_error(),
            stream_failure_kind,
            transport_error_class,
            transport_timeout = failure_diagnostics.transport_timeout,
            transport_decode = failure_diagnostics.transport_decode,
            transport_body = failure_diagnostics.transport_body,
            transport_connect = failure_diagnostics.transport_connect,
            transport_error_chain = %failure_diagnostics.source_chain,
            upstream_stream_elapsed_ms = failure_diagnostics.stream_elapsed_ms,
            upstream_idle_ms = failure_diagnostics.upstream_idle_ms,
            upstream_chunk_seen = failure_diagnostics.chunk_seen,
            upstream_chunks = failure_diagnostics.chunks,
            upstream_bytes = failure_diagnostics.bytes,
            upstream_events = failure_diagnostics.events,
            upstream_buffered_bytes = failure_diagnostics.buffered_bytes,
            configured_stream_read_timeout_ms = failure_diagnostics.configured_read_timeout_ms,
            error = %crosslane_translate::sanitize_error_message(error),
            "client stream response completed with failure"
        );
    } else {
        tracing::info!(
            event = "proxy.stream.completed",
            trace_id = %context.trace_id,
            request_id = %context.request_id,
            provider_id = "kiro",
            user_agent = %context.user_agent,
            account_id = %context.lease.account_id(),
            account_name,
            endpoint,
            model_path = %context.model_path.join(" -> "),
            mapping_rule = context.model_mapping_rule.as_deref().unwrap_or("none"),
            status,
            input_tokens = decoded.usage.input_tokens,
            output_tokens = decoded.usage.output_tokens,
            credits,
            duration_ms,
            "client stream response completed"
        );
    }
    context.state.stats.record(RequestLog {
        timestamp: now_secs(),
        trace_id: context.trace_id,
        request_id: context.request_id,
        path: context.path,
        user_agent: context.user_agent,
        provider_id: "kiro".into(),
        model: context.mapped_model,
        original_model: context.original_model,
        kiro_model: context.kiro_model,
        account_id: context.lease.account_id(),
        account_name,
        endpoint,
        model_path: context.model_path,
        model_mapping_rule: context.model_mapping_rule,
        attempts: context.attempts,
        duration_ms,
        status,
        input_tokens: decoded.usage.input_tokens,
        output_tokens: decoded.usage.output_tokens,
        credits,
        error: failure,
        diagnostics: context.diagnostics,
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamFailureScope {
    Proxy,
    Client,
    Account,
    Model,
    Endpoint,
    Upstream,
}

impl StreamFailureScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Proxy => "proxy",
            Self::Client => "client",
            Self::Account => "account",
            Self::Model => "model",
            Self::Endpoint => "endpoint",
            Self::Upstream => "upstream",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct StreamFailureDetails {
    upstream_status: Option<u16>,
    error_code: &'static str,
    error_stage: &'static str,
    scope: StreamFailureScope,
}

impl StreamFailureDetails {
    fn account_error(self) -> bool {
        self.scope == StreamFailureScope::Account
    }

    fn is_auth_error(self) -> bool {
        self.error_code == "upstream_authentication_failed"
    }

    fn is_quota_error(self) -> bool {
        self.error_code == "upstream_quota_exhausted"
    }

    fn is_throttle_error(self) -> bool {
        self.error_code == "upstream_rate_limited"
    }

    fn is_model_unavailable(self) -> bool {
        self.error_code == "upstream_model_unavailable"
    }

    fn is_request_rejection(self) -> bool {
        matches!(
            self.error_code,
            "context_length_exceeded" | "tool_budget_exceeded" | "invalid_tool_protocol"
        )
    }
}

fn classify_stream_failure(
    message: &str,
    diagnostics: Option<&StreamFailureDiagnostics>,
) -> StreamFailureDetails {
    let lower = message.to_ascii_lowercase();
    let upstream_status = extract_upstream_status(&lower);

    if lower.starts_with("web_search is not supported:") {
        return StreamFailureDetails {
            upstream_status,
            error_code: "unsupported_tool_protocol",
            error_stage: "web_search",
            scope: StreamFailureScope::Proxy,
        };
    }

    if lower.contains(WEB_SEARCH_REPLAY_FAILURE_MESSAGE) {
        return StreamFailureDetails {
            upstream_status: None,
            error_code: "proxy_internal_error",
            error_stage: "response_assembly",
            scope: StreamFailureScope::Proxy,
        };
    }

    if let Some(diagnostics) = diagnostics {
        if diagnostics.kind == "http_body_read" {
            return StreamFailureDetails {
                upstream_status,
                error_code: if diagnostics.transport_timeout {
                    "upstream_idle_timeout"
                } else {
                    "upstream_transport_interrupted"
                },
                error_stage: "upstream_stream_transport",
                scope: StreamFailureScope::Endpoint,
            };
        }
        if matches!(diagnostics.kind, "event_stream_decode" | "event_stream_eof") {
            return StreamFailureDetails {
                upstream_status,
                error_code: "upstream_event_stream_corrupt",
                error_stage: "upstream_stream_decode",
                scope: StreamFailureScope::Upstream,
            };
        }
    }

    if crosslane_translate::error::is_context_length_error(message) {
        return StreamFailureDetails {
            upstream_status,
            error_code: "context_length_exceeded",
            error_stage: "context_validation",
            scope: StreamFailureScope::Client,
        };
    }
    if lower.contains("too many loaded tools")
        || lower.contains("tool definitions are too large")
        || lower.contains("payload is too large")
        || lower.contains("payload too large")
    {
        return StreamFailureDetails {
            upstream_status,
            error_code: "tool_budget_exceeded",
            error_stage: "request_budget",
            scope: StreamFailureScope::Client,
        };
    }
    if crosslane_kiro::client::text_is_model_temporarily_unavailable(message) {
        return StreamFailureDetails {
            upstream_status,
            error_code: "upstream_model_unavailable",
            error_stage: "upstream_stream",
            scope: StreamFailureScope::Model,
        };
    }
    if crosslane_kiro::client::text_is_throttle_error(message) {
        return StreamFailureDetails {
            upstream_status: upstream_status.or(Some(429)),
            error_code: "upstream_rate_limited",
            error_stage: "upstream_stream",
            scope: StreamFailureScope::Model,
        };
    }
    if crosslane_kiro::client::text_is_auth_error(message) {
        return StreamFailureDetails {
            upstream_status,
            error_code: "upstream_authentication_failed",
            error_stage: "upstream_stream",
            scope: StreamFailureScope::Account,
        };
    }
    if crosslane_kiro::client::text_is_quota_error(message) {
        return StreamFailureDetails {
            upstream_status,
            error_code: "upstream_quota_exhausted",
            error_stage: "upstream_stream",
            scope: StreamFailureScope::Account,
        };
    }
    if crosslane_kiro::client::text_is_request_rejection(message) {
        return StreamFailureDetails {
            upstream_status,
            error_code: "invalid_tool_protocol",
            error_stage: "upstream_stream",
            scope: StreamFailureScope::Client,
        };
    }
    StreamFailureDetails {
        upstream_status,
        error_code: "upstream_unavailable",
        error_stage: "upstream_stream",
        scope: StreamFailureScope::Upstream,
    }
}

async fn record_account_scoped_stream_failure(
    state: &Arc<AppState>,
    trace_id: &str,
    request_id: &str,
    user_agent: &str,
    account_id: &str,
    details: StreamFailureDetails,
) {
    if details.is_quota_error() {
        state.pool().record_quota_error(account_id).await;
        if state
            .pool()
            .get(account_id)
            .await
            .is_some_and(|runtime| runtime.health() == crosslane_pool::AccountHealth::Exhausted)
        {
            if let Err(error) = crate::tasks::persist_pool_accounts(state).await {
                tracing::error!(
                    trace_id,
                    request_id,
                    user_agent,
                    account_id,
                    %error,
                    "failed to persist stream quota exhaustion"
                );
            }
            crate::alerts::sync_account_quota(state, account_id).await;
            crate::alerts::sync_service_quota(state).await;
        }
    } else if details.account_error() {
        state.pool().record_error(account_id).await;
    }
}

fn extract_upstream_status(message: &str) -> Option<u16> {
    ["returned some(", "http status ", "status code "]
        .iter()
        .find_map(|marker| {
            let start = message.find(marker)? + marker.len();
            let digits = message[start..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>();
            let status = digits.parse::<u16>().ok()?;
            (400..=599).contains(&status).then_some(status)
        })
}

#[cfg(test)]
mod tests;
