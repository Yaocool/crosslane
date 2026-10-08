//! Responses output encoding over the existing OpenAI execution stream.
//! Keeping this adapter outside the upstream loop preserves its retries,
//! backpressure, accounting, filters, and cancellation behavior.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::http::header;
use axum::response::Response;
use bytes::BytesMut;
use crosslane_translate::{ResponsesRequest, ResponsesToolName};
use futures::StreamExt;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::meter::now_secs;

pub(super) fn is_responses_path(path: &str) -> bool {
    matches!(path, "/v1/responses" | "/responses")
}

const RESPONSES_SESSION_TTL: Duration = Duration::from_secs(30 * 60);
const MAX_RESPONSES_SESSIONS: usize = 256;
const MAX_RESPONSES_SESSION_BYTES: usize = 2 * 1024 * 1024;
const MAX_RESPONSES_SESSION_TOTAL_BYTES: usize = 32 * 1024 * 1024;
const RESPONSES_STORE_FAILED: &str =
    "unable to store response state within the configured in-memory limit";

/// A stored Responses conversation is scoped to both the proxy service and
/// authenticated API key. Response IDs are random, but scoping them prevents a
/// caller on another service or credential from replaying a known ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResponsesSessionOwner {
    service_id: String,
    api_key_id: Option<String>,
}

