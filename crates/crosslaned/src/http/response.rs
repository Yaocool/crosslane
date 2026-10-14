use std::collections::{BTreeMap, HashSet};

use crosslane_kiro::{KiroCitation, KiroEvent, UsageInfo};
use crosslane_translate::{
    ClaudeContextEditStats, ClaudeServerToolEmission, ClaudeToolSearchTrace, ClaudeWebSearchTrace,
    KiroToolUse, WebSearchReplayCodec, WebSearchReplayError,
};
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiToolIdentity {
    pub kind: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionIterationUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Default)]
pub struct DecodedResponse {
    pub text: String,
    pub reasoning: String,
    pub reasoning_signature: Option<String>,
    pub redacted_thinking: String,
    pub citations: Vec<KiroCitation>,
    pub tools: BTreeMap<String, ToolBuffer>,
    pub tool_searches: Vec<ClaudeToolSearchTrace>,
    pub web_searches: Vec<ClaudeWebSearchTrace>,
    /// Non-streaming order for proxy-executed server tools. Text produced
    /// before each server call must stay before that call; the final answer is
    /// rendered after the complete timeline.
    pub claude_server_events: Vec<ClaudeServerEvent>,
    /// Latest local checkpoint produced while resolving server tools.
    pub automatic_compaction: Option<crosslane_translate::ContextCompactionStats>,
    pub usage: UsageInfo,
    pub upstream_input_tokens: Option<u64>,
    pub upstream_context_usage_percentage: Option<f64>,
    /// Explicit Claude stop reason for proxy-managed server loops (for example
    /// `pause_turn`). When absent the ordinary tool/max/end inference applies.
    pub stop_reason: Option<String>,
    /// Custom Claude stop sequence matched by the proxy response filter.
    pub stop_sequence: Option<String>,
}

impl DecodedResponse {
    pub fn log_context_usage(&self, trace_id: &str, estimated_input_tokens: u64) {
        if self.upstream_input_tokens.is_some() || self.upstream_context_usage_percentage.is_some()
        {
            tracing::info!(
                event = "proxy.upstream.context_usage",
                trace_id,
                estimated_input_tokens,
                upstream_input_tokens = self.upstream_input_tokens,
                upstream_context_usage_percentage = self.upstream_context_usage_percentage,
                "upstream context accounting observed independently of local token estimates"
            );
        }
    }
    /// Removes buffered tool calls matching `predicate` in their stable map order.
    pub fn take_tool_uses_where(
        &mut self,
        mut predicate: impl FnMut(&ToolBuffer) -> bool,
    ) -> Vec<KiroToolUse> {
        let tools = std::mem::take(&mut self.tools);
        let mut selected = Vec::new();
        for (key, tool) in tools {
            if predicate(&tool) {
                selected.push(KiroToolUse {
                    tool_use_id: tool.id,
                    name: tool.name,
                    input: repair_json(&tool.input),
                });
            } else {
                self.tools.insert(key, tool);
            }
        }
        selected
    }
}

#[derive(Debug, Default)]
pub struct StopSequenceFilter {
    sequences: Vec<CompiledStopSequence>,
    pending: String,
    matched: Option<String>,
    visible_bytes: usize,
}

const THINKING_START_TAG: &str = "<thinking>";
const THINKING_END_TAG: &str = "</thinking>";

/// Normalizes Kiro's two thinking representations before protocol encoding.
///
/// Kiro usually emits `reasoningContentEvent`, but it can fall back to literal
/// `<thinking>...</thinking>` text inside `assistantResponseEvent`. Tags may be
/// split across arbitrary upstream chunks, so a small suffix must be retained
/// until the next event resolves it. When thinking was disabled for the actual
/// request, both representations are consumed without exposing their content.
#[derive(Debug)]
pub struct ThinkingContentFilter {
    enabled: bool,
    omit_summary: bool,
    pending: String,
    in_tagged_thinking: bool,
    position: MarkdownPosition,
}

impl ThinkingContentFilter {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            omit_summary: false,
            pending: String::new(),
            in_tagged_thinking: false,
            position: MarkdownPosition::default(),
        }
    }

    pub fn with_omitted_summary(mut self, omit_summary: bool) -> Self {
        self.omit_summary = omit_summary;
        self
    }

    pub fn push(&mut self, event: KiroEvent) -> Vec<KiroEvent> {
        match event {
            KiroEvent::AssistantResponse { content } => {
                self.pending.push_str(&content);
                self.drain(false)
            }
            KiroEvent::Reasoning {
                mut content,
                signature,
                redacted_content,
            } => {
                let mut output = self.drain(false);
                if self.omit_summary {
                    // Claude display:omitted hides text, not the native
                    // signature needed to preserve multi-turn continuity.
                    content.clear();
                }
                if self.enabled
                    && (!content.is_empty() || signature.is_some() || redacted_content.is_some())
                {
                    output.push(KiroEvent::Reasoning {
                        content,
                        signature,
                        redacted_content,
                    });
                }
                output
            }
            event => {
                // Metadata and native content events can be interleaved with
                // text fragments, including fragments of either thinking tag.
                // Only actual text delimiters or EOF end tagged reasoning.
                let mut output = self.drain(false);
                output.push(event);
                output
            }
        }
    }

    pub fn finish(&mut self) -> Vec<KiroEvent> {
        self.drain(true)
    }

    fn drain(&mut self, finish: bool) -> Vec<KiroEvent> {
        let mut output = Vec::new();
        loop {
            if self.in_tagged_thinking {
                if let Some(end) = self.pending.find(THINKING_END_TAG) {
                    let content = take_prefix(&mut self.pending, end + THINKING_END_TAG.len());
                    if self.enabled && !self.omit_summary {
                        push_text(&mut output, content);
                    }
                    self.in_tagged_thinking = false;
                    self.position = MarkdownPosition::default();
                    self.position.advance(THINKING_END_TAG);
                    continue;
                }
                // Retain only a possible closing-tag suffix, never the entire
                // reasoning body. Visible reasoning remains ordinary tagged
                // text; streaming must not invent native thinking signatures.
                let split = if finish {
                    self.pending.len()
                } else {
                    safe_prefix_len(&self.pending, THINKING_END_TAG)
                };
                let content = take_prefix(&mut self.pending, split);
                if self.enabled && !self.omit_summary {
                    push_text(&mut output, content);
                }
                if finish {
                    self.in_tagged_thinking = false;
                }
                break;
            }

            if let Some(start) = self.pending.find(THINKING_START_TAG) {
                let content = take_prefix(&mut self.pending, start);
                self.position.advance(&content);
                push_text(&mut output, content);
                let tag = take_prefix(&mut self.pending, THINKING_START_TAG.len());
                self.position.advance(&tag);
                if self.position.in_code() {
                    push_text(&mut output, tag);
                } else {
                    self.in_tagged_thinking = true;
                    if self.enabled && !self.omit_summary {
                        push_text(&mut output, tag);
                    }
                }
                continue;
            }
            let split = if finish {
                self.pending.len()
            } else {
                safe_prefix_len(&self.pending, THINKING_START_TAG)
            };
            if split > 0 {
                let content = take_prefix(&mut self.pending, split);
                self.position.advance(&content);
                push_text(&mut output, content);
            }
            break;
        }
        output
    }
}

fn safe_prefix_len(value: &str, delimiter: &str) -> usize {
    let max_retained = value.len().min(delimiter.len().saturating_sub(1));
    for retained in (1..=max_retained).rev() {
        let split = value.len() - retained;
        if value.is_char_boundary(split)
            && delimiter.as_bytes().starts_with(&value.as_bytes()[split..])
        {
            return split;
        }
    }
    value.len()
}

fn take_prefix(value: &mut String, end: usize) -> String {
    value.drain(..end).collect()
}

fn push_text(output: &mut Vec<KiroEvent>, content: String) {
    if !content.is_empty() {
        output.push(KiroEvent::AssistantResponse { content });
    }
}

#[derive(Debug)]
struct CompiledStopSequence {
    value: String,
    failure: Vec<usize>,
    state: usize,
}

impl CompiledStopSequence {
    fn new(value: String) -> Self {
        let bytes = value.as_bytes();
        let mut failure = vec![0; bytes.len()];
        let mut prefix = 0;
        for index in 1..bytes.len() {
            while prefix > 0 && bytes[index] != bytes[prefix] {
                prefix = failure[prefix - 1];
            }
            if bytes[index] == bytes[prefix] {
                prefix += 1;
            }
            failure[index] = prefix;
        }
        Self {
            value,
            failure,
            state: 0,
        }
    }

    fn advance(&mut self, byte: u8) -> bool {
        let bytes = self.value.as_bytes();
        while self.state > 0 && byte != bytes[self.state] {
            self.state = self.failure[self.state - 1];
        }
        if byte == bytes[self.state] {
            self.state += 1;
        }
        self.state == bytes.len()
    }

    fn reset(&mut self) {
        self.state = 0;
    }
}

impl StopSequenceFilter {
    pub fn new(sequences: &[String]) -> Self {
        let mut compiled = Vec::with_capacity(sequences.len());
        for sequence in sequences.iter().filter(|sequence| !sequence.is_empty()) {
            if compiled
                .iter()
                .any(|candidate: &CompiledStopSequence| candidate.value == *sequence)
            {
                continue;
            }
            compiled.push(CompiledStopSequence::new(sequence.clone()));
        }
        Self {
            sequences: compiled,
            pending: String::new(),
            matched: None,
            visible_bytes: 0,
        }
    }

    /// Returns the portion that is safe to publish. A suffix that could begin
    /// a stop sequence is retained until the next chunk resolves it.
    pub fn push(&mut self, text: &str) -> String {
        if self.matched.is_some() {
            return String::new();
        }
        if self.sequences.is_empty() {
            self.visible_bytes = self.visible_bytes.saturating_add(text.len());
            return text.into();
        }
        let pending_len = self.pending.len();
        self.pending.push_str(text);
        for (offset, byte) in text.bytes().enumerate() {
            let matched = self
                .sequences
                .iter_mut()
                .enumerate()
                .find_map(|(index, sequence)| sequence.advance(byte).then_some(index));
            if let Some(index) = matched {
                let sequence_len = self.sequences[index].value.len();
                let match_end = pending_len + offset + 1;
                let match_start = match_end.saturating_sub(sequence_len);
                let visible = self.pending[..match_start].to_owned();
                self.pending.clear();
                self.matched = Some(self.sequences[index].value.clone());
                self.visible_bytes = self.visible_bytes.saturating_add(visible.len());
                return visible;
            }
        }
        let retained = self
            .sequences
            .iter()
            .map(|sequence| sequence.state)
            .max()
            .unwrap_or_default();
        let split = self.pending.len().saturating_sub(retained);
        let visible = self.pending[..split].to_owned();
        self.pending = self.pending[split..].to_owned();
        self.visible_bytes = self.visible_bytes.saturating_add(visible.len());
        visible
    }

    pub fn finish(&mut self) -> String {
        for sequence in &mut self.sequences {
            sequence.reset();
        }
        if self.matched.is_some() {
            self.pending.clear();
            String::new()
        } else {
            let visible = std::mem::take(&mut self.pending);
            self.visible_bytes = self.visible_bytes.saturating_add(visible.len());
            visible
        }
    }

    pub fn matched(&self) -> Option<&str> {
        self.matched.as_deref()
    }