impl ResponsesSessionOwner {
    pub(crate) fn new(service_id: String, api_key_id: Option<String>) -> Self {
        Self {
            service_id,
            api_key_id,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct StoredResponsesSession {
    owner: ResponsesSessionOwner,
    /// The final UUID sent to Kiro, not the client hint that produced it.
    /// Persisting the resolved value keeps `previous_response_id` authoritative
    /// even if a later request drops or changes its session-affinity header.
    conversation_id: Option<String>,
    history: Vec<Value>,
    tools: Vec<Value>,
    tool_choice: Option<Value>,
    parallel_tool_calls: Option<bool>,
    last_accessed: Instant,
    stored_bytes: usize,
}

impl StoredResponsesSession {
    fn from_request(
        request: &ResponsesRequest,
        tools: Vec<Value>,
        owner: ResponsesSessionOwner,
        conversation_id: Option<String>,
    ) -> Self {
        Self {
            owner,
            conversation_id,
            history: response_input_history(request),
            tools,
            tool_choice: request.tool_choice.clone(),
            parallel_tool_calls: request.parallel_tool_calls,
            last_accessed: Instant::now(),
            stored_bytes: 0,
        }
    }

    pub(crate) fn conversation_id(&self) -> Option<&str> {
        self.conversation_id.as_deref()
    }

    fn refresh_size(&mut self) -> bool {
        let Ok(serialized) = serde_json::to_vec(&(
            &self.conversation_id,
            &self.history,
            &self.tools,
            &self.tool_choice,
            self.parallel_tool_calls,
        )) else {
            return false;
        };
        self.stored_bytes = serialized.len();
        self.stored_bytes <= MAX_RESPONSES_SESSION_BYTES
    }
}

/// Small, process-local store for the stateful subset of the Responses API.
/// It deliberately has no disk backing: prompts and tool results disappear on
/// restart, instead of becoming a new durable-data surface for the proxy.
#[derive(Clone, Default)]
pub(crate) struct ResponsesSessionStore {
    entries: Arc<Mutex<HashMap<String, StoredResponsesSession>>>,
}

impl ResponsesSessionStore {
    pub(crate) fn get(
        &self,
        response_id: &str,
        owner: &ResponsesSessionOwner,
    ) -> Option<StoredResponsesSession> {
        let now = Instant::now();
        let mut entries = self.lock_entries();
        Self::prune(&mut entries, now);
        let session = entries.get_mut(response_id)?;
        if &session.owner != owner {
            return None;
        }
        session.last_accessed = now;
        Some(session.clone())
    }

    fn insert(
        &self,
        response_id: &str,
        mut session: StoredResponsesSession,
        output: &[Value],
    ) -> bool {
        session.history.extend(output.iter().cloned());
        session.last_accessed = Instant::now();
        if !session.refresh_size() {
            return false;
        }
        let mut entries = self.lock_entries();
        Self::prune(&mut entries, session.last_accessed);
        if let Some(replaced) = entries.remove(response_id) {
            debug_assert!(replaced.stored_bytes <= MAX_RESPONSES_SESSION_TOTAL_BYTES);
        }
        let mut total_bytes = entries
            .values()
            .map(|entry| entry.stored_bytes)
            .sum::<usize>();
        while !entries.is_empty()
            && (entries.len() >= MAX_RESPONSES_SESSIONS
                || total_bytes.saturating_add(session.stored_bytes)
                    > MAX_RESPONSES_SESSION_TOTAL_BYTES)
        {
            if let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_accessed)
                .map(|(id, _)| id.clone())
            {
                if let Some(removed) = entries.remove(&oldest) {
                    total_bytes = total_bytes.saturating_sub(removed.stored_bytes);
                }
            }
        }
        if total_bytes.saturating_add(session.stored_bytes) > MAX_RESPONSES_SESSION_TOTAL_BYTES {
            return false;
        }
        entries.insert(response_id.into(), session);
        true
    }

    fn lock_entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, StoredResponsesSession>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn prune(entries: &mut HashMap<String, StoredResponsesSession>, now: Instant) {
        entries.retain(|_, session| {
            now.saturating_duration_since(session.last_accessed) <= RESPONSES_SESSION_TTL
        });
    }
}

/// Merge a stored response history with a new stateful request. The translator
/// then validates call/output pairing across the complete reconstructed input.
pub(crate) fn resume_responses_request(
    request: &mut ResponsesRequest,
    session: StoredResponsesSession,
) {
    // An `additional_tools` item is the Responses Lite spelling of an
    // explicitly supplied catalog, so it must prevent inheritance just like a
    // present top-level `tools` field (including an empty catalog).
    let declares_additional_tools = has_additional_tools(&request.input);
    let mut input = session.history;
    input.extend(response_input_items(&request.input));
    request.input = Value::Array(input);
    let inherits_tools = request.tools.is_none() && !declares_additional_tools;
    if inherits_tools {
        request.tools = Some(session.tools);
    }
    if request.tool_choice.as_ref().is_none_or(Value::is_null) && inherits_tools {
        request.tool_choice = session.tool_choice;
    }
    if request.parallel_tool_calls.is_none() {
        request.parallel_tool_calls = session.parallel_tool_calls;
    }
}

fn response_input_history(request: &ResponsesRequest) -> Vec<Value> {
    // `instructions` deliberately does not become stored history. The
    // Responses API lets a continuation replace instructions on its next
    // request, so persisting it as a system message would make it impossible
    // to remove or swap.
    response_input_items(&request.input)
        .into_iter()
        .filter(|item| item.get("type").and_then(Value::as_str) != Some("additional_tools"))
        .collect()
}

fn has_additional_tools(input: &Value) -> bool {
    input.as_array().is_some_and(|items| {
        items
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some("additional_tools"))
    })
}

fn response_input_items(input: &Value) -> Vec<Value> {
    match input {
        Value::String(text) => {
            vec![json!({"type":"message","role":"user","content":text})]
        }
        Value::Array(items) => items.clone(),
        _ => Vec::new(),
    }
}

pub(super) struct ResponsesOptions {
    template: Value,
    tool_names: HashMap<String, ResponsesToolName>,
    emit_reasoning_summary: bool,
    session: Option<(ResponsesSessionStore, StoredResponsesSession)>,
    inherited_conversation_id: Option<String>,
}

impl ResponsesOptions {
    pub fn new(
        request: &ResponsesRequest,
        tools: Vec<Value>,
        tool_names: HashMap<String, ResponsesToolName>,
        session: Option<(ResponsesSessionStore, ResponsesSessionOwner)>,
        inherited_conversation_id: Option<String>,
    ) -> Self {
        let response_id = format!("resp_{}", Uuid::new_v4().simple());
        // Responses are stored by default. `store: false` is the explicit
        // opt-out used by stateless and zero-retention clients.
        let stores_session = request.store != Some(false) && session.is_some();
        let session = stores_session
            .then_some(session)
            .flatten()
            .map(|(store, owner)| {
                (
                    store,
                    StoredResponsesSession::from_request(
                        request,
                        tools.clone(),
                        owner,
                        inherited_conversation_id.clone(),
                    ),
                )
            });
        Self {
            template: json!({
                "id":response_id,
                "object":"response", "created_at":now_secs(),
                "status":"in_progress", "error":null, "incomplete_details":null,
                "model":request.model, "instructions":request.instructions,
                "output":[], "usage":null, "store":session.is_some(), "background":false,
                "previous_response_id":request.previous_response_id,
                "max_output_tokens":request.max_output_tokens,
                "parallel_tool_calls":request.parallel_tool_calls.unwrap_or(true),
                "tool_choice":request.tool_choice.as_ref().unwrap_or(&json!("auto")),
                "tools":tools, "reasoning":request.reasoning,
                "text":request.text.as_ref().map(|text| json!({
                    "format":text.format.as_ref().unwrap_or(&json!({"type":"text"})),
                    "verbosity":text.verbosity.as_deref().unwrap_or("medium")
                })).unwrap_or_else(|| json!({"format":{"type":"text"}})),
                "temperature":request.temperature.unwrap_or(1.0),
                "top_p":request.top_p.unwrap_or(1.0),
                "truncation":request.truncation.as_deref().unwrap_or("disabled"), "service_tier":"auto",
                "max_tool_calls":request.max_tool_calls,
                "metadata":request.metadata.as_ref().unwrap_or(&json!({}))
            }),
            tool_names,
            // `none` is used by Codex as its explicit turn-level opt-out. Keep
            // accepting future summary modes loosely, but do not manufacture
            // reasoning items after the client asked us to hide them.
            emit_reasoning_summary: request
                .reasoning
                .as_ref()
                .and_then(|reasoning| reasoning.summary.as_deref())
                != Some("none"),
            session,
            inherited_conversation_id,
        }
    }

    pub fn id(&self) -> &str {
        self.template["id"].as_str().expect("response id")
    }

    pub(super) fn automatic_truncation(&self) -> bool {
        self.template["truncation"] == "auto"
    }

    pub(super) fn inherited_conversation_id(&self) -> Option<&str> {
        self.inherited_conversation_id.as_deref()
    }

    /// A new stored chain uses its response ID only as an input to the stable
    /// conversation-ID derivation. Continuations reuse the already-resolved ID.
    pub(super) fn conversation_hint(&self) -> Option<&str> {
        self.session.as_ref().map(|_| self.id())
    }

    pub(super) fn set_conversation_id(&mut self, conversation_id: Option<String>) {
        if let Some((_, session)) = &mut self.session {
            session.conversation_id = conversation_id;
        }
    }

    fn envelope(&self, output: Vec<Value>, status: &str, usage: Value, error: Value) -> Value {
        let mut response = self.template.clone();
        if status == "failed" && self.session.is_some() {
            response["store"] = json!(false);
        }
        response["output"] = json!(output);
        response["status"] = json!(status);
        response["usage"] = usage;
        response["error"] = error;
        if status == "incomplete" {
            response["incomplete_details"] = json!({"reason":"max_output_tokens"});
        }
        response
    }

    fn finalize(
        &self,
        output: Vec<Value>,
        status: &str,
        usage: Value,
    ) -> Result<Value, &'static str> {
        if let Some((store, session)) = &self.session {
            if session.conversation_id.is_none()
                || !store.insert(self.id(), session.clone(), &output)
            {
                return Err(RESPONSES_STORE_FAILED);
            }
        }
        Ok(self.envelope(output, status, usage, Value::Null))
    }

    fn tool(&self, call: &Value) -> ItemContent {
        let custom = call["type"] == "custom";
        let body = &call[if custom { "custom" } else { "function" }];
        let name = body["name"].as_str().unwrap_or_default();
        let identity = self.tool_names.get(name);
        ItemContent::Tool {
            custom,
            call_id: call["id"].as_str().unwrap_or_default().into(),
            name: identity
                .map_or(name, |identity| identity.name.as_str())
                .into(),
            namespace: identity.and_then(|identity| identity.namespace.clone()),
            input: body[if custom { "input" } else { "arguments" }]
                .as_str()
                .unwrap_or_default()
                .into(),
        }
    }
}