    pub fn visible_bytes(&self) -> usize {
        self.visible_bytes
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeServerEvent {
    ToolSearch {
        index: usize,
        preceding_text: String,
    },
    WebSearch {
        index: usize,
        preceding_text: String,
    },
}

#[derive(Debug, Default)]
pub struct ToolBuffer {
    pub id: String,
    pub name: String,
    pub input: String,
    pub complete: bool,
}

const MAX_CUMULATIVE_TOOL_INPUT_BYTES: usize = 16 * 1024 * 1024;

/// Holds split XML-like tool calls until they can be recovered or proven to be text.
#[derive(Debug)]
pub struct ToolLeakFilter {
    enabled: bool,
    known_tools: HashSet<String>,
    tool_schemas: BTreeMap<String, Value>,
    position: MarkdownPosition,
    in_tagged_thinking: bool,
    pending: String,
    recovered: Vec<KiroEvent>,
    structured: BTreeMap<String, (String, String)>,
}

impl ToolLeakFilter {
    pub fn new(enabled: bool, known_tools: impl IntoIterator<Item = String>) -> Self {
        Self {
            enabled,
            known_tools: known_tools.into_iter().collect(),
            tool_schemas: BTreeMap::new(),
            position: MarkdownPosition::default(),
            in_tagged_thinking: false,
            pending: String::new(),
            recovered: Vec::new(),
            structured: BTreeMap::new(),
        }
    }

    pub fn for_payload(enabled: bool, payload: &crosslane_translate::KiroPayload) -> Self {
        let mut filter = Self::new(enabled, std::iter::empty());
        for tool in payload
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .iter()
            .flat_map(|context| &context.tools)
            .filter_map(crosslane_translate::KiroTool::specification)
        {
            filter.known_tools.insert(tool.name.clone());
            filter
                .tool_schemas
                .insert(tool.name.clone(), tool.input_schema.json.clone());
        }
        filter
    }

    pub fn push(&mut self, event: KiroEvent) -> Vec<KiroEvent> {
        if !self.enabled {
            return vec![event];
        }
        match event {
            KiroEvent::AssistantResponse { content } => self.push_text(content),
            KiroEvent::ToolUse {
                id,
                name,
                input_delta,
                stop,
            } => {
                let buffer = self
                    .structured
                    .entry(id.clone())
                    .or_insert_with(|| (name.clone(), String::new()));
                if buffer.0.is_empty() {
                    buffer.0 = name.clone();
                }
                buffer.1.push_str(&input_delta);
                vec![KiroEvent::ToolUse {
                    id,
                    name,
                    input_delta,
                    stop,
                }]
            }
            other => vec![other],
        }
    }

    pub fn finish(mut self) -> Vec<KiroEvent> {
        let mut output = Vec::new();
        if !self.pending.is_empty() {
            output.push(KiroEvent::AssistantResponse {
                content: std::mem::take(&mut self.pending),
            });
        }
        output.extend(self.recovered.into_iter().filter(|event| match event {
            KiroEvent::ToolUse {
                name, input_delta, ..
            } => {
                let input = repair_json(input_delta);
                !self.structured.values().any(|(seen_name, seen_input)| {
                    seen_name == name && repair_json(seen_input) == input
                })
            }
            _ => true,
        }));
        output
    }

    fn push_text(&mut self, content: String) -> Vec<KiroEvent> {
        self.pending.push_str(&content);
        if self.pending.len() > 1024 * 1024 {
            // A bounded fallback must not forget that the following chunks
            // are still inside a code fence or in the middle of a line.
            // Do not resume speculative recovery after bypassing the tagged
            // thinking parser. Native tool events still pass through/deduplicate.
            self.known_tools.clear();
            self.tool_schemas.clear();
            self.position.advance(&self.pending);
            return vec![KiroEvent::AssistantResponse {
                content: std::mem::take(&mut self.pending),
            }];
        }
        let mut visible = String::new();
        loop {
            let Some((open, marker)) = first_marker(&self.pending) else {
                let keep = marker_prefix_suffix(&self.pending);
                let split = self.pending.len().saturating_sub(keep);
                visible.push_str(&self.pending[..split]);
                self.position.advance(&self.pending[..split]);
                self.pending = self.pending[split..].into();
                break;
            };
            visible.push_str(&self.pending[..open]);
            self.position.advance(&self.pending[..open]);
            self.position.finish_tick_run();
            if matches!(marker, THINKING_START_TAG | THINKING_END_TAG) {
                let end = open + marker.len();
                visible.push_str(&self.pending[open..end]);
                if marker == THINKING_END_TAG && self.in_tagged_thinking {
                    // Markdown inside reasoning must not carry code/inline
                    // state into the assistant's subsequent answer.
                    self.position = MarkdownPosition::default();
                }
                self.position.advance(&self.pending[open..end]);
                if marker == THINKING_END_TAG {
                    self.in_tagged_thinking = false;
                } else if !self.position.in_code() {
                    // Literal tags in Markdown examples do not open reasoning.
                    // Always accept the closing tag once reasoning has begun:
                    // its content can itself contain unfinished Markdown.
                    self.in_tagged_thinking = true;
                }
                self.pending = self.pending[end..].into();
                continue;
            }
            if self.in_tagged_thinking || self.position.in_code() {
                // Display-only XML is not a candidate: waiting for its close
                // could swallow the end of a code/thinking block and a later
                // real call when the example intentionally omits that close.
                let end = open + marker.len();
                visible.push_str(&self.pending[open..end]);
                self.position.advance(&self.pending[open..end]);
                self.pending = self.pending[end..].into();
                continue;
            }
            let close = if marker == "<function_calls" {
                "</function_calls>"
            } else {
                "</tool_use>"
            };
            let lower = self.pending.to_ascii_lowercase();
            let Some(relative) = lower[open..].find(close) else {
                self.pending = self.pending[open..].into();
                break;
            };
            let end = open + relative + close.len();
            let xml = self.pending[open..end].to_string();
            let recovered = (self.position.can_recover() && !self.in_tagged_thinking)
                .then(|| self.recover(&xml))
                .flatten();
            if let Some(events) = recovered {
                self.recovered.extend(events);
                // Arguments are data, not Markdown in the assistant answer.
                // The opaque call still occupies the current line.
                self.position.advance(marker);
            } else {
                visible.push_str(&xml);
                self.position.advance(&xml);
            }
            self.pending = self.pending[end..].into();
        }
        if visible.is_empty() {
            Vec::new()
        } else {
            vec![KiroEvent::AssistantResponse { content: visible }]
        }
    }

    // Recovery is atomic: malformed or unknown invocations preserve the whole
    // block as text, including any valid invocation that precedes them.
    fn recover(&self, xml: &str) -> Option<Vec<KiroEvent>> {
        let mut events = Vec::new();
        let outer_end = xml.find('>')?;
        let attributes = xml_attributes(&xml[..=outer_end])?;
        let function_calls = xml.to_ascii_lowercase().starts_with("<function_calls");
        let closing = if function_calls {
            "</function_calls>"
        } else {
            "</tool_use>"
        };
        let inner = xml.get(outer_end + 1..xml.len().checked_sub(closing.len())?)?;
        if !function_calls {
            let mut names = ["name", "tool", "function"]
                .into_iter()
                .filter_map(|key| attributes.get(key));
            let name = names.next()?.clone();
            if names.next().is_some() {
                return None;
            }
            if !self.known_tools.contains(&name) {
                return None;
            }
            let input = serde_json::from_str::<Value>(&decode_xml(inner.trim()))
                .ok()
                .filter(Value::is_object)?;
            return Some(vec![KiroEvent::ToolUse {
                id: format!("recovered_tool_{}", uuid::Uuid::new_v4().simple()),
                name,
                input_delta: input.to_string(),
                stop: true,
            }]);
        }
        let mut cursor = 0;
        while let Some(open) = find_insensitive_from(inner, "<invoke", cursor) {
            if !inner[cursor..open].trim().is_empty()
                || !inner
                    .as_bytes()
                    .get(open + "<invoke".len())
                    .is_some_and(u8::is_ascii_whitespace)
            {
                return None;
            }
            let tag_end = inner[open..].find('>').map(|index| open + index)?;
            let close = find_insensitive_from(inner, "</invoke>", tag_end)?;
            let name = xml_attributes(&inner[open..=tag_end])?.remove("name")?;
            if self.known_tools.contains(&name) {
                let input =
                    parse_parameters(&inner[tag_end + 1..close], self.tool_schemas.get(&name))?;
                events.push(KiroEvent::ToolUse {
                    id: format!("recovered_tool_{}", uuid::Uuid::new_v4().simple()),
                    name,
                    input_delta: input.to_string(),
                    stop: true,
                });
            } else {
                return None;
            }
            cursor = close + "</invoke>".len();
        }
        (!events.is_empty() && inner[cursor..].trim().is_empty()).then_some(events)
    }
}

/// Tracks only the start of each Markdown line, with bounded memory even for
/// very long code blocks. Fence runs and newlines may arrive in different SSE chunks.
#[derive(Debug)]
struct MarkdownPosition {
    fence: Option<(u8, usize)>,
    inline_ticks: Option<usize>,
    pending_ticks: usize,
    indent: usize,
    whitespace: bool,
    run: Option<(u8, usize)>,
    run_ended: bool,
    trailer_whitespace: bool,
}

impl Default for MarkdownPosition {
    fn default() -> Self {
        Self {
            fence: None,
            inline_ticks: None,
            pending_ticks: 0,
            indent: 0,
            whitespace: true,
            run: None,
            run_ended: false,
            trailer_whitespace: true,
        }
    }
}

impl MarkdownPosition {
    fn in_code(&self) -> bool {
        self.fence.is_some()
            || self.inline_ticks.is_some()
            || self.indent > 3
            || self.run.is_some_and(|(_, length)| length >= 3)
    }

    fn can_recover(&self) -> bool {
        !self.in_code() && self.whitespace
    }

    fn finish_tick_run(&mut self) {
        let length = std::mem::take(&mut self.pending_ticks);
        if length == 0 {
            return;
        }
        let fence_start = self.run == Some((b'`', length)) && length >= 3 && !self.run_ended;
        if !fence_start {
            match self.inline_ticks {
                Some(open) if open == length => self.inline_ticks = None,
                None => self.inline_ticks = Some(length),
                _ => {}
            }
        }
    }

    fn advance(&mut self, text: &str) {
        for byte in text.bytes() {
            if byte == b'`' && self.fence.is_none() {
                self.pending_ticks = self.pending_ticks.saturating_add(1);
            } else {
                self.finish_tick_run();
            }
            if byte == b'\n' {
                if let Some((marker, length)) = self.run.filter(|(_, length)| *length >= 3) {
                    match self.fence {
                        None => self.fence = Some((marker, length)),
                        Some((open, count))
                            if open == marker && length >= count && self.trailer_whitespace =>
                        {
                            self.fence = None
                        }
                        _ => {}
                    }
                }
                *self = Self {
                    fence: self.fence,
                    inline_ticks: self.inline_ticks,
                    ..Self::default()
                };
            } else if self.whitespace && matches!(byte, b' ' | b'\t' | b'\r') {
                self.indent = self
                    .indent
                    .saturating_add(if byte == b'\t' { 4 } else { 1 });
            } else if self.whitespace {
                self.whitespace = false;
                if self.indent <= 3 && self.inline_ticks.is_none() && matches!(byte, b'`' | b'~') {
                    self.run = Some((byte, 1));
                }
            } else if let Some((marker, length)) = &mut self.run {
                if !self.run_ended && byte == *marker {
                    *length = length.saturating_add(1);
                } else {
                    self.run_ended = true;
                    self.trailer_whitespace &= byte.is_ascii_whitespace();
                }
            }
        }
    }
}

fn first_marker(text: &str) -> Option<(usize, &'static str)> {
    let lower = text.to_ascii_lowercase();
    [
        "<function_calls",
        "<tool_use",
        THINKING_START_TAG,
        THINKING_END_TAG,
    ]
    .into_iter()
    .filter_map(|marker| {
        lower.match_indices(marker).find_map(|(index, _)| {
            (marker.ends_with('>')
                || lower
                    .as_bytes()
                    .get(index + marker.len())
                    .is_none_or(|byte| byte.is_ascii_whitespace() || *byte == b'>'))
            .then_some((index, marker))
        })
    })
    .min_by_key(|(index, _)| *index)
}

fn marker_prefix_suffix(text: &str) -> usize {
    let lower = text.to_ascii_lowercase();
    let mut longest = 0;
    for marker in [
        "<function_calls",
        "<tool_use",
        THINKING_START_TAG,
        THINKING_END_TAG,
    ] {
        for length in 1..marker.len() {
            if lower.ends_with(&marker[..length]) {
                longest = longest.max(length);
            }
        }
    }
    longest.min(text.len())
}

fn find_insensitive_from(text: &str, needle: &str, from: usize) -> Option<usize> {
    text.get(from..)?
        .to_ascii_lowercase()
        .find(&needle.to_ascii_lowercase())
        .map(|index| from + index)
}

fn xml_attributes(tag: &str) -> Option<BTreeMap<String, String>> {
    let inner = tag.strip_prefix('<')?.strip_suffix('>')?;
    let element_end = inner.find(char::is_whitespace).unwrap_or(inner.len());
    let mut rest = &inner[element_end..];
    let mut attributes = BTreeMap::new();
    while !rest.trim().is_empty() {
        // Consume attributes as quoted tokens, never by searching inside
        // another attribute's value. Duplicate/malformed attributes fail closed.
        if !rest.starts_with(char::is_whitespace) {
            return None;
        }
        rest = rest.trim_start();
        let end = rest.find(|character: char| {
            !(character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | ':' | '.'))
        })?;
        if end == 0 {
            return None;
        }
        let name = rest[..end].to_ascii_lowercase();
        rest = rest[end..].trim_start().strip_prefix('=')?.trim_start();
        let quote = rest
            .chars()
            .next()
            .filter(|character| matches!(character, '\'' | '"'))?;
        rest = &rest[1..];
        let end = rest.find(quote)?;
        if attributes.insert(name, decode_xml(&rest[..end])).is_some() {
            return None;
        }
        rest = &rest[end + 1..];
    }
    Some(attributes)
}

fn parse_parameters(xml: &str, schema: Option<&Value>) -> Option<Value> {
    let mut map = serde_json::Map::new();
    let mut cursor = 0;
    while let Some(open) = find_insensitive_from(xml, "<parameter", cursor) {
        if !xml[cursor..open].trim().is_empty() {
            return None;
        }
        if !xml
            .as_bytes()
            .get(open + "<parameter".len())
            .is_some_and(u8::is_ascii_whitespace)
        {
            return None;
        }
        let tag_end = xml[open..].find('>').map(|index| open + index)?;
        let close = find_insensitive_from(xml, "</parameter>", tag_end)?;
        let name = xml_attributes(&xml[open..=tag_end])?.remove("name")?;
        if !map.contains_key(&name) {
            let value = decode_xml(&xml[tag_end + 1..close]);
            let string_parameter = match schema {
                Some(schema) => parameter_preserves_string(schema, Some(&name), schema, &mut 128)?,
                None => false,
            };
            map.insert(
                name,
                if string_parameter {
                    Value::String(value)
                } else {
                    coerce(&value)
                },
            );
        } else {
            return None;
        }
        cursor = close + "</parameter>".len();
    }
    xml[cursor..]
        .trim()
        .is_empty()
        .then_some(Value::Object(map))
}

/// Reads type hints, not a complete JSON Schema validator. References stay
/// local and traversal is bounded; unresolved/cyclic schemas fail recovery
/// closed instead of guessing a potentially destructive tool argument type.
fn parameter_preserves_string(
    schema: &Value,
    property: Option<&str>,
    root: &Value,
    remaining: &mut usize,
) -> Option<bool> {
    let hint = parameter_types(schema, property, root, remaining)?;
    (hint.string || hint.other).then_some(hint.specified && hint.string)
}

#[derive(Clone, Copy)]
struct XmlParameterTypes {
    string: bool,
    other: bool,
    specified: bool,
}

impl XmlParameterTypes {
    const ANY: Self = Self {
        string: true,
        other: true,
        specified: false,
    };
    const NONE: Self = Self {
        string: false,
        other: false,
        specified: true,
    };