pub(super) fn json_response(chat: Value, options: ResponsesOptions) -> Result<Value, &'static str> {
    let message = &chat["choices"][0]["message"];
    let mut output = Vec::new();
    if options.emit_reasoning_summary {
        if let Some(text) = message["reasoning_content"]
            .as_str()
            .filter(|text| has_substantive_reasoning_summary(text))
        {
            output.push(OutputItem::new(ItemContent::Reasoning(text.into())).json("completed"));
        }
    }
    if let Some(text) = message["content"].as_str().filter(|text| !text.is_empty()) {
        output.push(OutputItem::new(ItemContent::Message(text.into())).json("completed"));
    }
    if let Some(calls) = message["tool_calls"].as_array() {
        output.extend(
            calls
                .iter()
                .map(|call| OutputItem::new(options.tool(call)).json("completed")),
        );
    }
    let incomplete = chat["choices"][0]["finish_reason"] == "length";
    let status = if incomplete {
        "incomplete"
    } else {
        "completed"
    };
    if output.is_empty() {
        output.push(OutputItem::new(ItemContent::Message(String::new())).json(status));
    } else if incomplete {
        for item in &mut output {
            if item["type"] == "message" {
                item["status"] = json!("incomplete");
            }
        }
    }
    options.finalize(output, status, usage(&chat["usage"]))
}

enum ItemContent {
    Message(String),
    Reasoning(String),
    Tool {
        custom: bool,
        call_id: String,
        name: String,
        namespace: Option<String>,
        input: String,
    },
}

struct OutputItem {
    id: String,
    content: ItemContent,
    status: &'static str,
}

impl OutputItem {
    fn new(content: ItemContent) -> Self {
        let prefix = match &content {
            ItemContent::Message(_) => "msg",
            ItemContent::Reasoning(_) => "rs",
            ItemContent::Tool { custom: true, .. } => "ctc",
            ItemContent::Tool { .. } => "fc",
        };
        Self {
            id: format!("{prefix}_{}", Uuid::new_v4().simple()),
            content,
            status: "in_progress",
        }
    }

    fn json(&self, status: &str) -> Value {
        match &self.content {
            ItemContent::Message(text) => json!({
                "id":self.id,"type":"message","role":"assistant","status":status,
                "content":if status == "in_progress" && text.is_empty() { Vec::new() } else { vec![text_part(text)] }
            }),
            ItemContent::Reasoning(text) => json!({
                "id":self.id,"type":"reasoning",
                "summary":if text.is_empty() { Vec::new() } else { vec![json!({"type":"summary_text","text":text})] }
            }),
            ItemContent::Tool {
                custom,
                call_id,
                name,
                namespace,
                input,
            } => {
                let mut item = json!({
                    "id":self.id,"type":if *custom { "custom_tool_call" } else { "function_call" },
                    "call_id":call_id,"name":name,"status":status
                });
                item[if *custom { "input" } else { "arguments" }] = json!(input);
                if let Some(namespace) = namespace {
                    item["namespace"] = json!(namespace);
                }
                item
            }
        }
    }
}

fn text_part(text: &str) -> Value {
    json!({"type":"output_text","text":text,"annotations":[],"logprobs":[]})
}

/// Kiro can emit empty summary markers while retaining private reasoning. A
/// Responses reasoning item for those markers leaves Codex with a permanent
/// `Reasoning summary: ...` shell, so only publish text with visible meaning.
fn has_substantive_reasoning_summary(text: &str) -> bool {
    let without_empty_comments = text.replace("<!-- -->", "");
    let compact = without_empty_comments
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    !compact.is_empty()
        && !compact
            .chars()
            .all(|character| matches!(character, '.' | '…'))
}