    fn value(value: &Value) -> Self {
        Self {
            string: value.is_string(),
            other: !value.is_string(),
            specified: true,
        }
    }

    fn intersect(&mut self, hint: Self) {
        self.string &= hint.string;
        self.other &= hint.other;
        self.specified |= hint.specified;
    }

    fn union(&mut self, hint: Self) {
        self.string |= hint.string;
        self.other |= hint.other;
        self.specified |= hint.specified;
    }
}

fn parameter_types(
    schema: &Value,
    property: Option<&str>,
    root: &Value,
    remaining: &mut usize,
) -> Option<XmlParameterTypes> {
    *remaining = remaining.checked_sub(1)?;
    if let Some(allowed) = schema.as_bool() {
        return Some(if allowed {
            XmlParameterTypes::ANY
        } else {
            XmlParameterTypes::NONE
        });
    }
    schema.as_object()?;
    let mut hint = if let Some(name) = property {
        match schema
            .get("properties")
            .and_then(|properties| properties.get(name))
            .or_else(|| schema.get("additionalProperties"))
        {
            Some(schema) => parameter_types(schema, None, root, remaining)?,
            None => XmlParameterTypes::ANY,
        }
    } else {
        let mut hint = XmlParameterTypes::ANY;
        if let Some(kind) = schema.get("type") {
            let kinds = kind
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_else(|| std::slice::from_ref(kind));
            let mut declared = XmlParameterTypes::NONE;
            for kind in kinds {
                match kind.as_str()? {
                    "string" => declared.string = true,
                    "null" | "boolean" | "number" | "integer" | "array" | "object" => {
                        declared.other = true
                    }
                    _ => return None,
                }
            }
            hint.intersect(declared);
        }
        if let Some(values) = schema.get("enum") {
            let mut enumerated = XmlParameterTypes::NONE;
            for value in values.as_array()? {
                enumerated.union(XmlParameterTypes::value(value));
            }
            hint.intersect(enumerated);
        }
        if let Some(value) = schema.get("const") {
            hint.intersect(XmlParameterTypes::value(value));
        }
        hint
    };
    if let Some(reference) = schema.get("$ref") {
        let pointer = reference.as_str()?.strip_prefix('#')?;
        hint.intersect(parameter_types(
            root.pointer(pointer)?,
            property,
            root,
            remaining,
        )?);
    }
    for keyword in ["allOf", "anyOf", "oneOf"] {
        if let Some(branches) = schema.get(keyword) {
            let mut combined = if keyword == "allOf" {
                XmlParameterTypes::ANY
            } else {
                XmlParameterTypes {
                    specified: false,
                    ..XmlParameterTypes::NONE
                }
            };
            for branch in branches.as_array()? {
                let branch = parameter_types(branch, property, root, remaining)?;
                if keyword == "allOf" {
                    combined.intersect(branch);
                } else {
                    combined.union(branch);
                }
            }
            hint.intersect(combined);
        }
    }
    Some(hint)
}

fn coerce(value: &str) -> Value {
    serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.into()))
}

fn decode_xml(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

impl DecodedResponse {
    pub fn stop_at_sequence(&mut self, visible_text: String, sequence: String) {
        self.text = visible_text;
        self.tools.clear();
        self.stop_reason = Some("stop_sequence".into());
        self.stop_sequence = Some(sequence);
    }

    pub fn push(&mut self, event: KiroEvent) -> Result<(), String> {
        match event {
            KiroEvent::AssistantResponse { content } => {
                // Shared filters already classified this text. A second
                // string replacement here corrupts literal XML/code examples
                // and makes buffered output disagree with the SSE deltas.
                self.text.push_str(&content)
            }
            KiroEvent::Reasoning {
                content,
                signature,
                redacted_content,
            } => {
                self.reasoning.push_str(&content);
                if let Some(signature) = signature {
                    self.reasoning_signature
                        .get_or_insert_with(String::new)
                        .push_str(&signature);
                }
                if let Some(redacted) = redacted_content {
                    self.redacted_thinking.push_str(&redacted);
                }
            }
            KiroEvent::Citations { citations } => self.citations.extend(citations),
            KiroEvent::ToolUse {
                id,
                name,
                input_delta,
                stop,
            } => {
                let current_bytes = self
                    .tools
                    .values()
                    .map(|tool| tool.input.len())
                    .sum::<usize>();
                if current_bytes.saturating_add(input_delta.len()) > MAX_CUMULATIVE_TOOL_INPUT_BYTES
                {
                    return Err("tool input exceeded the 16 MiB response limit".into());
                }
                let tool = self.tools.entry(id.clone()).or_insert_with(|| ToolBuffer {
                    id,
                    name: name.clone(),
                    input: String::new(),
                    complete: false,
                });
                if tool.name.is_empty() {
                    tool.name = name;
                }
                tool.input.push_str(&input_delta);
                tool.complete |= stop;
            }
            KiroEvent::MessageMetadata { usage } | KiroEvent::Usage { usage } => {
                merge_usage(&mut self.usage, &usage);
                if self.usage.input_tokens > 0 {
                    self.upstream_input_tokens = Some(self.usage.input_tokens);
                }
            }
            KiroEvent::Error { message, .. } => return Err(message),
            KiroEvent::Other {
                event_type,
                payload,
            } => {
                if event_type == "contextUsageEvent" {
                    let body = payload.get("contextUsageEvent").unwrap_or(&payload);
                    self.upstream_context_usage_percentage = body
                        .get("contextUsagePercentage")
                        .or_else(|| body.get("percentage"))
                        .and_then(Value::as_f64)
                        .filter(|value| value.is_finite() && *value >= 0.0);
                }
            }
        }
        Ok(())
    }

    /// Finalize every tool by its JSON input, never by its name or presumed
    /// side effects. A missing stop event is harmless when the input is valid.
    /// Do not repair non-empty inputs here: unbuffered streams may already have
    /// sent those bytes, and guessing missing values can change a tool call.
    pub fn finalize_tool_inputs(&mut self) -> Result<(), String> {
        for tool in self.tools.values() {
            if !tool.input.trim().is_empty() && serde_json::from_str::<Value>(&tool.input).is_err()
            {
                return Err(format!("tool {} produced invalid JSON input", tool.name));
            }
        }
        for tool in self.tools.values_mut() {
            if tool.input.trim().is_empty() {
                tool.input = "{}".into();
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn claude_json(
        &self,
        id: &str,
        model: &str,
        max_tokens: u32,
        current_round_output_tokens: u64,
        compaction_summary: Option<&str>,
        web_search_replay: &WebSearchReplayCodec,
    ) -> Value {
        self.claude_json_with_context_management(
            id,
            model,
            max_tokens,
            current_round_output_tokens,
            compaction_summary,
            None,
            None,
            self.usage.input_tokens,
            &ClaudeContextEditStats::default(),
            web_search_replay,
        )
        .expect("test response replay encryption")
    }

    #[allow(clippy::too_many_arguments)]
    pub fn claude_json_with_context_management(
        &self,
        id: &str,
        model: &str,
        max_tokens: u32,
        current_round_output_tokens: u64,
        compaction_summary: Option<&str>,
        compaction_iteration: Option<CompactionIterationUsage>,
        auto_compaction_original_input_tokens: Option<u64>,
        effective_input_tokens: u64,
        context_edit_stats: &ClaudeContextEditStats,
        web_search_replay: &WebSearchReplayCodec,
    ) -> Result<Value, WebSearchReplayError> {
        let mut content = Vec::new();
        if let Some(summary) = compaction_summary {
            content.push(json!({"type":"compaction","content":summary}));
        }
        if !self.reasoning.is_empty() || self.reasoning_signature.is_some() {
            if let Some(signature) = &self.reasoning_signature {
                content.push(json!({
                    "type":"thinking",
                    "thinking":self.reasoning,
                    "signature":signature
                }));
            } else if !self.reasoning.is_empty() {
                // Tagged fallback reasoning has no upstream-verifiable
                // signature. Return it as ordinary visible text so a client
                // cannot mistake a locally fabricated token for a valid
                // round-trippable Claude thinking block.
                content.push(json!({
                    "type":"text",
                    "text":format!("<thinking>{}</thinking>", self.reasoning)
                }));
            }
        }
        if !self.redacted_thinking.is_empty() {
            content.push(json!({
                "type":"redacted_thinking",
                "data":self.redacted_thinking
            }));
        }
        if self.claude_server_events.is_empty() {
            for search in &self.tool_searches {
                append_tool_search_blocks(&mut content, search);
            }
            for search in &self.web_searches {
                append_web_search_blocks(&mut content, search, web_search_replay)?;
            }
        } else {
            for event in &self.claude_server_events {
                match event {
                    ClaudeServerEvent::ToolSearch {
                        index,
                        preceding_text,
                    } => {
                        if !preceding_text.is_empty() {
                            content.push(json!({"type":"text","text":preceding_text}));
                        }
                        if let Some(search) = self.tool_searches.get(*index) {
                            append_tool_search_blocks(&mut content, search);
                        }
                    }
                    ClaudeServerEvent::WebSearch {
                        index,
                        preceding_text,
                    } => {
                        if !preceding_text.is_empty() {
                            content.push(json!({"type":"text","text":preceding_text}));
                        }
                        if let Some(search) = self.web_searches.get(*index) {
                            append_web_search_blocks(&mut content, search, web_search_replay)?;
                        }
                    }
                }
            }
        }
        let citations = web_search_citations(&self.web_searches, &self.text, web_search_replay)?;
        let mut answer_text = self.text.clone();
        answer_text.push_str(&kiro_visible_references(&self.citations));
        if !answer_text.is_empty() {
            if citations.is_empty() {
                content.push(json!({"type":"text","text":answer_text}));
            } else {
                content.push(json!({
                    "type":"text",
                    "text":answer_text,
                    "citations":citations
                }));
            }
        }
        for tool in self.tools.values() {
            content.push(json!({
                "type":"tool_use",
                "id":tool.id,
                "name":tool.name,
                "input":repair_json(&tool.input)
            }));
        }
        let stop = if let Some(reason) = self.stop_reason.as_deref() {
            reason
        } else if !self.tools.is_empty() {
            "tool_use"
        } else if current_round_output_tokens >= u64::from(max_tokens) {
            "max_tokens"
        } else {
            "end_turn"
        };
        let uncached_input_tokens = self
            .usage
            .input_tokens
            .saturating_sub(self.usage.cache_read_tokens)
            .saturating_sub(self.usage.cache_write_tokens);
        let mut response = json!({
            "id":id,"type":"message","role":"assistant","content":content,
            "model":model,"stop_reason":stop,"stop_sequence":self.stop_sequence.as_deref(),
            "usage":{
                "input_tokens":uncached_input_tokens,
                "output_tokens":self.usage.output_tokens,
                "cache_creation_input_tokens":self.usage.cache_write_tokens,
                "cache_read_input_tokens":self.usage.cache_read_tokens
            }
        });
        if !self.web_searches.is_empty() {
            response["usage"]["server_tool_use"] = json!({
                "web_search_requests":self.web_searches.iter()
                    .filter(|search| search.executed)
                    .count()
            });
        }
        if let Some(compaction) = compaction_iteration {
            response["usage"]["iterations"] = json!([
                {
                    "type":"compaction",
                    "input_tokens":compaction.input_tokens,
                    "output_tokens":compaction.output_tokens
                },
                {
                    "type":"message",
                    "input_tokens":self.usage.input_tokens,
                    "output_tokens":self.usage.output_tokens
                }
            ]);
        }
        let mut applied_edits = context_edit_stats.applied_edits();
        if let Some(original_input_tokens) = auto_compaction_original_input_tokens {
            applied_edits.push(json!({
                    "type":"compact_20260112",
                    "reason":"model_mapping_overflow",
                    "original_input_tokens":original_input_tokens,
                    "compacted_input_tokens":effective_input_tokens
            }));
        }
        if !applied_edits.is_empty() {
            response["context_management"] = json!({"applied_edits":applied_edits});
        }
        Ok(response)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn openai_json(
        &self,
        id: &str,
        model: &str,
        created: i64,
        max_tokens: Option<u32>,
        current_round_output_tokens: u64,
        thinking_format: crosslane_core::config::ThinkingOutputFormat,
        tool_identities: &std::collections::HashMap<String, OpenAiToolIdentity>,
    ) -> Value {
        let tools = self
            .tools
            .values()
            .map(|tool| {
                let identity = tool_identities.get(&tool.name);
                let name = identity.map_or(tool.name.as_str(), |identity| identity.name.as_str());
                if identity.is_some_and(|identity| identity.kind == "custom") {
                    let input = custom_tool_input(&tool.input);
                    json!({"id":tool.id,"type":"custom","custom":{"name":name,"input":input}})
                } else {
                    json!({
                        "id":tool.id,"type":"function",
                        "function":{"name":name,"arguments":repair_json(&tool.input).to_string()}
                    })
                }
            })
            .collect::<Vec<_>>();
        let finish = if !tools.is_empty() {
            "tool_calls"
        } else if self.stop_reason.as_deref() == Some("max_tokens")
            || max_tokens.is_some_and(|maximum| current_round_output_tokens >= u64::from(maximum))
        {
            "length"
        } else {
            "stop"
        };
        let mut answer_text = self.text.clone();
        answer_text.push_str(&kiro_visible_references(&self.citations));
        let content = match thinking_format {
            crosslane_core::config::ThinkingOutputFormat::Claude if !self.reasoning.is_empty() => {
                let mut tagged = format!("<thinking>{}</thinking>", self.reasoning);
                tagged.push_str(&answer_text);
                json!(tagged)
            }
            _ if answer_text.is_empty() => Value::Null,
            _ => json!(answer_text),
        };
        let mut message = json!({
            "role":"assistant",
            "content":content,
            "tool_calls":tools
        });
        if thinking_format == crosslane_core::config::ThinkingOutputFormat::Openai
            && !self.reasoning.is_empty()
        {
            message["reasoning_content"] = json!(self.reasoning);
        }
        json!({
            "id":id,"object":"chat.completion","created":created,"model":model,
            "choices":[{"index":0,"message":message,"finish_reason":finish}],
            "usage":{
                "prompt_tokens":self.usage.input_tokens,
                "completion_tokens":self.usage.output_tokens,
                "total_tokens":self.usage.input_tokens+self.usage.output_tokens,
                "prompt_tokens_details":{"cached_tokens":self.usage.cache_read_tokens},
                "completion_tokens_details":{"reasoning_tokens":self.usage.reasoning_tokens}
            }
        })
    }
}

fn append_tool_search_blocks(content: &mut Vec<Value>, search: &ClaudeToolSearchTrace) {
    if search.emission != ClaudeServerToolEmission::ResultOnly {
        content.push(json!({
            "type":"server_tool_use",
            "id":search.id,
            "name":search.name,
            "input":search.input
        }));
    }
    if search.emission == ClaudeServerToolEmission::Pending {
        return;
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
    content.push(json!({
        "type":"tool_search_tool_result",
        "tool_use_id":search.id,
        "content":result
    }));
}

fn append_web_search_blocks(
    content: &mut Vec<Value>,
    search: &ClaudeWebSearchTrace,
    web_search_replay: &WebSearchReplayCodec,
) -> Result<(), WebSearchReplayError> {
    if search.emission != ClaudeServerToolEmission::ResultOnly {
        content.push(json!({
            "type":"server_tool_use",
            "id":search.id,
            "name":"web_search",
            "input":search.input
        }));
    }
    if search.emission == ClaudeServerToolEmission::Pending {
        return Ok(());
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
                        "encrypted_content":web_search_replay.try_encrypt(result)?
                    }))
                })
                .collect::<Result<Vec<_>, WebSearchReplayError>>()?,
        )
    };
    content.push(json!({
        "type":"web_search_tool_result",
        "tool_use_id":search.id,
        "caller":{"type":"direct"},
        "content":result
    }));
    Ok(())
}

pub(crate) fn web_search_citations(
    searches: &[ClaudeWebSearchTrace],
    answer_text: &str,
    web_search_replay: &WebSearchReplayCodec,
) -> Result<Vec<Value>, WebSearchReplayError> {
    const MAX_CITATIONS: usize = 20;

    let mut seen = std::collections::HashSet::new();
    searches
        .iter()
        .filter(|search| search.emission != ClaudeServerToolEmission::Pending)
        .flat_map(|search| search.results.iter())
        // Kiro does not return Anthropic's internal citation alignment. Only
        // emit a structured citation when the answer explicitly includes the
        // exact source URL requested by the continuation prompt. This avoids
        // falsely attributing unused search results to the answer.
        .filter(|result| answer_text.contains(&result.url))
        .filter(|result| seen.insert(result.url.clone()))
        .take(MAX_CITATIONS)
        .map(|result| {
            let cited_text = if result.snippet.trim().is_empty() {
                result.title.chars().take(150).collect::<String>()
            } else {
                result.snippet.chars().take(150).collect::<String>()
            };
            Ok(json!({
                "type":"web_search_result_location",
                "url":result.url,
                "title":result.title,
                "encrypted_index":web_search_replay.try_encrypt(result)?,
                "cited_text":cited_text
            }))
        })
        .collect()
}

/// Kiro's target ranges identify positions in the generated answer, whereas
/// Claude's char/page/block locations address the original source document.
/// Without source coordinates and a trustworthy document-index mapping, a
/// native Claude citation would fabricate attribution. Keep these references
/// visible in both public protocols instead.
pub(crate) fn kiro_visible_references(citations: &[KiroCitation]) -> String {
    let mut seen = std::collections::HashSet::new();
    let citations = citations
        .iter()
        .filter(|citation| seen.insert(kiro_citation_key(citation)))
        .take(20)
        .collect::<Vec<_>>();
    format_visible_references(&citations)
}

pub(crate) fn kiro_citation_key(citation: &KiroCitation) -> String {
    format!(
        "{}\0{}\0{}",
        citation.kind,
        citation.link,
        citation.text.as_deref().unwrap_or_default()
    )
}

fn format_visible_references(citations: &[&KiroCitation]) -> String {
    if citations.is_empty() {
        return String::new();
    }
    let mut output = String::from("\n\nReferences:\n");
    for citation in citations {
        let default_label = match citation.kind.as_str() {
            "web" => "Web source",
            "code" => "Code reference",
            "document" => "Document source",
            _ => "Source",
        };
        let label = citation.text.as_deref().unwrap_or(default_label);
        let label = label.replace(['\r', '\n'], " ");
        let link = citation.link.replace(['\r', '\n'], "");
        output.push_str("- ");
        let label = label.trim();
        let link = link.trim();
        if !label.is_empty() {
            output.push_str(label);
        }
        if !label.is_empty() && !link.is_empty() {
            output.push_str(": ");
        }
        if !link.is_empty() {
            output.push_str(link);
        }
        output.push('\n');
    }
    output.pop();
    output
}