fn usage(chat: &Value) -> Value {
    let input = chat["prompt_tokens"].as_u64().unwrap_or_default();
    let output = chat["completion_tokens"].as_u64().unwrap_or_default();
    json!({
        "input_tokens":input,"output_tokens":output,"total_tokens":input.saturating_add(output),
        "input_tokens_details":{"cached_tokens":chat["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or_default()},
        "output_tokens_details":{"reasoning_tokens":chat["completion_tokens_details"]["reasoning_tokens"].as_u64().unwrap_or_default()}
    })
}

struct ResponsesStream {
    options: ResponsesOptions,
    items: Vec<OutputItem>,
    active_text: Option<usize>,
    pending_reasoning: String,
    tool_indices: HashMap<u64, usize>,
    sequence: u64,
    usage: Value,
    finish_reason: Option<String>,
    terminal: bool,
}

impl ResponsesStream {
    fn new(options: ResponsesOptions) -> Self {
        Self {
            options,
            items: Vec::new(),
            active_text: None,
            pending_reasoning: String::new(),
            tool_indices: HashMap::new(),
            sequence: 0,
            usage: Value::Null,
            finish_reason: None,
            terminal: false,
        }
    }

    fn event(&mut self, mut value: Value) -> String {
        value["sequence_number"] = json!(self.sequence);
        self.sequence += 1;
        format!(
            "event: {}\ndata: {value}\n\n",
            value["type"].as_str().expect("event type")
        )
    }

    fn start(&mut self) -> Vec<String> {
        ["response.created", "response.in_progress"]
            .into_iter()
            .map(|kind| self.event(json!({"type":kind,"response":self.options.template})))
            .collect()
    }

    fn add(&mut self, content: ItemContent, events: &mut Vec<String>) -> usize {
        let index = self.items.len();
        let item = OutputItem::new(content);
        events.push(self.event(json!({"type":"response.output_item.added","output_index":index,"item":item.json("in_progress")})));
        self.items.push(item);
        index
    }

    fn close(&mut self, index: usize, status: &'static str, events: &mut Vec<String>) {
        if self.items[index].status != "in_progress" {
            return;
        }
        let item = &self.items[index];
        let mut values = match &item.content {
            ItemContent::Message(text) => vec![
                json!({"type":"response.output_text.done","item_id":item.id,"output_index":index,"content_index":0,"text":text,"logprobs":[]}),
                json!({"type":"response.content_part.done","item_id":item.id,"output_index":index,"content_index":0,"part":text_part(text)}),
            ],
            ItemContent::Reasoning(text) => vec![
                json!({"type":"response.reasoning_summary_text.done","item_id":item.id,"output_index":index,"summary_index":0,"text":text}),
                json!({"type":"response.reasoning_summary_part.done","item_id":item.id,"output_index":index,"summary_index":0,"part":{"type":"summary_text","text":text}}),
            ],
            ItemContent::Tool {
                custom,
                name,
                input,
                ..
            } => {
                if *custom {
                    vec![
                        json!({"type":"response.custom_tool_call_input.done","item_id":item.id,"output_index":index,"input":input}),
                    ]
                } else {
                    vec![
                        json!({"type":"response.function_call_arguments.done","item_id":item.id,"output_index":index,"name":name,"arguments":input}),
                    ]
                }
            }
        };
        values.push(json!({"type":"response.output_item.done","output_index":index,"item":item.json(status)}));
        self.items[index].status = status;
        events.extend(values.into_iter().map(|value| self.event(value)));
    }

    fn close_text(&mut self, events: &mut Vec<String>) {
        if let Some(index) = self.active_text.take() {
            self.close(index, "completed", events);
        }
    }

    fn reasoning(&mut self, text: &str) {
        if !self.options.emit_reasoning_summary || text.is_empty() {
            return;
        }
        // Do not retain an opening placeholder and later prefix real summary
        // text with it. Once a real section has started, preserve whitespace
        // and punctuation exactly as Kiro streamed them.
        if self.pending_reasoning.is_empty() && !has_substantive_reasoning_summary(text) {
            return;
        }
        self.pending_reasoning.push_str(text);
    }

    /// Materialize a complete summary section in the initial item event.
    /// Codex versions affected by openai/codex#31216 can persist the final
    /// summary while rendering only the empty item that arrived first. Kiro's
    /// reasoning events are already section-sized, so buffering until the next
    /// visible item preserves their ordering and gives those clients useful
    /// content even if they miss every summary delta.
    fn flush_reasoning(&mut self, events: &mut Vec<String>) {
        let text = std::mem::take(&mut self.pending_reasoning);
        if !has_substantive_reasoning_summary(&text) {
            return;
        }
        self.close_text(events);
        let index = self.add(ItemContent::Reasoning(text.clone()), events);
        events.push(self.event(json!({
            "type":"response.reasoning_summary_part.added",
            "item_id":self.items[index].id,
            "output_index":index,
            "summary_index":0,
            "part":{"type":"summary_text","text":text}
        })));
        self.close(index, "completed", events);
    }

    fn text(&mut self, text: &str, events: &mut Vec<String>) {
        if text.is_empty() {
            return;
        }
        self.flush_reasoning(events);
        let index = if let Some(index) = self.active_text {
            index
        } else {
            self.close_text(events);
            let index = self.add(ItemContent::Message(String::new()), events);
            self.active_text = Some(index);
            events.push(self.event(json!({"type":"response.content_part.added","item_id":self.items[index].id,"output_index":index,"content_index":0,"part":text_part("")})));
            index
        };
        match &mut self.items[index].content {
            ItemContent::Message(content) => content.push_str(text),
            _ => unreachable!("text item"),
        }
        let value = json!({"type":"response.output_text.delta","item_id":self.items[index].id,"output_index":index,"content_index":0,"delta":text,"logprobs":[]});
        events.push(self.event(value));
    }

    fn chunk(&mut self, chat: Value) -> Vec<String> {
        if self.terminal {
            return Vec::new();
        }
        if let Some(error) = chat.get("error") {
            return self.fail(
                error["code"].as_str().unwrap_or("upstream_error"),
                error["message"]
                    .as_str()
                    .unwrap_or("upstream stream failed"),
            );
        }
        if chat.get("usage").is_some_and(Value::is_object) {
            self.usage = usage(&chat["usage"]);
        }
        let mut events = Vec::new();
        let Some(choices) = chat["choices"].as_array() else {
            return self.fail("invalid_stream", "invalid internal OpenAI stream");
        };
        for choice in choices {
            if let Some(reason) = choice["finish_reason"].as_str() {
                self.finish_reason = Some(reason.into());
            }
            let delta = &choice["delta"];
            if let Some(text) = delta["reasoning_content"].as_str() {
                self.reasoning(text);
            }
            if let Some(text) = delta["content"].as_str() {
                self.text(text, &mut events);
            }
            if let Some(calls) = delta["tool_calls"].as_array() {
                self.flush_reasoning(&mut events);
                self.close_text(&mut events);
                for call in calls {
                    let Some(tool_index) = call["index"].as_u64() else {
                        events
                            .extend(self.fail("invalid_stream", "tool delta is missing its index"));
                        return events;
                    };
                    let index = if let Some(index) = self.tool_indices.get(&tool_index) {
                        *index
                    } else {
                        let mut tool = self.options.tool(call);
                        let ItemContent::Tool {
                            call_id,
                            name,
                            input,
                            ..
                        } = &mut tool
                        else {
                            unreachable!()
                        };
                        if call_id.is_empty() || name.is_empty() {
                            events.extend(self.fail(
                                "invalid_stream",
                                "tool delta is missing its call id or name",
                            ));
                            return events;
                        }
                        input.clear();
                        let index = self.add(tool, &mut events);
                        self.tool_indices.insert(tool_index, index);
                        index
                    };
                    let ItemContent::Tool {
                        custom,
                        input,
                        call_id,
                        ..
                    } = &mut self.items[index].content
                    else {
                        unreachable!()
                    };
                    let body = &call[if *custom { "custom" } else { "function" }];
                    let added = body[if *custom { "input" } else { "arguments" }]
                        .as_str()
                        .unwrap_or_default();
                    input.push_str(added);
                    let custom = *custom;
                    let call_id = call_id.clone();
                    if !added.is_empty() {
                        events.push(self.event(json!({
                            "type":if custom { "response.custom_tool_call_input.delta" } else { "response.function_call_arguments.delta" },
                            "item_id":self.items[index].id,"output_index":index,"call_id":call_id,"delta":added
                        })));
                    }
                }
            }
        }
        events
    }

    fn finish(&mut self) -> Vec<String> {
        if self.terminal {
            return Vec::new();
        }
        if self.finish_reason.is_none() || self.usage.is_null() {
            return self.fail(
                "incomplete_stream",
                "upstream stream ended without a finish reason and usage",
            );
        }
        let status = if self.finish_reason.as_deref() == Some("length") {
            "incomplete"
        } else {
            "completed"
        };
        let mut events = Vec::new();
        self.flush_reasoning(&mut events);
        if self.items.is_empty() {
            let index = self.add(ItemContent::Message(String::new()), &mut events);
            events.push(self.event(json!({"type":"response.content_part.added","item_id":self.items[index].id,"output_index":index,"content_index":0,"part":text_part("")})));
        }
        let output = self
            .items
            .iter()
            .map(|item| {
                item.json(if item.status == "in_progress" {
                    status
                } else {
                    item.status
                })
            })
            .collect();
        let response = match self.options.finalize(output, status, self.usage.clone()) {
            Ok(response) => response,
            Err(message) => {
                events.extend(self.fail("response_store_failed", message));
                return events;
            }
        };
        for index in 0..self.items.len() {
            self.close(index, status, &mut events);
        }
        events.push(self.event(json!({"type":if status == "incomplete" { "response.incomplete" } else { "response.completed" },"response":response})));
        self.terminal = true;
        events
    }

    fn fail(&mut self, code: &str, message: &str) -> Vec<String> {
        if self.terminal {
            return Vec::new();
        }
        self.pending_reasoning.clear();
        self.terminal = true;
        let output = self
            .items
            .iter()
            .map(|item| {
                item.json(if item.status == "in_progress" {
                    "incomplete"
                } else {
                    item.status
                })
            })
            .collect();
        let response = self.options.envelope(
            output,
            "failed",
            self.usage.clone(),
            json!({"code":code,"message":crosslane_translate::sanitize_error_message(message)}),
        );
        vec![self.event(json!({"type":"response.failed","response":response}))]
    }
}

pub(super) fn stream_response(response: Response, options: ResponsesOptions) -> Response {
    let (mut parts, body) = response.into_parts();
    parts.headers.remove(header::CONTENT_LENGTH);
    let stream = async_stream::stream! {
        let mut state = ResponsesStream::new(options);
        for event in state.start() { yield Ok::<Bytes, Infallible>(Bytes::from(event)); }
        let mut source = body.into_data_stream();
        let mut buffer = BytesMut::new();
        while let Some(chunk) = source.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(_) => {
                    for event in state.fail("incomplete_stream", "upstream stream was interrupted") { yield Ok(Bytes::from(event)); }
                    break;
                }
            };
            buffer.extend_from_slice(&chunk);
            while let Some((end, delimiter)) = frame_end(&buffer) {
                let frame = buffer.split_to(end + delimiter);
                let events = match std::str::from_utf8(&frame[..end]) {
                    Ok(frame) => {
                        let data = frame.lines().filter_map(|line| line.strip_prefix("data:").map(|data| data.strip_prefix(' ').unwrap_or(data))).collect::<Vec<_>>().join("\n");
                        if data.is_empty() {
                            if state.terminal { Vec::new() } else { vec![": keepalive\n\n".into()] }
                        } else if data == "[DONE]" {
                            state.finish()
                        } else {
                            match serde_json::from_str(&data) {
                                Ok(value) => state.chunk(value),
                                Err(_) => state.fail("invalid_stream", "invalid internal OpenAI event JSON"),
                            }
                        }
                    }
                    Err(_) => state.fail("invalid_stream", "invalid internal OpenAI event encoding"),
                };
                for event in events { yield Ok(Bytes::from(event)); }
            }
            // The shared decoder already bounds generated output. Also bound
            // an unterminated adapter frame so a broken stream cannot grow it.
            if buffer.len() > 16 * 1024 * 1024 {
                for event in state.fail("invalid_stream", "internal OpenAI event exceeds the frame limit") { yield Ok(Bytes::from(event)); }
                break;
            }
        }
        if !state.terminal {
            for event in state.fail("incomplete_stream", "upstream stream ended before completion") { yield Ok(Bytes::from(event)); }
        }
    };
    Response::from_parts(parts, Body::from_stream(stream))
}

pub(super) fn frame_end(buffer: &[u8]) -> Option<(usize, usize)> {
    let lf = buffer
        .windows(2)
        .position(|bytes| bytes == b"\n\n")
        .map(|index| (index, 2));
    let crlf = buffer
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .map(|index| (index, 4));
    lf.into_iter().chain(crlf).min_by_key(|(index, _)| *index)
}

#[cfg(test)]
mod tests;