fn merge_usage(output: &mut UsageInfo, value: &Value) {
    let uncached = find_number(value, &["uncachedInputTokens"]);
    let total_input = find_number(value, &["inputTokens", "input_tokens"]);
    let cache_read = find_number(
        value,
        &[
            "cacheReadInputTokens",
            "cacheReadTokens",
            "cache_read_input_tokens",
            "cache_read_tokens",
        ],
    );
    let cache_write = find_number(
        value,
        &[
            "cacheWriteInputTokens",
            "cacheWriteTokens",
            "cache_creation_input_tokens",
            "cache_write_tokens",
        ],
    );
    let cache_tokens = cache_read
        .unwrap_or_default()
        .saturating_add(cache_write.unwrap_or_default());
    if let Some(uncached) = uncached {
        output.input_tokens = uncached.saturating_add(cache_tokens);
    } else if let Some(input) = total_input {
        // Kiro's generic inputTokens field is the total prompt size. Cache
        // counters describe subsets of it and must not be added again.
        output.input_tokens = input.max(cache_tokens);
    } else if cache_tokens > 0 {
        output.input_tokens = cache_tokens;
    }
    output.output_tokens =
        find_number(value, &["outputTokens", "output_tokens"]).unwrap_or(output.output_tokens);
    if output.input_tokens == 0 {
        if let Some(total) = find_number(value, &["totalTokens", "total_tokens"]) {
            output.input_tokens = total.saturating_sub(output.output_tokens);
        }
    }
    output.cache_read_tokens = cache_read.unwrap_or(output.cache_read_tokens);
    output.cache_write_tokens = cache_write.unwrap_or(output.cache_write_tokens);
    output.reasoning_tokens = find_number(value, &["reasoningTokens", "reasoning_tokens"])
        .unwrap_or(output.reasoning_tokens);
    if output.credits <= 0.0 {
        output.credits = find_float(
            value,
            &[
                "credits",
                "creditsConsumed",
                "creditsUsed",
                "creditUsage",
                "billedCredits",
                "usedCredits",
                "charge",
                "cost",
                "meteredUsage",
                "usage",
            ],
        )
        .filter(|credits| *credits > 0.0)
        .unwrap_or(output.credits);
    }
}

fn find_number(value: &Value, keys: &[&str]) -> Option<u64> {
    find_numeric_value(value, keys).and_then(|value| match value {
        Value::Number(number) => number.as_u64().or_else(|| {
            number
                .as_f64()
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map(|value| value as u64)
        }),
        Value::String(value) => value
            .parse::<f64>()
            .ok()
            .and_then(|value| (value.is_finite() && value >= 0.0).then_some(value as u64)),
        _ => None,
    })
}

fn find_float(value: &Value, keys: &[&str]) -> Option<f64> {
    find_numeric_value(value, keys).and_then(|value| match value {
        Value::Number(number) => number.as_f64(),
        Value::String(value) => value.parse().ok(),
        _ => None,
    })
}

fn find_numeric_value<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    match value {
        Value::Object(map) => keys
            .iter()
            .filter_map(|key| map.get(*key))
            .find(|candidate| candidate.is_number() || candidate.is_string())
            .or_else(|| {
                map.values()
                    .find_map(|child| find_numeric_value(child, keys))
            }),
        Value::Array(array) => array
            .iter()
            .find_map(|child| find_numeric_value(child, keys)),
        _ => None,
    }
}

pub fn repair_json(input: &str) -> Value {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return json!({});
    }
    serde_json::from_str(trimmed).unwrap_or_else(|_| {
        let mut repaired = normalize_partial_json(trimmed);
        let mut stack = Vec::new();
        let mut in_string = false;
        let mut escaped = false;
        for character in repaired.chars() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == '"' {
                    in_string = false;
                }
                continue;
            }
            match character {
                '"' => in_string = true,
                '{' => stack.push('}'),
                '[' => stack.push(']'),
                '}' | ']' if stack.last() == Some(&character) => {
                    stack.pop();
                }
                _ => {}
            }
        }
        if in_string {
            repaired.push('"');
        }
        while repaired.ends_with(char::is_whitespace) {
            repaired.pop();
        }
        if repaired.ends_with(',') {
            repaired.pop();
        }
        repaired.extend(stack.into_iter().rev());
        serde_json::from_str(&repaired).unwrap_or_else(|_| json!({"raw":trimmed}))
    })
}

/// Unwrap the object schema used to expose a Responses custom tool to Kiro.
/// Models occasionally return a JSON string or use a different single field
/// despite the schema; preserve the intended free-form payload in those cases
/// instead of sending JSON quoting/wrapping back to the client.
pub fn custom_tool_input(input: &str) -> String {
    match repair_json(input) {
        Value::Object(map) => map
            .get("input")
            .and_then(Value::as_str)
            .or_else(|| {
                if map.len() == 1 {
                    map.values().next().and_then(Value::as_str)
                } else {
                    None
                }
            })
            .map(str::to_owned)
            .unwrap_or_else(|| input.to_owned()),
        Value::String(value) => value,
        _ => input.to_owned(),
    }
}

fn normalize_partial_json(input: &str) -> String {
    let characters = input.chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(input.len());
    let mut in_string = false;
    let mut escaped = false;
    for (index, character) in characters.iter().copied().enumerate() {
        if in_string {
            match character {
                '\n' => output.push_str("\\n"),
                '\r' => output.push_str("\\r"),
                '\t' => output.push_str("\\t"),
                _ => output.push(character),
            }
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        if character == '"' {
            in_string = true;
            output.push(character);
            continue;
        }
        if character == ','
            && characters[index + 1..]
                .iter()
                .find(|character| !character.is_whitespace())
                .is_some_and(|character| matches!(*character, '}' | ']'))
        {
            continue;
        }
        output.push(character);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replay_codec() -> WebSearchReplayCodec {
        WebSearchReplayCodec::from_key([0x5A; 32])
    }

    fn assistant_text(events: &[KiroEvent]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                KiroEvent::AssistantResponse { content } => Some(content.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn audit_decoded_text_preserves_literal_markup_at_every_chunk_boundary() {
        let text = "```xml\n<tool_use>示例</tool_use>\n```\nraw\0text";
        for split in text
            .char_indices()
            .map(|(index, _)| index)
            .chain([text.len()])
        {
            let mut decoded = DecodedResponse::default();
            for chunk in [&text[..split], &text[split..]] {
                decoded
                    .push(KiroEvent::AssistantResponse {
                        content: chunk.into(),
                    })
                    .unwrap();
            }
            assert_eq!(decoded.text, text, "split={split}");
        }
    }

    #[test]
    fn audit_thinking_state_survives_interleaved_metadata_at_every_boundary() {
        let text = "before <thinking>private reasoning</thinking>after";
        for (enabled, omitted) in [(false, false), (true, false), (true, true)] {
            for split in 0..=text.len() {
                let mut filter = ThinkingContentFilter::new(enabled).with_omitted_summary(omitted);
                let mut events = filter.push(KiroEvent::AssistantResponse {
                    content: text[..split].into(),
                });
                events.extend(filter.push(KiroEvent::Usage {
                    usage: json!({"inputTokens":1}),
                }));
                events.extend(filter.push(KiroEvent::AssistantResponse {
                    content: text[split..].into(),
                }));
                events.extend(filter.finish());
                assert_eq!(
                    assistant_text(&events),
                    if enabled && !omitted {
                        text
                    } else {
                        "before after"
                    },
                    "enabled={enabled}, omitted={omitted}, split={split}"
                );
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(event, KiroEvent::Usage { .. }))
                        .count(),
                    1
                );
            }
        }
    }

    #[test]
    fn audit_thinking_filter_preserves_markdown_examples_at_every_boundary() {
        for literal in ["`<thinking>`", "```xml\n<thinking>\n```", "    <thinking>"] {
            let text = format!("{literal}\nanswer <thinking>private</thinking>done");
            for split in 0..=text.len() {
                let mut filter = ThinkingContentFilter::new(false);
                let mut events = filter.push(KiroEvent::AssistantResponse {
                    content: text[..split].into(),
                });
                events.extend(filter.push(KiroEvent::AssistantResponse {
                    content: text[split..].into(),
                }));
                events.extend(filter.finish());
                assert_eq!(
                    assistant_text(&events),
                    format!("{literal}\nanswer done"),
                    "split={split}"
                );
            }
        }
    }

    #[test]
    fn audit_thinking_filter_bounds_pending_reasoning_and_streams_visible_text() {
        for enabled in [false, true] {
            let mut filter = ThinkingContentFilter::new(enabled);
            let mut events = filter.push(KiroEvent::AssistantResponse {
                content: "<thinking>".into(),
            });
            for _ in 0..32 {
                events.extend(filter.push(KiroEvent::AssistantResponse {
                    content: "x".repeat(1024),
                }));
                assert!(
                    filter.pending.len() < THINKING_END_TAG.len(),
                    "reasoning retained without bound"
                );
            }
            if enabled {
                assert_eq!(
                    assistant_text(&events),
                    format!("<thinking>{}", "x".repeat(32 * 1024))
                );
            }
            events.extend(filter.push(KiroEvent::AssistantResponse {
                content: "</thinking>answer".into(),
            }));
            events.extend(filter.finish());
            assert_eq!(
                assistant_text(&events),
                if enabled {
                    format!("<thinking>{}</thinking>answer", "x".repeat(32 * 1024))
                } else {
                    "answer".into()
                }
            );
        }
    }

    #[test]
    fn audit_tool_recovery_ignores_markdown_inside_tool_arguments() {
        let text = concat!(
            "<function_calls><invoke name=\"write_file\">",
            "<parameter name=\"content\">\n```xml\n</parameter></invoke></function_calls>\n",
            "<tool_use name=\"lookup\">{}</tool_use>"
        );
        for split in 0..=text.len() {
            let mut filter = ToolLeakFilter::new(true, ["write_file".into(), "lookup".into()]);
            let mut events = filter.push(KiroEvent::AssistantResponse {
                content: text[..split].into(),
            });
            events.extend(filter.push(KiroEvent::AssistantResponse {
                content: text[split..].into(),
            }));
            events.extend(filter.finish());
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, KiroEvent::ToolUse { .. }))
                    .count(),
                2,
                "argument opened a Markdown fence at split={split}"
            );
            assert_eq!(assistant_text(&events), "\n");
        }
    }

    #[test]
    fn audit_tool_recovery_resumes_after_incomplete_xml_examples() {
        for prefix in [
            "```xml\n<tool_use name=\"lookup\">\n```\n",
            "<thinking>\n<tool_use name=\"lookup\">example\n</thinking>\n",
        ] {
            let text = format!("{prefix}<tool_use name=\"lookup\">{{}}</tool_use>");
            for split in 0..=text.len() {
                let mut filter = ToolLeakFilter::new(true, ["lookup".into()]);
                let mut events = filter.push(KiroEvent::AssistantResponse {
                    content: text[..split].into(),
                });
                events.extend(filter.push(KiroEvent::AssistantResponse {
                    content: text[split..].into(),
                }));
                events.extend(filter.finish());
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(event, KiroEvent::ToolUse { .. }))
                        .count(),
                    1,
                    "example swallowed a real call at split={split}"
                );
                assert_eq!(assistant_text(&events), prefix);
            }
        }
    }

    #[test]
    fn audit_xml_parameters_preserve_strings_in_composed_schemas() {
        for property in [
            json!({"type":"string"}),
            json!({"type":["string","null"]}),
            json!({"enum":["true"]}),
            json!({"const":"true"}),
            json!({"anyOf":[{"type":"string"},{"type":"null"}]}),
            json!({"oneOf":[{"type":"string"},{"type":"boolean"}]}),
            json!({"allOf":[{"type":"string"},{"minLength":1}]}),
            json!({"$ref":"#/$defs/text"}),
        ] {
            let schema = json!({"type":"object", "$defs":{"text":{"type":"string"}},
                "properties":{"content":property}});
            let parsed = parse_parameters(
                "<parameter name=\"content\">true</parameter>",
                Some(&schema),
            )
            .unwrap();
            assert_eq!(parsed["content"], json!("true"), "{schema}");
        }
    }

    #[test]
    fn audit_xml_schema_resolution_is_local_bounded_and_handles_root_composition() {
        let xml =
            "<parameter name=\"content\">42</parameter><parameter name=\"count\">42</parameter>";
        let object = json!({"properties":{
            "content":{"$ref":"#/$defs/text"}, "count":{"type":"integer"}
        }});
        for schema in [
            json!({"$ref":"#/$defs/request", "$defs":{"request":object,"text":{"type":"string"}}}),
            json!({"allOf":[object], "$defs":{"text":{"type":"string"}}}),
        ] {
            assert_eq!(
                parse_parameters(xml, Some(&schema)),
                Some(json!({"content":"42","count":42}))
            );
        }
        for schema in [
            json!({"properties":{"content":{"$ref":"https://example.invalid/schema"}}}),
            json!({"properties":{"content":{"$ref":"#/$defs/missing"}}}),
            json!({"properties":{"content":{"$ref":"#/$defs/cycle"}}, "$defs":{
                "cycle":{"anyOf":[{"$ref":"#/$defs/cycle"},{"$ref":"#/$defs/cycle"}]}
            }}),
        ] {
            assert!(parse_parameters(xml, Some(&schema)).is_none());
        }
    }

    #[test]
    fn audit_xml_string_hints_respect_schema_intersections() {
        let xml = "<parameter name=\"content\">42</parameter>";
        for property in [
            json!({"type":"integer", "enum":["42",42]}),
            json!({"allOf":[{"type":["string","integer"]},{"type":"integer"}]}),
            json!({"$ref":"#/$defs/numeric", "anyOf":[{"type":"string"},{"type":"integer"}]}),
        ] {
            let schema =
                json!({"properties":{"content":property}, "$defs":{"numeric":{"type":"integer"}}});
            assert_eq!(
                parse_parameters(xml, Some(&schema)),
                Some(json!({"content":42})),
                "{schema}"
            );
        }
        let contradictory =
            json!({"properties":{"content":{"allOf":[{"type":"string"},{"type":"integer"}]}}});
        assert!(parse_parameters(xml, Some(&contradictory)).is_none());
    }

    #[test]
    fn tagged_thinking_is_normalized_across_chunk_boundaries() {
        let mut filter = ThinkingContentFilter::new(true);
        let mut events = filter.push(KiroEvent::AssistantResponse {
            content: "before <thin".into(),
        });
        events.extend(filter.push(KiroEvent::AssistantResponse {
            content: "king>hidden</think".into(),
        }));
        events.extend(filter.push(KiroEvent::AssistantResponse {
            content: "ing>after".into(),
        }));
        events.extend(filter.finish());

        assert_eq!(
            assistant_text(&events),
            "before <thinking>hidden</thinking>after"
        );
        assert!(events
            .iter()
            .all(|event| matches!(event, KiroEvent::AssistantResponse { .. })));
    }

    #[test]
    fn disabled_thinking_suppresses_native_and_tagged_reasoning() {
        let mut filter = ThinkingContentFilter::new(false);
        let mut events = filter.push(KiroEvent::AssistantResponse {
            content: "before <thinking>hidden</thinking>after".into(),
        });
        events.extend(filter.push(KiroEvent::Reasoning {
            content: "also hidden".into(),
            signature: None,
            redacted_content: None,
        }));
        events.extend(filter.finish());

        assert_eq!(
            events,
            vec![
                KiroEvent::AssistantResponse {
                    content: "before ".into()
                },
                KiroEvent::AssistantResponse {
                    content: "after".into()
                }
            ]
        );
    }

    #[test]
    fn omitted_thinking_hides_text_but_preserves_native_signatures() {
        let mut filter = ThinkingContentFilter::new(true).with_omitted_summary(true);
        let mut events = filter.push(KiroEvent::AssistantResponse {
            content: "<thinking>hidden tags</thinking>answer".into(),
        });
        events.extend(filter.push(KiroEvent::Reasoning {
            content: "hidden summary".into(),
            signature: Some("native-signature".into()),
            redacted_content: None,
        }));
        events.extend(filter.push(KiroEvent::Reasoning {
            content: "unsigned hidden summary".into(),
            signature: None,
            redacted_content: None,
        }));
        events.extend(filter.finish());
        assert_eq!(
            events,
            vec![
                KiroEvent::AssistantResponse {
                    content: "answer".into()
                },
                KiroEvent::Reasoning {
                    content: String::new(),
                    signature: Some("native-signature".into()),
                    redacted_content: None
                },
            ]
        );
    }

    #[test]
    fn incomplete_start_tag_is_preserved_as_text_on_finish() {
        let mut filter = ThinkingContentFilter::new(true);
        let mut events = filter.push(KiroEvent::AssistantResponse {
            content: "literal <thin".into(),
        });
        events.extend(filter.finish());

        let text = events
            .iter()
            .filter_map(|event| match event {
                KiroEvent::AssistantResponse { content } => Some(content.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(text, "literal <thin");
        assert!(events
            .iter()
            .all(|event| matches!(event, KiroEvent::AssistantResponse { .. })));
    }

    #[test]
    fn recovers_split_function_calls_and_keeps_visible_text() {
        let mut filter = ToolLeakFilter::new(true, ["write_file".into()]);
        let first = filter.push(KiroEvent::AssistantResponse {
            content:
                "Before\n<function_calls><invoke name=\"write_file\"><parameter name=\"path\">"
                    .into(),
        });
        let second = filter.push(KiroEvent::AssistantResponse {
            content: "/tmp/a.txt</parameter><parameter name=\"overwrite\">true</parameter>".into(),
        });
        let third = filter.push(KiroEvent::AssistantResponse {
            content: "</invoke></function_calls> after".into(),
        });
        assert_eq!(
            first,
            vec![KiroEvent::AssistantResponse {
                content: "Before\n".into()
            }]
        );
        assert!(second.is_empty());
        assert_eq!(
            third,
            vec![KiroEvent::AssistantResponse {
                content: " after".into()
            }]
        );
        let recovered = filter.finish();
        assert_eq!(recovered.len(), 1);
        match &recovered[0] {
            KiroEvent::ToolUse {
                name, input_delta, ..
            } => {
                assert_eq!(name, "write_file");
                assert_eq!(repair_json(input_delta)["path"], "/tmp/a.txt");
                assert_eq!(repair_json(input_delta)["overwrite"], true);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn flushes_incomplete_marker_as_literal_text() {
        let mut filter = ToolLeakFilter::new(true, ["write_file".into()]);
        let visible = filter.push(KiroEvent::AssistantResponse {
            content: "Use <function_calls as literal text".into(),
        });
        assert_eq!(visible.len(), 1);
        let tail = filter.finish();
        assert_eq!(tail.len(), 1);
    }

    #[test]
    fn tool_leak_recovery_preserves_display_text_and_invalid_blocks_at_every_chunk_boundary() {
        let call = "<function_calls><invoke name=\"lookup\"><parameter name=\"query\">weather</parameter></invoke></function_calls>";
        for text in [
            format!("Example: {call}"),
            format!("`inline\n{call}\ncode`"),
            format!("Some ``inline\n{call}\ncode``"),
            format!("```xml\n{call}\n```"),
            format!("~~~~xml\n```\n{call}\n~~~~"),
            format!("    {call}"),
            call.replace("lookup", "undeclared"),
            call.replace("</parameter>", ""),
            call.replace("</function_calls>", "unparsed text</function_calls>"),
            "<function_calls>example only</function_calls>".into(),
            "<tool_use name=\"lookup\">invalid json</tool_use>".into(),
            "<tool_use name=\"lookup\">[]</tool_use>".into(),
            "<tool_use_example name=\"lookup\">{}</tool_use>".into(),
            "<tool_use description='example name=\"lookup\"'>{}</tool_use>".into(),
            "<tool_use name=\"lookup\" name=\"other\">{}</tool_use>".into(),
            "<tool_use name=\"lookup\" tool=\"other\">{}</tool_use>".into(),
            "<tool_use name=\"lookup\"invalid>{}</tool_use>".into(),
        ] {
            for split in 0..=text.len() {
                let mut filter = ToolLeakFilter::new(true, ["lookup".into()]);
                let mut events = Vec::new();
                for chunk in [&text[..split], &text[split..]] {
                    events.extend(filter.push(KiroEvent::AssistantResponse {
                        content: chunk.into(),
                    }));
                }
                events.extend(filter.finish());
                let visible: String = events
                    .into_iter()
                    .map(|event| match event {
                        KiroEvent::AssistantResponse { content } => content,
                        other => panic!("display text produced {other:?}: {text}, split={split}"),
                    })
                    .collect();
                assert_eq!(visible, text, "split={split}");
            }
        }
    }

    #[test]
    fn tool_leak_recovery_resumes_after_split_fence_and_requires_declared_tools() {
        let text = "```xml\n<tool_use name=\"lookup\">{}</tool_use>\n```\n  <tool_use name=\"lookup\">{\"q\":1}</tool_use>";
        for known in [vec![], vec!["lookup".into()]] {
            let mut filter = ToolLeakFilter::new(true, known.clone());
            let mut events = Vec::new();
            for character in text.chars() {
                events.extend(filter.push(KiroEvent::AssistantResponse {
                    content: character.to_string(),
                }));
            }
            events.extend(filter.finish());
            let tools = events
                .iter()
                .filter(|event| matches!(event, KiroEvent::ToolUse { .. }))
                .count();
            assert_eq!(tools, known.len());
            let visible = events
                .into_iter()
                .filter_map(|event| match event {
                    KiroEvent::AssistantResponse { content } => Some(content),
                    _ => None,
                })
                .collect::<String>();
            assert_eq!(
                visible,
                if known.is_empty() {
                    text
                } else {
                    "```xml\n<tool_use name=\"lookup\">{}</tool_use>\n```\n  "
                }
            );
        }
    }

    #[test]
    fn tool_leak_recovery_never_executes_xml_inside_tagged_thinking() {
        let text = "<thinking>\n<tool_use name=\"lookup\">{\"q\":\"example\"}</tool_use>\n</thinking>\nDone.";
        for split in 0..=text.len() {
            let mut filter = ToolLeakFilter::new(true, ["lookup".into()]);
            let mut events = Vec::new();
            for chunk in [&text[..split], &text[split..]] {
                events.extend(filter.push(KiroEvent::AssistantResponse {
                    content: chunk.into(),
                }));
            }
            events.extend(filter.finish());
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, KiroEvent::ToolUse { .. })),
                "thinking became executable at split {split}"
            );
            let visible: String = events
                .into_iter()
                .filter_map(|event| match event {
                    KiroEvent::AssistantResponse { content } => Some(content),
                    _ => None,
                })
                .collect();
            assert_eq!(visible, text);
        }
    }

    #[test]
    fn tool_recovery_resumes_after_thinking_and_literal_tags() {
        for example in [
            "```xml\n<thinking>\n```\n",
            "`<thinking>`\n",
            "    <thinking>\n",
            "<thinking>\n```xml\n<tool_use name=\"lookup\">{}</tool_use>\n</thinking>\n",
            "<thinking>\n`unfinished inline\n</thinking>\n",
        ] {
            let text = format!("{example}<tool_use name=\"lookup\">{{}}</tool_use>");
            for split in 0..=text.len() {
                let mut filter = ToolLeakFilter::new(true, ["lookup".into()]);
                let mut events = Vec::new();
                for chunk in [&text[..split], &text[split..]] {
                    events.extend(filter.push(KiroEvent::AssistantResponse {
                        content: chunk.into(),
                    }));
                }
                events.extend(filter.finish());
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(event, KiroEvent::ToolUse { .. }))
                        .count(),
                    1,
                    "recovery disabled by preceding text {example:?} at split {split}"
                );
                let visible: String = events
                    .into_iter()
                    .filter_map(|event| match event {
                        KiroEvent::AssistantResponse { content } => Some(content),
                        _ => None,
                    })
                    .collect();
                assert_eq!(visible, example);
            }
        }
    }

    #[test]
    fn tool_leak_recovery_preserves_declared_string_parameters_and_whitespace() {
        let request: crosslane_translate::ClaudeRequest = serde_json::from_value(json!({
            "model":"claude-opus-5", "max_tokens":1024,
            "messages":[{"role":"user","content":"write file"}],
            "tools":[{"name":"write_file", "input_schema":{"type":"object","properties":{
                "content":{"type":"string"}, "overwrite":{"type":"boolean"}
            }}}]
        }))
        .unwrap();
        let payload = crosslane_translate::claude_to_kiro(
            &request,
            &crosslane_translate::TranslationOptions::new("claude-opus-5", "AI_EDITOR"),
        );
        for content in ["true", "42", "  indented\n", "\n", "{\"text\":true}"] {
            let mut filter = ToolLeakFilter::for_payload(true, &payload);
            filter.push(KiroEvent::AssistantResponse { content: format!(
                "<function_calls><invoke name=\"write_file\"><parameter name=\"content\">{content}</parameter><parameter name=\"overwrite\">true</parameter></invoke></function_calls>") });
            let events = filter.finish();
            let KiroEvent::ToolUse { input_delta, .. } = &events[0] else {
                panic!("call not recovered");
            };
            let input: Value = serde_json::from_str(input_delta).unwrap();
            assert_eq!(input["content"], json!(content));
            assert_eq!(input["overwrite"], true);
        }
    }

    #[test]
    fn stop_sequence_filter_matches_across_chunks_without_leaking_the_sequence() {
        let mut filter = StopSequenceFilter::new(&["<END>".into(), "结束".into()]);
        assert_eq!(filter.push("hello <E"), "hello ");
        assert_eq!(filter.push("ND>ignored"), "");
        assert_eq!(filter.matched(), Some("<END>"));
        assert_eq!(filter.visible_bytes(), "hello ".len());
        assert_eq!(filter.finish(), "");

        let mut unicode = StopSequenceFilter::new(&["结束".into()]);
        assert_eq!(unicode.push("完成结"), "完成");
        assert_eq!(unicode.push("束ignored"), "");
        assert_eq!(unicode.matched(), Some("结束"));
    }

    #[test]
    fn stop_sequence_filter_uses_generation_order_and_resets_at_block_boundaries() {
        let mut earliest = StopSequenceFilter::new(&["abc".into(), "b".into()]);
        assert_eq!(earliest.push("abc"), "a");
        assert_eq!(earliest.matched(), Some("b"));

        let mut boundary = StopSequenceFilter::new(&["END".into()]);
        assert_eq!(boundary.push("E"), "");
        assert_eq!(boundary.finish(), "E");
        assert_eq!(boundary.push("ND"), "ND");
        assert_eq!(boundary.finish(), "");
        assert_eq!(boundary.matched(), None);
        assert_eq!(boundary.visible_bytes(), "END".len());
    }

    #[test]
    fn claude_response_truncates_and_reports_a_matched_stop_sequence() {
        let mut decoded = DecodedResponse {
            text: "before END after".into(),
            ..DecodedResponse::default()
        };
        decoded.stop_at_sequence("before ".into(), "END".into());
        let response = decoded.claude_json("msg", "model", 100, 1, None, &replay_codec());

        assert_eq!(response["content"][0]["text"], "before ");
        assert_eq!(response["stop_reason"], "stop_sequence");
        assert_eq!(response["stop_sequence"], "END");
    }

    #[test]
    fn response_format_and_stop_reason_use_config_and_current_round() {
        let decoded = DecodedResponse {
            text: "answer".into(),
            reasoning: "thought".into(),
            usage: UsageInfo {
                output_tokens: 100,
                reasoning_tokens: 7,
                ..UsageInfo::default()
            },
            ..DecodedResponse::default()
        };
        let claude = decoded.claude_json("msg", "model", 50, 10, None, &replay_codec());
        assert_eq!(claude["stop_reason"], "end_turn");
        let paused = DecodedResponse {
            stop_reason: Some("pause_turn".into()),
            ..DecodedResponse::default()
        }
        .claude_json("msg", "model", 50, 0, None, &replay_codec());
        assert_eq!(paused["stop_reason"], "pause_turn");
        let tagged = decoded.openai_json(
            "chat",
            "model",
            1,
            Some(50),
            10,
            crosslane_core::config::ThinkingOutputFormat::Claude,
            &std::collections::HashMap::new(),
        );
        assert_eq!(
            tagged["choices"][0]["message"]["content"],
            "<thinking>thought</thinking>answer"
        );
        assert!(tagged["choices"][0]["message"]
            .get("reasoning_content")
            .is_none());
        let openai = decoded.openai_json(
            "chat",
            "model",
            1,
            Some(50),
            10,
            crosslane_core::config::ThinkingOutputFormat::Openai,
            &std::collections::HashMap::new(),
        );
        assert_eq!(
            openai["choices"][0]["message"]["reasoning_content"],
            "thought"
        );
    }

    #[test]
    fn claude_response_preserves_tool_search_reference_blocks() {
        let decoded = DecodedResponse {
            tool_searches: vec![ClaudeToolSearchTrace {
                id: "srvtoolu_1".into(),
                name: "tool_search_tool_regex".into(),
                input: json!({"pattern":"github"}),
                references: vec!["mcp__github__list_issues".into()],
                error: None,
                requested_limit: 5,
                matched_count: 1,
                budget_truncated: false,
                emission: ClaudeServerToolEmission::Complete,
            }],
            ..DecodedResponse::default()
        };
        let response = decoded.claude_json("msg", "model", 100, 1, None, &replay_codec());
        assert_eq!(response["content"][0]["type"], "server_tool_use");
        assert_eq!(response["content"][1]["type"], "tool_search_tool_result");
        assert_eq!(
            response["content"][1]["content"]["tool_references"][0]["tool_name"],
            "mcp__github__list_issues"
        );
        assert_eq!(response["stop_reason"], "end_turn");
    }

    #[test]
    fn claude_response_uses_native_web_search_blocks_and_usage() {
        let decoded = DecodedResponse {
            text: "Tokio uses an async runtime: https://tokio.rs".into(),
            web_searches: vec![crosslane_translate::ClaudeWebSearchTrace::success(
                "srvtoolu_web".into(),
                "rust async",
                crosslane_translate::WebSearchResults {
                    query: "rust async".into(),
                    total_results: 1,
                    results: vec![crosslane_translate::WebSearchResult {
                        title: "Tokio".into(),
                        url: "https://tokio.rs".into(),
                        snippet: "runtime".into(),
                        published_date: None,
                    }],
                },
            )],
            ..DecodedResponse::default()
        };
        let response = decoded.claude_json("msg", "model", 100, 1, None, &replay_codec());
        assert_eq!(response["content"][0]["name"], "web_search");
        assert_eq!(response["content"][1]["type"], "web_search_tool_result");
        assert_eq!(
            response["content"][1]["content"][0]["url"],
            "https://tokio.rs"
        );
        assert_eq!(
            response["usage"]["server_tool_use"]["web_search_requests"],
            1
        );
        assert!(response["content"][1]["content"][0]["encrypted_content"]
            .as_str()
            .is_some_and(|value| value.starts_with("crosslane.v2.")));
        assert_eq!(
            response["content"][2]["citations"][0]["url"],
            "https://tokio.rs"
        );
    }

    #[test]
    fn web_search_does_not_cite_results_absent_from_the_answer() {
        let decoded = DecodedResponse {
            text: "The available results do not answer the question.".into(),
            web_searches: vec![ClaudeWebSearchTrace::success(
                "srvtoolu_web".into(),
                "rust async",
                crosslane_translate::WebSearchResults {
                    query: "rust async".into(),
                    total_results: 1,
                    results: vec![crosslane_translate::WebSearchResult {
                        title: "Tokio".into(),
                        url: "https://tokio.rs".into(),
                        snippet: "runtime".into(),
                        published_date: None,
                    }],
                },
            )],
            ..DecodedResponse::default()
        };
        let response = decoded.claude_json("msg", "model", 100, 1, None, &replay_codec());
        assert!(response["content"][2].get("citations").is_none());
    }

    #[test]
    fn targetless_kiro_citations_remain_visible_in_both_protocols() {
        let decoded = DecodedResponse {
            text: "Answer".into(),
            citations: vec![KiroCitation {
                text: Some("Example source".into()),
                link: "https://example.com/source".into(),
                target: json!({}),
                kind: "web".into(),
            }],
            ..DecodedResponse::default()
        };

        let claude = decoded.claude_json("msg", "model", 100, 1, None, &replay_codec());
        assert!(claude["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("Example source: https://example.com/source")));
        assert!(claude["content"][0].get("citations").is_none());

        let openai = decoded.openai_json(
            "chat",
            "model",
            1,
            Some(100),
            1,
            crosslane_core::config::ThinkingOutputFormat::Openai,
            &std::collections::HashMap::new(),
        );
        assert!(openai["choices"][0]["message"]["content"]
            .as_str()
            .is_some_and(|text| text.contains("Example source: https://example.com/source")));
    }

    #[test]
    fn kiro_answer_ranges_never_become_fabricated_source_coordinates() {
        let citations = [
            KiroCitation {
                text: Some("same".into()),
                link: "https://example.com/source".into(),
                target: json!({"range":{"start":0,"end":4}}),
                kind: "document".into(),
            },
            KiroCitation {
                text: Some("same".into()),
                link: "https://example.com/source".into(),
                target: json!({"range":{"start":5,"end":9}}),
                kind: "document".into(),
            },
        ];
        let decoded = DecodedResponse {
            text: "same same".into(),
            citations: citations.to_vec(),
            ..DecodedResponse::default()
        };
        let response = decoded.claude_json("msg", "model", 100, 1, None, &replay_codec());
        let text = response["content"][0]["text"]
            .as_str()
            .expect("answer text");
        assert_eq!(text.matches("https://example.com/source").count(), 1);
        assert!(response["content"][0].get("citations").is_none());
    }

    #[test]
    fn pending_and_resumed_server_tools_emit_only_the_protocol_phase_they_own() {
        let pending = DecodedResponse {
            web_searches: vec![ClaudeWebSearchTrace::pending(
                "srvtoolu_pending".into(),
                json!({"query":"rust"}),
            )],
            ..DecodedResponse::default()
        }
        .claude_json("msg", "model", 100, 1, None, &replay_codec());
        assert_eq!(pending["content"].as_array().map(Vec::len), Some(1));
        assert_eq!(pending["content"][0]["type"], "server_tool_use");
        assert_eq!(
            pending["usage"]["server_tool_use"]["web_search_requests"],
            0
        );

        let resumed = DecodedResponse {
            web_searches: vec![ClaudeWebSearchTrace::success(
                "srvtoolu_pending".into(),
                "rust",
                crosslane_translate::WebSearchResults::default(),
            )
            .result_only()],
            ..DecodedResponse::default()
        }
        .claude_json("msg", "model", 100, 1, None, &replay_codec());
        assert_eq!(resumed["content"].as_array().map(Vec::len), Some(1));
        assert_eq!(resumed["content"][0]["type"], "web_search_tool_result");
        assert_eq!(
            resumed["usage"]["server_tool_use"]["web_search_requests"],
            1
        );
    }

    #[test]
    fn nonstream_server_tools_preserve_pre_call_and_final_text_order() {
        let decoded = DecodedResponse {
            text: "final answer".into(),
            tool_searches: vec![ClaudeToolSearchTrace {
                id: "srvtoolu_tool".into(),
                name: "tool_search_tool_regex".into(),
                input: json!({"pattern":"web"}),
                references: vec!["web_search".into()],
                error: None,
                requested_limit: 5,
                matched_count: 1,
                budget_truncated: false,
                emission: ClaudeServerToolEmission::Complete,
            }],
            web_searches: vec![ClaudeWebSearchTrace::success(
                "srvtoolu_web".into(),
                "news",
                crosslane_translate::WebSearchResults::default(),
            )],
            claude_server_events: vec![
                ClaudeServerEvent::ToolSearch {
                    index: 0,
                    preceding_text: "finding a tool".into(),
                },
                ClaudeServerEvent::WebSearch {
                    index: 0,
                    preceding_text: "searching the web".into(),
                },
            ],
            ..DecodedResponse::default()
        };

        let response = decoded.claude_json("msg", "model", 100, 1, None, &replay_codec());
        let types = response["content"]
            .as_array()
            .expect("content")
            .iter()
            .map(|block| block["type"].as_str().unwrap_or_default())
            .collect::<Vec<_>>();
        assert_eq!(
            types,
            [
                "text",
                "server_tool_use",
                "tool_search_tool_result",
                "text",
                "server_tool_use",
                "web_search_tool_result",
                "text"
            ]
        );
    }

    #[test]
    fn usage_understands_kiro_cache_and_credit_fields() {
        let mut decoded = DecodedResponse::default();
        decoded
            .push(KiroEvent::MessageMetadata {
                usage: json!({"messageMetadataEvent":{"tokenUsage":{
                    "uncachedInputTokens":"10",
                    "cacheReadInputTokens":20,
                    "cacheWriteInputTokens":5,
                    "outputTokens":7,
                    "creditsConsumed":"1.25"
                }}}),
            })
            .expect("usage");
        assert_eq!(decoded.usage.input_tokens, 35);
        assert_eq!(decoded.usage.cache_read_tokens, 20);
        assert_eq!(decoded.usage.cache_write_tokens, 5);
        assert_eq!(decoded.usage.output_tokens, 7);
        assert_eq!(decoded.usage.credits, 1.25);
        let response = decoded.claude_json("msg", "model", 100, 7, None, &replay_codec());
        assert_eq!(response["usage"]["input_tokens"], 10);
        assert_eq!(response["usage"]["cache_read_input_tokens"], 20);
        assert_eq!(response["usage"]["cache_creation_input_tokens"], 5);
    }

    #[test]
    fn usage_does_not_add_cache_subsets_to_generic_total_input() {
        let mut decoded = DecodedResponse::default();
        decoded
            .push(KiroEvent::MessageMetadata {
                usage: json!({"messageMetadataEvent":{"tokenUsage":{
                    "inputTokens":100,
                    "cacheReadInputTokens":60,
                    "cacheWriteInputTokens":10,
                    "outputTokens":20
                }}}),
            })
            .expect("usage");

        assert_eq!(decoded.usage.input_tokens, 100);
        let response = decoded.claude_json("msg", "model", 100, 20, None, &replay_codec());
        assert_eq!(response["usage"]["input_tokens"], 30);
        assert_eq!(response["usage"]["cache_read_input_tokens"], 60);
        assert_eq!(response["usage"]["cache_creation_input_tokens"], 10);
    }

    #[test]
    fn claude_compaction_block_precedes_the_continued_response() {
        let decoded = DecodedResponse {
            text: "continued answer".into(),
            ..DecodedResponse::default()
        };
        let response = decoded.claude_json(
            "msg",
            "model",
            100,
            10,
            Some("compacted conversation summary"),
            &replay_codec(),
        );

        assert_eq!(response["content"][0]["type"], "compaction");
        assert_eq!(
            response["content"][0]["content"],
            "compacted conversation summary"
        );
        assert_eq!(response["content"][1]["type"], "text");
    }

    #[test]
    fn automatic_compaction_reports_original_and_effective_input_sizes() {
        let decoded = DecodedResponse {
            text: "continued answer".into(),
            usage: UsageInfo {
                // Internal Tool Search/continuation rounds accumulate here;
                // the applied edit must still report the first compacted
                // payload size passed separately below.
                input_tokens: 47_000,
                output_tokens: 100,
                ..UsageInfo::default()
            },
            ..DecodedResponse::default()
        };
        let response = decoded
            .claude_json_with_context_management(
                "msg",
                "source-large",
                100,
                100,
                Some("summary"),
                Some(CompactionIterationUsage {
                    input_tokens: 180_500,
                    output_tokens: 3_500,
                }),
                Some(180_000),
                23_000,
                &ClaudeContextEditStats::default(),
                &replay_codec(),
            )
            .expect("encrypt replay data");

        let edit = &response["context_management"]["applied_edits"][0];
        assert_eq!(edit["type"], "compact_20260112");
        assert_eq!(edit["reason"], "model_mapping_overflow");
        assert_eq!(edit["original_input_tokens"], 180_000);
        assert_eq!(edit["compacted_input_tokens"], 23_000);
        assert_eq!(response["usage"]["input_tokens"], 47_000);
        assert_eq!(response["usage"]["iterations"][0]["type"], "compaction");
        assert_eq!(response["usage"]["iterations"][0]["input_tokens"], 180_500);
        assert_eq!(response["usage"]["iterations"][1]["type"], "message");
    }

    #[test]
    fn custom_openai_tools_roundtrip_their_original_shape() {
        let mut decoded = DecodedResponse::default();
        decoded
            .push(KiroEvent::ToolUse {
                id: "call_1".into(),
                name: "shell_tool".into(),
                input_delta: r#"{"input":"echo ok"}"#.into(),
                stop: true,
            })
            .expect("tool");
        let registry = std::collections::HashMap::from([(
            "shell_tool".into(),
            OpenAiToolIdentity {
                kind: "custom".into(),
                name: "shell.tool".into(),
            },
        )]);
        let response = decoded.openai_json(
            "chat",
            "model",
            1,
            Some(10),
            1,
            crosslane_core::config::ThinkingOutputFormat::Openai,
            &registry,
        );
        let call = &response["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(call["type"], "custom");
        assert_eq!(call["custom"]["name"], "shell.tool");
        assert_eq!(call["custom"]["input"], "echo ok");
    }

    #[test]
    fn custom_openai_tools_recover_common_schema_violations() {
        for (input, expected) in [
            (r#"{"input":"echo ok"}"#, "echo ok"),
            (r#"{"command":"echo ok"}"#, "echo ok"),
            (r#""echo ok""#, "echo ok"),
            (r#"{"left":"a","right":"b"}"#, r#"{"left":"a","right":"b"}"#),
            ("not json", "not json"),
        ] {
            assert_eq!(custom_tool_input(input), expected);
        }
    }

    #[test]
    fn rejects_invalid_tool_inputs_and_cumulative_tool_overflow() {
        assert_eq!(repair_json("{\"ok\":true,}"), json!({"ok":true}));
        assert_eq!(
            repair_json("{\"line\":\"one\ntwo\"}"),
            json!({"line":"one\ntwo"})
        );
        let mut truncated = DecodedResponse::default();
        truncated
            .push(KiroEvent::ToolUse {
                id: "write".into(),
                name: "write_file".into(),
                input_delta: r#"{"path":"/tmp/a""#.into(),
                stop: false,
            })
            .expect("buffer");
        assert!(truncated.finalize_tool_inputs().is_err());

        let mut oversized = DecodedResponse::default();
        assert!(oversized
            .push(KiroEvent::ToolUse {
                id: "large".into(),
                name: "read".into(),
                input_delta: "x".repeat(MAX_CUMULATIVE_TOOL_INPUT_BYTES + 1),
                stop: false,
            })
            .is_err());
    }

    #[test]
    fn finalizes_tool_inputs_without_inferring_side_effects() {
        for name in [
            "read",
            "Write",
            "write_file",
            "execute_command",
            "mcp__files__edit",
            "mcp__relayer__memory_list_editable_atoms",
        ] {
            for stop in [false, true] {
                for input in [
                    "",
                    " \n\t",
                    "{}",
                    r#" {"raw":"preserve me","items":[1,2]} "#,
                ] {
                    let mut decoded = DecodedResponse::default();
                    decoded
                        .push(KiroEvent::ToolUse {
                            id: "call".into(),
                            name: name.into(),
                            input_delta: input.into(),
                            stop,
                        })
                        .expect("buffer");
                    decoded.finalize_tool_inputs().expect("valid tool input");
                    assert_eq!(
                        decoded.tools["call"].input,
                        if input.trim().is_empty() { "{}" } else { input },
                        "name={name}, stop={stop}, input={input:?}"
                    );
                    assert_eq!(decoded.tools["call"].complete, stop);
                }
            }
        }
    }

    #[test]
    fn rejects_invalid_tool_inputs_uniformly_instead_of_guessing_missing_json() {
        for name in [
            "read",
            "write_file",
            "mcp__relayer__memory_list_editable_atoms",
        ] {
            for stop in [false, true] {
                for input in [
                    r#"{"path":"/tmp/a""#,
                    r#"{"path":"/tmp/a"#,
                    r#"{"ok":true,}"#,
                    "{\"line\":\"one\ntwo\"}",
                    "not JSON",
                ] {
                    let mut decoded = DecodedResponse::default();
                    decoded
                        .push(KiroEvent::ToolUse {
                            id: "call".into(),
                            name: name.into(),
                            input_delta: input.into(),
                            stop,
                        })
                        .expect("buffer");
                    assert_eq!(
                        decoded.finalize_tool_inputs().unwrap_err(),
                        format!("tool {name} produced invalid JSON input"),
                        "stop={stop}, input={input:?}"
                    );
                    assert_eq!(decoded.tools["call"].input, input);
                }
            }
        }
    }

    #[test]
    fn buffered_tool_extraction_partitions_in_stable_order() {
        let mut decoded = DecodedResponse::default();
        for (id, name) in [
            ("call_b", "search"),
            ("call_a", "shell"),
            ("call_c", "search"),
        ] {
            decoded.tools.insert(
                id.into(),
                ToolBuffer {
                    id: id.into(),
                    name: name.into(),
                    input: r#"{"command":"true"}"#.into(),
                    complete: true,
                },
            );
        }

        let selected = decoded.take_tool_uses_where(|tool| tool.name == "search");

        assert_eq!(
            selected
                .iter()
                .map(|tool| tool.tool_use_id.as_str())
                .collect::<Vec<_>>(),
            ["call_b", "call_c"]
        );
        assert_eq!(
            decoded.tools.keys().map(String::as_str).collect::<Vec<_>>(),
            ["call_a"]
        );
    }
}
