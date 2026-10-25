//! Local, deterministic secret filtering at the Kiro transport boundary.
//!
//! These rules never make a model or network request. They intentionally operate
//! on model-visible data, not protocol identifiers or authentication headers.
//! Arbitrary unlabeled secrets and opaque binary attachments cannot be classified
//! reliably by text rules; callers must not treat this as a general DLP guarantee.

mod shell;

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, Mutex, OnceLock};

use aho_corasick::{AhoCorasick, AhoCorasickKind};
use base64::Engine;
use regex::Regex;
use serde_json::Value;

use crate::{KiroPayload, KiroUserInputMessage};

pub const REDACTED: &str = "[REDACTED]";

/// Only counts are returned: original secrets are never retained in diagnostics.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RedactionStats {
    pub redacted_values: usize,
    pub redacted_documents: usize,
}

/// An ephemeral mapping shared only by one upstream request and its response.
/// Debug output intentionally never includes values, markers, or their nonce.
#[derive(Clone)]
pub struct RedactionSession {
    state: Arc<Mutex<SessionState>>,
}

struct SessionState {
    nonce: String,
    by_value: HashMap<String, String>,
    by_marker: HashMap<String, Value>,
    source_texts: HashMap<String, String>,
}

impl Default for RedactionSession {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for RedactionSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        formatter
            .debug_struct("RedactionSession")
            .field("mapped_values", &state.by_marker.len())
            .finish()
    }
}

impl RedactionSession {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(SessionState {
                nonce: uuid::Uuid::new_v4().simple().to_string(),
                by_value: HashMap::new(),
                by_marker: HashMap::new(),
                source_texts: HashMap::new(),
            })),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .by_marker
            .is_empty()
    }

    pub fn redact_payload(
        &self,
        payload: &mut KiroPayload,
        known_secrets: &[&str],
    ) -> RedactionStats {
        let remembered = self.known_values();
        let known: Vec<&str> = known_secrets
            .iter()
            .copied()
            .chain(remembered.iter().map(String::as_str))
            .collect();
        let stats = redact_payload_impl(payload, &known, Some(self));
        self.allow_schema_placeholders(payload);
        stats
    }

    pub fn redact_text(&self, text: &str, known_secrets: &[&str]) -> String {
        let remembered = self.known_values();
        let known: Vec<&str> = known_secrets
            .iter()
            .copied()
            .chain(remembered.iter().map(String::as_str))
            .collect();
        redact_text_with_session(text, &known, Some(self))
    }

    /// Restore a model's parsed tool input so JSON escaping and scalar types
    /// remain correct. Exact marker values recover their original scalar type.
    pub fn restore_tool_input(&self, input: &mut Value) {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        restore_value(input, &state);
    }

    /// Conservative allocation bound, including possible shell/JSON escaping.
    /// Check before restoring repeated references to a large sensitive value.
    pub fn tool_input_expansion_bound(&self, input: &Value) -> usize {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        fn text_extra(text: &str, state: &SessionState) -> usize {
            if let Some(original) = state.source_texts.get(text) {
                // Exact source replay bypasses shell escaping. Count its real
                // JSON string encoding instead of rejecting ordinary large
                // signed bodies with an unconditional eightfold multiplier.
                return serde_json::to_string(original).map_or(usize::MAX, |encoded| {
                    encoded
                        .len()
                        .saturating_sub(serde_json::to_string(text).map_or(0, |safe| safe.len()))
                });
            }
            if !text.contains("__CROSSLANE_SECRET_") {
                return 0;
            }
            marker_pattern()
                .find_iter(text)
                .fold(0usize, |size, marker| {
                    let extra = state.by_marker.get(marker.as_str()).map_or(0, |value| {
                        let literal = value
                            .as_str()
                            .map_or_else(|| value.to_string(), str::to_owned);
                        escaped_value_bound(&literal).saturating_sub(marker.as_str().len())
                    });
                    size.saturating_add(extra)
                })
        }
        fn extra(value: &Value, state: &SessionState) -> usize {
            match value {
                Value::String(text) => text_extra(text, state),
                Value::Array(values) => values.iter().fold(0usize, |size, value| {
                    size.saturating_add(extra(value, state))
                }),
                Value::Object(fields) => fields.iter().fold(0usize, |size, (name, value)| {
                    size.saturating_add(text_extra(name, state))
                        .saturating_add(extra(value, state))
                }),
                _ => 0,
            }
        }
        input.to_string().len().saturating_add(extra(input, &state))
    }

    /// One pass only: an original value containing marker-like text is literal,
    /// never recursively expanded into another secret.
    pub fn restore_text(&self, text: &str) -> String {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        restore_string(text, &state)
    }

    /// Carry credentials through a client-visible checkpoint without losing
    /// their classification when the next HTTP request starts a new session.
    /// Capsules are decoded locally and re-masked before any trip to Kiro.
    pub fn checkpoint_text(&self, text: &str) -> String {
        if !text.contains("__CROSSLANE_SECRET_") {
            return text.to_owned();
        }
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        marker_pattern()
            .replace_all(text, |captures: &regex::Captures<'_>| {
                let marker = captures.get(0).expect("marker").as_str();
                state.by_marker.get(marker).map_or_else(
                    || marker.to_owned(),
                    |value| {
                        format!(
                            "__CROSSLANE_CHECKPOINT_{}__",
                            base64::engine::general_purpose::STANDARD.encode(value.to_string())
                        )
                    },
                )
            })
            .into_owned()
    }

    /// Size after checkpoint capsule substitution, without allocating Base64.
    pub fn checkpoint_expansion_bound(&self, text: &str) -> usize {
        if !text.contains("__CROSSLANE_SECRET_") {
            return text.len();
        }
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let mut capsule_sizes = HashMap::new();
        marker_pattern()
            .find_iter(text)
            .fold(text.len(), |size, marker| {
                let Some(value) = state.by_marker.get(marker.as_str()) else {
                    return size;
                };
                let capsule_size = *capsule_sizes.entry(marker.as_str()).or_insert_with(|| {
                    value
                        .to_string()
                        .len()
                        .div_ceil(3)
                        .saturating_mul(4)
                        .saturating_add("__CROSSLANE_CHECKPOINT_".len() + 2)
                });
                size.saturating_sub(marker.as_str().len())
                    .saturating_add(capsule_size)
            })
    }

    fn marker(&self, value: Value) -> String {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let key = value.to_string();
        if let Some(marker) = state.by_value.get(&key) {
            return marker.clone();
        }
        let marker = format!(
            "__CROSSLANE_SECRET_{}_{}__",
            state.nonce,
            state.by_marker.len()
        );
        state.by_value.insert(key, marker.clone());
        state.by_marker.insert(marker.clone(), value);
        marker
    }

    fn known_values(&self) -> Vec<String> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state
            .by_marker
            .values()
            .map(|value| match value {
                Value::String(text) => text.clone(),
                value => value.to_string(),
            })
            .collect()
    }

    fn remember_source_text(&self, original: &str, safe: String) -> String {
        // A checkpoint capsule represents a semantic value, not literal source
        // bytes. Replaying its original encoded spelling would undo restoration.
        if (original.contains("__CROSSLANE_CHECKPOINT_") && checkpoint_pattern().is_match(original))
            || (original.contains("__CROSSLANE_SECRET_") && marker_pattern().is_match(original))
        {
            return safe;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        // JSON normalization can collapse distinct signed source bytes. Keep
        // the first visible document stable; later byte variants become an
        // explicit opaque reference rather than an invisible whitespace tag.
        if state
            .source_texts
            .get(&safe)
            .is_some_and(|previous| previous != original)
        {
            drop(state);
            let marker = self.marker(Value::String(original.to_owned()));
            self.state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .source_texts
                .entry(marker.clone())
                .or_insert_with(|| original.to_owned());
            return marker;
        }
        state.source_texts.insert(safe.clone(), original.to_owned());
        safe
    }

    fn allow_schema_placeholders(&self, payload: &mut KiroPayload) {
        let entries: Vec<_> = {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state
                .by_marker
                .iter()
                .map(|(marker, value)| (marker.clone(), value.clone()))
                .collect()
        };
        if entries.is_empty() {
            return;
        }
        let allow = |user: &mut KiroUserInputMessage| {
            if let Some(context) = &mut user.user_input_message_context {
                for tool in &mut context.tools {
                    if let Some(specification) = tool.specification_mut() {
                        allow_schema_markers(&mut specification.input_schema.json, false, &entries);
                    }
                }
            }
        };
        allow(
            &mut payload
                .conversation_state
                .current_message
                .user_input_message,
        );
        for item in &mut payload.conversation_state.history {
            if let Some(user) = &mut item.user_input_message {
                allow(user);
            }
        }
    }
}

fn escaped_value_bound(text: &str) -> usize {
    text.chars().fold(8usize, |size, ch| {
        size.saturating_add(match ch {
            '\'' => 5,
            '\\' | '"' => 4,
            '$' | '`' | '\n' | '\r' | '\t' => 3,
            ch if ch.is_ascii_control() => 7,
            ch => ch.len_utf8(),
        })
    })
}

fn value_matches_schema_type(value: &Value, schema: &serde_json::Map<String, Value>) -> bool {
    let matches = |kind: &str| match kind {
        "string" => value.is_string(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    };
    match schema.get("type") {
        Some(Value::String(kind)) => matches(kind),
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).any(matches),
        None => {
            for name in ["oneOf", "anyOf", "allOf"] {
                if let Some(Value::Array(branches)) = schema.get(name) {
                    let mut matches = branches
                        .iter()
                        .filter_map(Value::as_object)
                        .map(|branch| value_matches_schema_type(value, branch));
                    return if name == "allOf" {
                        matches.all(|matches| matches)
                    } else {
                        matches.any(|matches| matches)
                    };
                }
            }
            !schema.contains_key("properties") && !schema.contains_key("items")
        }
        _ => false,
    }
}

fn schema_marker_pattern(entries: &[(String, Value)]) -> Option<String> {
    let nonce = entries
        .first()?
        .0
        .strip_prefix("__CROSSLANE_SECRET_")?
        .split('_')
        .next()?;
    Some(format!("__CROSSLANE_SECRET_{nonce}_[0-9]{{1,20}}__"))
}

fn schema_needs_marker_branch(fields: &serde_json::Map<String, Value>) -> bool {
    // Plain strings already accept opaque references. Restricted scalar types
    // and scalar validation keywords need an explicit transport alternative.
    let permits_string = fields.get("type").is_none_or(|kind| match kind {
        Value::String(kind) => kind == "string",
        Value::Array(kinds) => kinds.iter().any(|kind| kind.as_str() == Some("string")),
        _ => false,
    });
    !permits_string
        || [
            "pattern",
            "format",
            "enum",
            "const",
            "minLength",
            "maxLength",
            "minimum",
            "maximum",
            "exclusiveMinimum",
            "exclusiveMaximum",
            "multipleOf",
            "oneOf",
            "anyOf",
            "allOf",
            "not",
            "$ref",
            "if",
            "then",
            "else",
        ]
        .iter()
        .any(|name| fields.contains_key(*name))
}

fn allow_schema_markers(schema: &mut Value, wrap_current: bool, entries: &[(String, Value)]) {
    let Value::Object(fields) = schema else {
        if let Value::Array(values) = schema {
            for value in values {
                allow_schema_markers(value, true, entries);
            }
        }
        return;
    };
    let Some(pattern) = schema_marker_pattern(entries) else {
        return;
    };
    if fields
        .get("anyOf")
        .and_then(Value::as_array)
        .is_some_and(|branches| {
            branches.len() == 2
                && branches[1].get("type").and_then(Value::as_str) == Some("string")
                && branches[1].get("pattern").and_then(Value::as_str) == Some(pattern.as_str())
        })
    {
        let branches = fields
            .get_mut("anyOf")
            .and_then(Value::as_array_mut)
            .unwrap();
        allow_schema_markers(&mut branches[0], false, entries);
        return;
    }
    let mut relax_one_of = false;
    for (name, child) in fields.iter_mut() {
        match name.as_str() {
            "properties" | "patternProperties" | "$defs" | "definitions" | "dependentSchemas" => {
                if let Value::Object(properties) = child {
                    for child in properties.values_mut() {
                        allow_schema_markers(child, true, entries);
                    }
                }
            }
            "default" | "example" | "examples" | "enum" | "const" => {}
            "oneOf" | "anyOf" | "allOf" => {
                if let Value::Array(branches) = child {
                    for branch in branches {
                        // Wrap the composed scalar once, so a marker does not
                        // accidentally match multiple branches of oneOf.
                        allow_schema_markers(branch, false, entries);
                    }
                    relax_one_of |= name == "oneOf" && contains_marker_schema(child, &pattern);
                }
            }
            _ if child.is_object() || child.is_array() => {
                allow_schema_markers(child, true, entries);
            }
            _ => {}
        }
    }
    if relax_one_of {
        if let Some(branches) = fields.remove("oneOf") {
            // Object alternatives may now share an opaque marker branch in a
            // child property. Keep both model-facing alternatives usable; the
            // tool validates the restored value against its original oneOf.
            let relaxed = serde_json::json!({"anyOf":branches});
            match fields.get_mut("allOf") {
                Some(Value::Array(all)) => all.push(relaxed),
                None => {
                    fields.insert("allOf".into(), serde_json::json!([relaxed]));
                }
                _ => {}
            }
        }
    }
    if wrap_current
        && schema_needs_marker_branch(fields)
        && entries
            .iter()
            .any(|(_, value)| value_matches_schema_type(value, fields))
    {
        let original = Value::Object(std::mem::take(fields));
        // The nonce bounds this alternative to this request without copying an
        // unbounded credential enum into every property. Original validation is
        // still applied by the trusted tool to the restored semantic value.
        *schema = serde_json::json!({"anyOf":[original,{"type":"string","pattern":pattern}]});
    }
}

fn contains_marker_schema(value: &Value, pattern: &str) -> bool {
    match value {
        Value::Object(fields) => {
            (fields.get("type").and_then(Value::as_str) == Some("string")
                && fields.get("pattern").and_then(Value::as_str) == Some(pattern))
                || fields
                    .values()
                    .any(|value| contains_marker_schema(value, pattern))
        }
        Value::Array(values) => values
            .iter()
            .any(|value| contains_marker_schema(value, pattern)),
        _ => false,
    }
}

fn marker_pattern() -> &'static Regex {
    static MARKER: OnceLock<Regex> = OnceLock::new();
    regex(&MARKER, r"__CROSSLANE_SECRET_[0-9a-f]{32}_[0-9]+__")
}

fn checkpoint_pattern() -> &'static Regex {
    static CHECKPOINT: OnceLock<Regex> = OnceLock::new();
    regex(
        &CHECKPOINT,
        r"__CROSSLANE_CHECKPOINT_([A-Za-z0-9+/]+={0,2})__",
    )
}

fn checkpoint_value(captures: &regex::Captures<'_>) -> Option<Value> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(captures.get(1)?.as_str())
        .ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    (value.is_string() || value.is_number() || value.is_boolean()).then_some(value)
}

fn checkpoint_secrets(text: &str) -> Vec<String> {
    if !text.contains("__CROSSLANE_CHECKPOINT_") {
        return Vec::new();
    }
    checkpoint_pattern()
        .captures_iter(text)
        .filter_map(|captures| checkpoint_value(&captures))
        .map(|value| match value {
            Value::String(text) => text,
            value => value.to_string(),
        })
        .filter(|value| reusable_secret(value))
        .collect()
}

fn replace_checkpoint_capsules(text: &str, session: &RedactionSession) -> (String, usize) {
    let mut count = 0;
    let safe = checkpoint_pattern()
        .replace_all(text, |captures: &regex::Captures<'_>| {
            checkpoint_value(captures).map_or_else(
                || captures.get(0).expect("checkpoint").as_str().to_owned(),
                |value| {
                    count += 1;
                    session.marker(value)
                },
            )
        })
        .into_owned();
    (safe, count)
}

fn restore_string(text: &str, state: &SessionState) -> String {
    if let Some(original) = recover_exact_source(text, state) {
        return original;
    }
    if !text.contains("__CROSSLANE_SECRET_") {
        return text.to_owned();
    }
    if !marker_pattern()
        .find_iter(text)
        .any(|marker| state.by_marker.contains_key(marker.as_str()))
    {
        return text.to_owned();
    }
    if text.trim_start().starts_with(['{', '[']) {
        if let Ok(mut document) = serde_json::from_str::<Value>(text) {
            if document.is_object() || document.is_array() {
                restore_value(&mut document, state);
                return document.to_string();
            }
        }
    }
    marker_pattern()
        .replace_all(text, |captures: &regex::Captures<'_>| {
            let marker = captures.get(0).expect("marker").as_str();
            match state.by_marker.get(marker) {
                Some(Value::String(value)) => value.clone(),
                Some(value) => value.to_string(),
                None => marker.to_owned(),
            }
        })
        .into_owned()
}

fn recover_exact_source(text: &str, state: &SessionState) -> Option<String> {
    state.source_texts.get(text).cloned()
}

fn restore_value(value: &mut Value, state: &SessionState) {
    match value {
        Value::String(text) => {
            if let Some(original) = state.by_marker.get(text) {
                *value = original.clone();
            } else {
                *text = restore_string(text, state);
            }
        }
        Value::Array(values) => {
            for value in values {
                restore_value(value, state);
            }
        }
        Value::Object(fields) => {
            let original = std::mem::take(fields);
            for (name, mut child) in original {
                restore_value(&mut child, state);
                fields.insert(restore_string(&name, state), child);
            }
        }
        _ => {}
    }
}

/// Redacts a standalone model-visible string, using the same rules as generation.
pub fn redact_text(text: &str, known_secrets: &[&str]) -> String {
    redact_text_with_session(text, known_secrets, None)
}

fn redact_text_with_session(
    text: &str,
    known_secrets: &[&str],
    session: Option<&RedactionSession>,
) -> String {
    let mut discovered = discover_text_secrets(text);
    discovered.sort_unstable();
    discovered.dedup();
    let secrets: Vec<&str> = known_secrets
        .iter()
        .copied()
        .chain(discovered.iter().map(String::as_str))
        .collect();
    let mut redactor = Redactor::new(&secrets);
    redactor.session = session.cloned();
    let mut safe = text.to_owned();
    redactor.text(&mut safe);
    safe
}

/// Covers every generation path, including translated system/history messages,
/// tool continuations and model-based compaction requests. Authentication and
/// routing fields are deliberately outside this traversal.
pub fn redact_kiro_payload(payload: &mut KiroPayload, known_secrets: &[&str]) -> RedactionStats {
    redact_payload_impl(payload, known_secrets, None)
}

fn redact_payload_impl(
    payload: &mut KiroPayload,
    known_secrets: &[&str],
    session: Option<&RedactionSession>,
) -> RedactionStats {
    // Discover before any mutation: a labeled password in history must also be
    // scrubbed when repeated without a label in another message or tool result.
    let mut collector = Redactor::new(&[]);
    collector.collecting = true;
    collector.payload(payload);
    collector.discovered.sort_unstable();
    collector.discovered.dedup();
    let secrets: Vec<&str> = known_secrets
        .iter()
        .copied()
        .chain(collector.discovered.iter().map(String::as_str))
        .collect();
    let mut redactor = Redactor::new(&secrets);
    redactor.session = session.cloned();
    redactor.payload(payload);
    redactor.stats
}

struct Redactor<'a> {
    known_secrets: &'a [&'a str],
    matcher: Option<AhoCorasick>,
    stats: RedactionStats,
    collecting: bool,
    discovered: Vec<String>,
    session: Option<RedactionSession>,
    shell_commands: Vec<String>,
}

impl<'a> Redactor<'a> {
    fn new(known_secrets: &'a [&'a str]) -> Self {
        Self {
            known_secrets,
            matcher: {
                let patterns: Vec<&str> = known_secrets
                    .iter()
                    .copied()
                    .filter(|value| reusable_secret(value))
                    .collect();
                if patterns.is_empty() {
                    None
                } else {
                    // Default DFA selection depends on pattern count, not
                    // total length; a single large credential can make DFA
                    // construction prohibitively expensive. Keep it sparse.
                    AhoCorasick::builder()
                        .kind(Some(AhoCorasickKind::ContiguousNFA))
                        .build(patterns)
                        .ok()
                }
            },
            stats: RedactionStats::default(),
            collecting: false,
            discovered: Vec::new(),
            session: None,
            shell_commands: Vec::new(),
        }
    }

    fn payload(&mut self, payload: &mut KiroPayload) {
        self.user(
            &mut payload
                .conversation_state
                .current_message
                .user_input_message,
        );
        for item in &mut payload.conversation_state.history {
            if let Some(user) = &mut item.user_input_message {
                self.user(user);
            }
            if let Some(assistant) = &mut item.assistant_response_message {
                self.text(&mut assistant.content);
                for tool in &mut assistant.tool_uses {
                    let previous_shell_commands = std::mem::replace(
                        &mut self.shell_commands,
                        shell::command_sources(&tool.name, &tool.input)
                            .into_iter()
                            .map(str::to_owned)
                            .collect(),
                    );
                    if self.collecting {
                        self.discovered
                            .extend(shell::discover_shell_secrets(&tool.name, &tool.input));
                    }
                    let original = self.session.as_ref().map(|_| tool.input.clone());
                    self.data(&mut tool.input, false);
                    if let (Some(session), Some(original)) = (&self.session, original) {
                        shell::normalize_shell_markers(
                            &tool.name,
                            &original,
                            &mut tool.input,
                            session,
                        );
                    }
                    self.shell_commands = previous_shell_commands;
                }
            }
        }
        if let Some(fields) = &mut payload.additional_model_request_fields {
            self.additional_model_fields(fields);
        }
    }

    fn additional_model_fields(&mut self, fields: &mut Value) {
        // These strictly shaped native branches are protocol controls rebuilt
        // from client intent. Known secrets that happen to equal "enabled" or
        // "high" must not corrupt enum values or their branch names.
        let mut controls = Vec::new();
        if let Value::Object(values) = fields {
            for name in ["thinking", "reasoning", "output_config"] {
                if values
                    .get(name)
                    .is_some_and(|value| native_model_control(name, value))
                {
                    controls.push((
                        name.to_owned(),
                        values.remove(name).expect("native control"),
                    ));
                }
            }
        }
        self.data(fields, false);
        if let Value::Object(values) = fields {
            values.extend(controls);
        }
    }

    fn text(&mut self, value: &mut String) {
        let shell_source = self.shell_commands.iter().any(|source| source == value);
        if !self.collecting {
            if let Some(session) = self
                .session
                .as_ref()
                .filter(|_| value.contains("__CROSSLANE_CHECKPOINT_"))
            {
                let (safe, count) = replace_checkpoint_capsules(value, session);
                if count > 0 {
                    *value = safe;
                    self.stats.redacted_values += count;
                }
            }
        }
        // Tool output often contains a complete JSON document. Match decoded
        // values rather than their textual escape sequences, so a secret such
        // as a\"b restores as the actual quote-containing credential.
        if value.trim_start().starts_with(['{', '[']) {
            if let Ok(mut document) = serde_json::from_str::<Value>(value) {
                if document.is_object() || document.is_array() {
                    let before = self.stats.redacted_values;
                    self.data(&mut document, false);
                    if !self.collecting && self.stats.redacted_values > before {
                        let safe = document.to_string();
                        *value = self.session.as_ref().map_or_else(
                            || safe.clone(),
                            |session| session.remember_source_text(value, safe.clone()),
                        );
                    }
                    return;
                }
            }
        }
        if self.collecting {
            self.discovered
                .extend(discover_text_secrets_with_context(value, shell_source));
            return;
        }
        let (safe, count) = redact_text_impl_with_context(
            value,
            self.known_secrets,
            self.matcher.as_ref(),
            self.session.as_ref(),
            shell_source,
        );
        if count > 0 {
            *value = self.session.as_ref().map_or_else(
                || safe.clone(),
                |session| session.remember_source_text(value, safe.clone()),
            );
            self.stats.redacted_values += count;
        }
    }

    fn user(&mut self, user: &mut KiroUserInputMessage) {
        self.text(&mut user.content);
        for document in &mut user.documents {
            self.text(&mut document.name);
            if matches!(
                document.format.to_ascii_lowercase().as_str(),
                "txt" | "md" | "csv" | "html"
            ) {
                if let Ok(bytes) =
                    base64::engine::general_purpose::STANDARD.decode(&document.source.bytes)
                {
                    if let Ok(mut text) = String::from_utf8(bytes) {
                        let before = self.stats.redacted_values;
                        self.text(&mut text);
                        if self.stats.redacted_values > before {
                            document.source.bytes =
                                base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
                            self.stats.redacted_documents += 1;
                        }
                    }
                }
            }
        }
        if let Some(context) = &mut user.user_input_message_context {
            for result in &mut context.tool_results {
                for part in &mut result.content {
                    self.text(&mut part.text);
                }
            }
            for tool in &mut context.tools {
                if let Some(spec) = tool.specification_mut() {
                    self.text(&mut spec.description);
                    self.schema(&mut spec.input_schema.json, false);
                }
            }
            if let Some(env) = &mut context.env_state {
                if let Some(value) = &mut env.current_working_directory {
                    self.text(value);
                }
                if let Some(value) = &mut env.operating_system {
                    self.text(value);
                }
            }
        }
    }

    fn data(&mut self, value: &mut Value, sensitive: bool) {
        if let Value::String(text) = value {
            if self.collecting {
                self.discovered.extend(checkpoint_secrets(text));
            } else if let Some(session) = self
                .session
                .as_ref()
                .filter(|_| text.contains("__CROSSLANE_CHECKPOINT_"))
            {
                let (safe, count) = replace_checkpoint_capsules(text, session);
                if count > 0 {
                    *text = safe;
                    self.stats.redacted_values += count;
                }
            }
        }
        match value {
            Value::Object(fields) => {
                // Input data dictionaries can use a secret as the key. Schema
                // declarations take their separate, structure-preserving path.
                let original = std::mem::take(fields);
                for (mut name, mut child) in original {
                    let secret_field = sensitive || sensitive_name(&name);
                    self.text(&mut name);
                    self.data(&mut child, secret_field);
                    if fields.contains_key(&name) {
                        let base = name;
                        let mut suffix = 2usize;
                        name = format!("{base}_{suffix}");
                        while fields.contains_key(&name) {
                            suffix += 1;
                            name = format!("{base}_{suffix}");
                        }
                    }
                    fields.insert(name, child);
                }
            }
            Value::Array(values) => {
                for child in values {
                    self.data(child, sensitive);
                }
            }
            Value::Null => {}
            Value::String(text)
                if sensitive
                    && (symbolic_sensitive_value(text) || marker_pattern().is_match(text)) =>
            {
                let ranges = exclude_references(text, std::iter::once(0..text.len()).collect())
                    .into_iter()
                    .filter(|range| {
                        !matches!(
                            text[range.clone()].trim().to_ascii_lowercase().as_str(),
                            "bearer" | "basic"
                        )
                    })
                    .collect::<Vec<_>>();
                if self.collecting {
                    for range in ranges {
                        let literal = &text[range];
                        if reusable_secret(literal) {
                            self.discovered.push(literal.to_owned());
                        }
                    }
                } else {
                    let (safe, count) = replace_ranges(text, ranges, self.session.as_ref());
                    if count > 0 {
                        *text = self.session.as_ref().map_or_else(
                            || safe.clone(),
                            |session| session.remember_source_text(text, safe.clone()),
                        );
                        self.stats.redacted_values += count;
                    }
                }
            }
            Value::String(text) if !sensitive => self.text(text),
            _ if sensitive && self.collecting => {
                if let Value::String(text) = value {
                    if reusable_secret(text) {
                        self.discovered.push(text.clone());
                    }
                    self.discovered.extend(discover_text_secrets(text));
                } else if value.is_number() {
                    let text = value.to_string();
                    if reusable_secret(&text) {
                        self.discovered.push(text);
                    }
                }
            }
            _ if sensitive
                && value.as_str() != Some(REDACTED)
                && !value.as_str().is_some_and(|text| {
                    marker_pattern()
                        .find(text)
                        .is_some_and(|matched| matched.as_str() == text)
                }) =>
            {
                let marker = self.session.as_ref().map_or_else(
                    || REDACTED.to_owned(),
                    |session| session.marker(value.clone()),
                );
                *value = Value::String(marker);
                self.stats.redacted_values += 1;
            }
            _ => {}
        }
    }

    // JSON Schema property names, types, required names and structural keywords
    // must survive. Only human text and data-bearing default/example/const/enum
    // values are eligible for masking.
    fn schema(&mut self, value: &mut Value, sensitive_property: bool) {
        match value {
            Value::Object(fields) => {
                for (name, child) in fields {
                    match name.as_str() {
                        "properties" | "patternProperties" | "$defs" | "definitions"
                        | "dependentSchemas" => {
                            if let Value::Object(properties) = child {
                                for (property, schema) in properties {
                                    self.schema(
                                        schema,
                                        sensitive_property || sensitive_name(property),
                                    );
                                }
                            }
                        }
                        "default" | "example" | "examples" | "enum" | "const" => {
                            self.data(child, sensitive_property)
                        }
                        "description" | "title" | "$comment" => {
                            if let Value::String(text) = child {
                                self.text(text);
                            }
                        }
                        _ => {
                            if child.is_object() || child.is_array() {
                                self.schema(child, sensitive_property);
                            }
                        }
                    }
                }
            }
            Value::Array(values) => {
                for child in values {
                    self.schema(child, sensitive_property);
                }
            }
            _ => {}
        }
    }
}

fn native_model_control(name: &str, value: &Value) -> bool {
    let Value::Object(fields) = value else {
        return false;
    };
    match name {
        "thinking" => {
            fields
                .keys()
                .all(|name| matches!(name.as_str(), "type" | "display" | "budget_tokens"))
                && matches!(
                    fields.get("type").and_then(Value::as_str),
                    Some("enabled" | "disabled" | "adaptive")
                )
                && fields.get("display").is_none_or(|display| {
                    matches!(display.as_str(), Some("summarized" | "omitted"))
                })
                && fields.get("budget_tokens").is_none_or(|budget| {
                    budget
                        .as_u64()
                        .is_some_and(|budget| u32::try_from(budget).is_ok())
                })
        }
        "reasoning" | "output_config" => {
            fields.len() == 1
                && matches!(
                    fields.get("effort").and_then(Value::as_str),
                    Some("none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max")
                )
        }
        _ => false,
    }
}

fn sensitive_name(name: &str) -> bool {
    let trimmed = name.trim_matches(['"', '\'', '\\', '-']);
    if ["密码", "口令", "密钥", "令牌"]
        .iter()
        .any(|label| trimmed.ends_with(label))
    {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    let compact: String = lower
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect();
    matches!(
        compact.as_str(),
        "key"
            | "ak"
            | "sk"
            | "sts"
            | "secret"
            | "token"
            | "password"
            | "passwd"
            | "pwd"
            | "authorization"
            | "proxyauthorization"
            | "cookie"
            | "setcookie"
            | "credentials"
            | "credential"
            | "connectionstring"
            | "sig"
            | "signature"
            | "qsignature"
            | "xamzsignature"
    ) || [
        "apikey",
        "secretid",
        "accesskeyid",
        "accesskeysecret",
        "secretaccesskey",
        "securitytoken",
        "privatekeyid",
        "sharedaccesssignature",
        "accountkey",
        "accesskey",
        "secretkey",
        "privatekey",
        "clientsecret",
        "accesstoken",
        "refreshtoken",
        "sessiontoken",
        "authtoken",
        "bearertoken",
        "idtoken",
        "password",
        "passwd",
        "credential",
        "credentials",
        "connectionstring",
    ]
    .iter()
    .any(|suffix| compact.ends_with(suffix))
        || lower.ends_with("_token")
        || lower.ends_with("-token")
        || lower.ends_with(".token")
        || lower.ends_with("_key")
        || lower.ends_with("-key")
        || lower.ends_with(".key")
        || lower.ends_with("_secret")
        || lower.ends_with("-secret")
        || lower.ends_with(".secret")
        || ["_ak", "-ak", ".ak", "_sk", "-sk", ".sk"]
            .iter()
            .any(|suffix| lower.ends_with(suffix))
        || trimmed.ends_with("Secret")
        || trimmed.ends_with("Token")
        || trimmed.ends_with("Key")
}

fn regex(cell: &'static OnceLock<Regex>, expression: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(expression).expect("static secret-redaction rule"))
}

fn reusable_secret(secret: &str) -> bool {
    secret.chars().count() >= 4
        && !REDACTED.contains(secret)
        && !matches!(secret, "true" | "false" | "null")
        && !symbolic_sensitive_value(secret)
        && !marker_pattern()
            .find(secret)
            .is_some_and(|matched| matched.as_str() == secret)
}

fn symbolic_pattern() -> &'static Regex {
    static SYMBOLIC: OnceLock<Regex> = OnceLock::new();
    regex(
        &SYMBOLIC,
        r#"\$\{[A-Za-z_][A-Za-z0-9_]*(?::[-+?=][^{}\r\n]*)?\}|\$(?:env:)?[A-Za-z_][A-Za-z0-9_]*|%[A-Za-z_][A-Za-z0-9_]*%|process\.env(?:\.[A-Za-z_][A-Za-z0-9_]*|\[\s*["'][A-Za-z_][A-Za-z0-9_]*["']\s*\])|os\.(?:getenv|environ\.get)\(\s*["'][A-Za-z_][A-Za-z0-9_]*["']\s*\)|os\.environ\[\s*["'][A-Za-z_][A-Za-z0-9_]*["']\s*\]"#,
    )
}

fn symbolic_sensitive_value(value: &str) -> bool {
    if !may_contain_symbolic_reference(value) {
        return false;
    }
    let trimmed = value.trim();
    let candidate = trimmed
        .split_once(char::is_whitespace)
        .filter(|(scheme, _)| {
            scheme.eq_ignore_ascii_case("bearer") || scheme.eq_ignore_ascii_case("basic")
        })
        .map_or(trimmed, |(_, reference)| reference.trim());
    symbolic_pattern()
        .find(candidate)
        .is_some_and(|reference| reference.start() == 0 && reference.end() == candidate.len())
}

fn may_contain_symbolic_reference(text: &str) -> bool {
    text.contains('$') || text.contains('%') || text.contains("process.env") || text.contains("os.")
}

fn exclude_references(text: &str, ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    if ranges.is_empty() {
        return ranges;
    }
    let protected = protected_reference_ranges(text);
    let mut safe = Vec::new();
    for range in ranges {
        let mut offset = range.start;
        for reference in &protected {
            if reference.end <= offset {
                continue;
            }
            if reference.start >= range.end {
                break;
            }
            if reference.start > offset {
                safe.push(offset..reference.start.min(range.end));
            }
            offset = offset.max(reference.end);
            if offset >= range.end {
                break;
            }
        }
        if offset < range.end {
            safe.push(offset..range.end);
        }
    }
    safe
}

fn protected_reference_ranges(text: &str) -> Vec<Range<usize>> {
    let mut protected = Vec::new();
    if may_contain_symbolic_reference(text) {
        for reference in symbolic_pattern().find_iter(text) {
            // A shell-escaped dollar is literal credential data. Treating it as an
            // environment reference leaks the suffix and splits a quoted secret.
            if reference.as_str().starts_with('$')
                && text[..reference.start()]
                    .bytes()
                    .rev()
                    .take_while(|byte| *byte == b'\\')
                    .count()
                    % 2
                    == 1
            {
                continue;
            }
            let expression = reference.as_str();
            let expansion = [":-", ":+", ":?", ":="]
                .iter()
                .filter_map(|operator| expression.find(operator))
                .min();
            if expression.starts_with("${") {
                if let Some(operator) = expansion {
                    protected.push(reference.start()..reference.start() + operator + 2);
                    protected.push(reference.end() - 1..reference.end());
                    continue;
                }
            }
            protected.push(reference.range());
        }
    }
    if text.contains("__CROSSLANE_SECRET_") {
        protected.extend(
            marker_pattern()
                .find_iter(text)
                .map(|matched| matched.range()),
        );
    }
    protected.sort_unstable_by_key(|range| range.start);
    protected
}

fn discover_text_secrets(text: &str) -> Vec<String> {
    discover_text_secrets_with_context(text, false)
}

fn discover_text_secrets_with_context(text: &str, shell_source: bool) -> Vec<String> {
    let mut secrets = checkpoint_secrets(text);
    secrets.extend(
        secret_ranges_with_context(text, &[], None, shell_source)
            .into_iter()
            .filter_map(|range| {
                let secret = &text[range];
                reusable_secret(secret).then(|| secret.to_owned())
            })
            .collect::<Vec<_>>(),
    );
    secrets
}

fn secret_ranges_shell(
    text: &str,
    known_secrets: &[&str],
    matcher: Option<&AhoCorasick>,
) -> Vec<Range<usize>> {
    secret_ranges_with_context(text, known_secrets, matcher, true)
}

fn secret_ranges_with_context(
    text: &str,
    known_secrets: &[&str],
    matcher: Option<&AhoCorasick>,
    shell_source: bool,
) -> Vec<Range<usize>> {
    let mut ranges = Vec::<Range<usize>>::new();
    if text.contains("__CROSSLANE_CHECKPOINT_") {
        ranges.extend(
            checkpoint_pattern()
                .captures_iter(text)
                .filter(|captures| checkpoint_value(captures).is_some())
                .map(|captures| captures.get(0).expect("checkpoint").range()),
        );
    }
    let mut known_ranges = Vec::new();
    if let Some(matcher) = matcher {
        // Overlaps are merged below, so a short known token cannot hide the
        // suffix of a longer overlapping credential. Bound pathological repeated
        // match output by masking the entire field, never by dropping secrets.
        for (index, matched) in matcher.find_overlapping_iter(text).enumerate() {
            if index > text.len().saturating_add(1024) {
                return exclude_references(text, std::iter::once(0..text.len()).collect());
            }
            known_ranges.push(matched.range());
        }
    } else {
        // Building can fail for oversized untrusted pattern sets. Retain a
        // correct literal fallback rather than panicking or failing open.
        for secret in known_secrets
            .iter()
            .copied()
            .filter(|value| reusable_secret(value))
        {
            known_ranges.extend(
                text.match_indices(secret)
                    .map(|(start, _)| start..start + secret.len()),
            );
        }
    }
    static PEM: OnceLock<Regex> = OnceLock::new();
    static TOKEN: OnceLock<Regex> = OnceLock::new();
    static CLOUD_ACCESS_KEY: OnceLock<Regex> = OnceLock::new();
    static JWT: OnceLock<Regex> = OnceLock::new();
    static AUTH: OnceLock<Regex> = OnceLock::new();
    static USERINFO: OnceLock<Regex> = OnceLock::new();
    static FLAG: OnceLock<Regex> = OnceLock::new();
    static FALLBACK: OnceLock<Regex> = OnceLock::new();
    static TOKEN_PREFIXES: OnceLock<AhoCorasick> = OnceLock::new();
    static AUTH_PREFIXES: OnceLock<AhoCorasick> = OnceLock::new();
    // Cheap literal prefilters avoid running full regex automata over large
    // harmless history fields. Keep Unicode K/S case folds accepted by (?i).
    // These fixed short patterns have a bounded DFA; unlike dynamic secret
    // sets, they cannot grow with input size or contain megabyte-long values.
    let token_candidate = TOKEN_PREFIXES
        .get_or_init(|| {
            AhoCorasick::builder()
                .ascii_case_insensitive(true)
                .kind(Some(AhoCorasickKind::DFA))
                .build([
                    "ksk_",
                    "sk-",
                    "sk_live_",
                    "sk_test_",
                    "aiza",
                    "ghp_",
                    "gho_",
                    "ghu_",
                    "ghs_",
                    "ghr_",
                    "github_pat_",
                    "glpat-",
                    "xoxb-",
                    "xoxa-",
                    "xoxp-",
                    "xoxr-",
                    "xoxs-",
                    "akia",
                    "asia",
                ])
                .expect("static credential prefixes")
        })
        .is_match(text)
        || text.contains('\u{212a}')
        || text.contains('\u{17f}');
    for pattern in [
        text.contains("-----BEGIN ").then(|| regex(
            &PEM,
            r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
        )),
        token_candidate.then(|| regex(
            &TOKEN,
            r#"(?i)(?:ksk_[^\s"'<>\x60,;()\[\]{}]+|sk-(?:(?:ant|proj|svcacct)-)?[a-z0-9_-]{8,}|sk_(?:live|test)_[a-z0-9]{8,}|AIza[a-z0-9_-]{20,}|gh[pousr]_[a-z0-9_]{8,}|github_pat_[a-z0-9_]{8,}|glpat-[a-z0-9_-]{8,}|xox[baprs]-[a-z0-9-]{8,}|(?:AKIA|ASIA)[A-Z0-9]{16})"#,
        )),
        text.contains("eyJ").then(|| regex(&JWT, r"eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]*")),
        // Tencent SecretId, Alibaba permanent/STS IDs, and AKLT IDs used by
        // Volcengine/Kingsoft. Keep suffix lengths open for format evolution;
        // opaque SKs are identified by their credential field names instead.
        ["AKID", "LTAI", "STS.", "AKLT"].iter().any(|prefix| text.contains(prefix)).then(|| regex(
            &CLOUD_ACCESS_KEY,
            r"(?:AKID[A-Za-z0-9]{16,}|LTAI[A-Za-z0-9]{12,}|STS\.[A-Za-z0-9_-]{12,}|AKLT[A-Za-z0-9_-]{16,})",
        )),
    ].into_iter().flatten() {
        ranges.extend(
            pattern
                .find_iter(text)
                .filter(|matched| {
                    matched.start() == 0
                        || !text[..matched.start()]
                            .chars()
                            .next_back()
                            .is_some_and(|ch| ch.is_ascii_alphanumeric())
                })
                .map(|matched| matched.range()),
        );
    }
    if AUTH_PREFIXES
        .get_or_init(|| {
            AhoCorasick::builder()
                .ascii_case_insensitive(true)
                .kind(Some(AhoCorasickKind::DFA))
                .build(["bearer", "basic"])
                .expect("static authentication prefixes")
        })
        .is_match(text)
        || text.contains('\u{17f}')
    {
        for captures in regex(
            &AUTH,
            r#"(?i)\b(?P<scheme>bearer|basic)[ \t]+(?P<secret>[^\s"'<>\x60,;()\[\]{}]+)"#,
        )
        .captures_iter(text)
        {
            if let Some(secret) = captures.name("secret") {
                let basic = captures
                    .name("scheme")
                    .is_some_and(|scheme| scheme.as_str().eq_ignore_ascii_case("basic"));
                if !basic
                    || base64::engine::general_purpose::STANDARD
                        .decode(secret.as_str())
                        .is_ok_and(|decoded| decoded.contains(&b':'))
                {
                    ranges.push(secret.range());
                }
            }
        }
    }
    // Keep the scheme and host usable while removing all URL userinfo (which
    // may contain both a username and a password or a token-only credential).
    if text.contains("://") {
        for captures in
            regex(&USERINFO, r"(?i)\b[a-z][a-z0-9+.-]*://(?P<secret>[^/\s]+)@").captures_iter(text)
        {
            if let Some(secret) = captures.name("secret") {
                ranges.push(secret.range());
            }
        }
    }
    // Long opaque keys and Base64 data usually contain no assignment delimiter.
    // Avoid running the bounded identifier matcher at every byte in that case.
    for assignment in assignments(text) {
        let name = assignment.name;
        if sensitive_name(name) {
            let prefix = &text[..assignment.start];
            let is_flag = prefix.ends_with("--");
            let is_query = prefix.ends_with('?') || prefix.ends_with('&');
            let line_prefix = prefix.rsplit('\n').next().unwrap_or(prefix).trim_end();
            let quoted_key = name.starts_with(['"', '\'', '\\']);
            let yaml_block = text[assignment.value_start..].starts_with(['|', '>'])
                && line_prefix.trim().trim_start_matches('-').trim().is_empty();
            let whole_line = !is_flag
                && !is_query
                && (!quoted_key || yaml_block)
                && !line_prefix.ends_with(['{', ',']);
            let context = if is_query {
                ValueContext::Query
            } else if is_flag || shell_source {
                ValueContext::Shell
            } else if whole_line {
                ValueContext::Line
            } else {
                ValueContext::Json
            };
            if let Some(range) = value_range(text, assignment.value_start, context, name) {
                ranges.push(range);
            }
        }
    }
    if text.contains("--") {
        for captures in
            regex(&FLAG, r"--(?P<name>[A-Za-z_][A-Za-z0-9_.-]{0,127})[ \t]+").captures_iter(text)
        {
            let name = captures.name("name").expect("flag name");
            if sensitive_name(name.as_str()) {
                if let Some(range) = value_range(
                    text,
                    captures.get(0).expect("flag").end(),
                    ValueContext::Shell,
                    name.as_str(),
                ) {
                    ranges.push(range);
                }
            }
        }
    }
    if text.contains("${") {
        for captures in regex(
            &FALLBACK,
            r"\$\{(?P<name>[A-Za-z_][A-Za-z0-9_]*):[-+?=](?P<secret>[^{}\r\n]*)\}",
        )
        .captures_iter(text)
        {
            if sensitive_name(captures.name("name").expect("variable name").as_str()) {
                ranges.push(captures.name("secret").expect("fallback value").range());
            }
        }
    }
    ranges.retain(|range| range.start < range.end && &text[range.clone()] != REDACTED);
    let mut safe = exclude_references(text, ranges);
    if !known_ranges.is_empty() {
        let protected = protected_reference_ranges(text);
        safe.extend(known_ranges.into_iter().filter(|range| {
            let mut covered = range.start;
            for reference in &protected {
                if reference.end <= covered {
                    continue;
                }
                if reference.start > covered {
                    break;
                }
                covered = covered.max(reference.end);
                if covered >= range.end {
                    return false;
                }
            }
            true
        }));
    }
    safe
}

struct Assignment<'a> {
    name: &'a str,
    start: usize,
    value_start: usize,
}

fn assignments(text: &str) -> impl Iterator<Item = Assignment<'_>> {
    static NAME: OnceLock<Regex> = OnceLock::new();
    let name_pattern = regex(
        &NAME,
        r#"(?P<name>(?:\\?["'])?[\p{L}_][\p{L}\p{N}_.-]{0,127}(?:\\?["'])?)[ \t]*(?:=|:|：|＝)\z"#,
    );
    // Individual char searchers use UTF-8 byte prefilters; a heterogeneous
    // Unicode char-array search walks every character in debug builds.
    let mut delimiters = ['=', ':', '：', '＝'].map(|ch| text.match_indices(ch).peekable());
    std::iter::from_fn(move || {
        let next = delimiters
            .iter_mut()
            .enumerate()
            .filter_map(|(index, iter)| iter.peek().map(|(offset, _)| (index, *offset)))
            .min_by_key(|(_, offset)| *offset)
            .map(|(index, _)| index)?;
        delimiters[next].next()
    })
    .filter_map(move |(delimiter_start, delimiter)| {
        let name_end = text[..delimiter_start].trim_end_matches([' ', '\t']).len();
        // A name has at most 128 Unicode characters plus escaped quotes;
        // horizontal spacing is accounted for without scanning Unicode
        // captures over unrelated multi-megabyte history text.
        let window_start = text[..name_end]
            .char_indices()
            .rev()
            .nth(131)
            .map_or(0, |(offset, _)| offset);
        let mut probe = text[window_start..name_end].to_owned();
        probe.push_str(delimiter);
        let captures = name_pattern.captures(&probe)?;
        let name = captures.name("name")?;
        let value_offset = delimiter_start + delimiter.len();
        let value_start = value_offset + text[value_offset..].len()
            - text[value_offset..].trim_start_matches([' ', '\t']).len();
        Some(Assignment {
            name: &text[window_start + name.start()..window_start + name.end()],
            start: window_start + captures.get(0)?.start(),
            value_start,
        })
    })
}

fn redact_text_impl_with_context(
    text: &str,
    known_secrets: &[&str],
    matcher: Option<&AhoCorasick>,
    session: Option<&RedactionSession>,
    shell_source: bool,
) -> (String, usize) {
    let ranges = secret_ranges_with_context(text, known_secrets, matcher, shell_source);
    replace_ranges(text, ranges, session)
}

fn replace_ranges(
    text: &str,
    mut ranges: Vec<Range<usize>>,
    session: Option<&RedactionSession>,
) -> (String, usize) {
    ranges.retain(|range| range.start < range.end);
    if ranges.is_empty() {
        return (text.to_owned(), 0);
    }
    ranges.sort_unstable_by_key(|range| (range.start, range.end));
    let mut merged = Vec::<Range<usize>>::new();
    for range in ranges {
        if let Some(previous) = merged.last_mut() {
            if range.start < previous.end {
                previous.end = previous.end.max(range.end);
                continue;
            }
        }
        merged.push(range);
    }
    let mut safe = String::with_capacity(text.len());
    let mut offset = 0;
    for range in &merged {
        safe.push_str(&text[offset..range.start]);
        if let Some(session) = session {
            safe.push_str(&session.marker(Value::String(text[range.clone()].to_owned())));
        } else {
            safe.push_str(REDACTED);
        }
        offset = range.end;
    }
    safe.push_str(&text[offset..]);
    (safe, merged.len())
}

#[derive(Clone, Copy)]
enum ValueContext {
    Line,
    Json,
    Shell,
    Query,
}

impl ValueContext {
    fn ends_at(self, ch: char) -> bool {
        match self {
            Self::Line => matches!(ch, '\r' | '\n'),
            Self::Json => matches!(ch, '\r' | '\n' | ',' | '}' | ']' | '"' | '\''),
            Self::Shell => ch.is_whitespace() || matches!(ch, ';' | '&' | '|' | '"' | '\'' | '`'),
            Self::Query => ch.is_whitespace() || matches!(ch, '&' | '#' | '"' | '\'' | '`'),
        }
    }
}

fn value_range(
    text: &str,
    start: usize,
    context: ValueContext,
    name: &str,
) -> Option<Range<usize>> {
    let tail = text.get(start..)?;
    if tail.is_empty() {
        return None;
    }
    // A variable or command substitution is a symbolic reference, not its
    // runtime value. Stop at its end so a shell command following it survives.
    if let Some(reference) = symbolic_pattern()
        .find(tail)
        .filter(|found| found.start() == 0)
    {
        return Some(start..start + reference.end());
    }
    if let Some(rest) = tail.strip_prefix(REDACTED) {
        if rest.is_empty() || rest.starts_with(|ch| context.ends_at(ch)) || rest.starts_with(" #") {
            return None;
        }
    }
    // JSON nested in a quoted command often has escaped quote delimiters.
    for delimiter in ["\"\"\"", "'''", "\\\"", "\\'", "$'", "\"", "'"] {
        if let Some(inner) = tail.strip_prefix(delimiter) {
            let content_start = start + delimiter.len();
            let closing = if delimiter == "$'" { "'" } else { delimiter };
            let mut offset = 0;
            while offset < inner.len() {
                if inner[offset..].starts_with(closing) {
                    return Some(content_start..content_start + offset);
                }
                let character = inner[offset..].chars().next()?;
                if (delimiter.len() == 1 || delimiter == "$'") && character == '\\' {
                    offset += character.len_utf8();
                    if let Some(next) = inner[offset..].chars().next() {
                        offset += next.len_utf8();
                    }
                } else {
                    offset += character.len_utf8();
                }
            }
            return Some(content_start..text.len());
        }
    }
    // Preserve an authentication scheme, but redact its entire credential.
    let lower = tail.to_ascii_lowercase();
    if sensitive_name(name) && (lower.starts_with("bearer ") || lower.starts_with("basic ")) {
        let scheme_end = tail.find(char::is_whitespace)?;
        let credential_start =
            scheme_end + tail[scheme_end..].len() - tail[scheme_end..].trim_start().len();
        return value_range(text, start + credential_start, ValueContext::Shell, "token");
    }
    if matches!(
        name.trim_matches(['"', '\'', '\\'])
            .to_ascii_lowercase()
            .as_str(),
        "cookie" | "set-cookie" | "set_cookie"
    ) {
        let end = tail.find(['\r', '\n']).unwrap_or(tail.len());
        return Some(start..start + tail[..end].trim_end().len());
    }
    if matches!(context, ValueContext::Line) {
        let line_end = tail.find('\n').unwrap_or(tail.len());
        let line_value = tail[..line_end].trim_end();
        if line_value
            .chars()
            .next()
            .is_some_and(|ch| matches!(ch, '|' | '>'))
        {
            // YAML block scalars are a single value even though the content is
            // on following indented lines. Stop at the next sibling field.
            let source_line = text[..start].rsplit('\n').next().unwrap_or("");
            let indentation = source_line
                .bytes()
                .take_while(|byte| matches!(byte, b' ' | b'\t'))
                .count();
            let mut offset = start + line_end;
            let mut block_end = offset;
            while offset < text.len() {
                let next_start = offset + 1;
                let next_end = text[next_start..]
                    .find('\n')
                    .map_or(text.len(), |end| next_start + end);
                let line = &text[next_start..next_end];
                let next_indentation = line
                    .bytes()
                    .take_while(|byte| matches!(byte, b' ' | b'\t'))
                    .count();
                if !line.trim().is_empty() && next_indentation <= indentation {
                    break;
                }
                block_end = next_end;
                offset = next_end;
            }
            return Some(start..block_end);
        }
        // Unquoted .env/YAML/TOML values may legally contain commas, ampersands
        // and semicolons. Those are delimiters only in URL/flag/JSON contexts.
        let end = line_value.find(" #").unwrap_or(line_value.len());
        return (end > 0).then_some(start..start + line_value[..end].trim_end().len());
    }
    let mut escaped = false;
    let end = tail
        .char_indices()
        .find_map(|(offset, ch)| {
            if matches!(context, ValueContext::Shell) {
                if escaped {
                    escaped = false;
                    return None;
                }
                if ch == '\\' {
                    escaped = true;
                    return None;
                }
            }
            context.ends_at(ch).then_some(offset)
        })
        .unwrap_or(tail.len());
    let raw = &tail[..end];
    let end = raw.find(" #").unwrap_or(raw.len());
    let length = raw[..end].trim_end().len();
    (length > 0).then_some(start..start + length)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assert_schema_allows_marker(schema: &Value, marker: &Value) {
        let pattern = schema["anyOf"][1]["pattern"].as_str().unwrap();
        let pattern = Regex::new(pattern).unwrap();
        assert!(pattern.is_match(marker.as_str().unwrap()));
        assert!(!pattern.is_match("__CROSSLANE_SECRET_00000000000000000000000000000000_0__"));
    }

    #[test]
    fn removes_config_values_without_destroying_quoted_syntax() {
        let source = "export OPENAI_API_KEY='opaque secret with spaces'\npassword: 中文 password with spaces\n{\"accessToken\":\"opaque-token\",\"max_tokens\":256}\n--password 'shell secret' --token=another-secret\n";
        let safe = redact_text(source, &[]);
        for secret in [
            "opaque secret with spaces",
            "中文 password with spaces",
            "opaque-token",
            "shell secret",
            "another-secret",
        ] {
            assert!(!safe.contains(secret), "{safe}");
        }
        assert!(safe.contains("\"max_tokens\":256"));
        assert!(safe.contains("OPENAI_API_KEY='[REDACTED]'"));
        assert_eq!(redact_text(&safe, &[]), safe);
    }

    #[test]
    fn recognizes_standalone_credentials_and_authentication_forms() {
        for secret in [
            "ksk_synthetic-key",
            "sk-proj-abcdefgh12345678",
            "ghp_abcdefgh12345678",
            "github_pat_abcdefgh12345678",
            "glpat-abcdefgh12345678",
            "xoxb-1234567890-abcdef",
            "AKIA1234567890ABCDEF",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJhIn0.signature",
        ] {
            assert_eq!(redact_text(secret, &[]), REDACTED);
        }
        assert_eq!(
            redact_text(
                "Authorization: Bearer abcdef\nProxy-Authorization: Basic YTpi\n",
                &[]
            ),
            "Authorization: Bearer [REDACTED]\nProxy-Authorization: Basic [REDACTED]\n"
        );
        assert_eq!(
            redact_text(
                "https://user:pass@example.invalid/p?access_token=opaque&limit=10",
                &[]
            ),
            "https://[REDACTED]@example.invalid/p?access_token=[REDACTED]&limit=10"
        );
    }

    #[test]
    fn preserves_nonsecret_text_and_control_parameters() {
        let source = "max_tokens=100\ntoken_count: 200\nkeyboard: letters\npublic_key_algorithm: RSA\nThe token count and key names are useful. 中文😀";
        assert_eq!(redact_text(source, &[]), source);
        let mut value = json!({"max_tokens":100,"token_count":200,"keyboard":"letters","apiKey":"opaque","nested":{"password":1234}});
        let mut redactor = Redactor::new(&[]);
        redactor.data(&mut value, false);
        assert_eq!(
            value,
            json!({"max_tokens":100,"token_count":200,"keyboard":"letters","apiKey":REDACTED,"nested":{"password":REDACTED}})
        );
    }

    #[test]
    fn removes_private_keys_known_values_and_escaped_assignments() {
        let pem = "before\n-----BEGIN RSA PRIVATE KEY-----\nline1\nline2\n-----END RSA PRIVATE KEY-----\nafter";
        assert_eq!(redact_text(pem, &[]), "before\n[REDACTED]\nafter");
        assert_eq!(
            redact_text("prefix-long-secret-suffix", &["long-secret", "secret"]),
            "prefix-[REDACTED]-suffix"
        );
        let source = r#"curl -d "{\"password\":\"opaque secret\"}""#;
        let safe = redact_text(source, &[]);
        assert!(!safe.contains("opaque secret"), "{safe}");
        assert_eq!(redact_text(&safe, &[]), safe);
    }

    #[test]
    fn schema_keeps_declarations_but_masks_sensitive_defaults() {
        let mut schema = json!({"type":"object","properties":{"password":{"type":"string","description":"A password to use","default":"opaque-value","examples":["example-value"]},"count":{"type":"integer","default":10}},"required":["password"],"additionalProperties":false});
        let mut redactor = Redactor::new(&[]);
        redactor.schema(&mut schema, false);
        assert_eq!(schema["properties"]["password"]["type"], "string");
        assert_eq!(schema["properties"]["password"]["default"], REDACTED);
        assert_eq!(schema["properties"]["password"]["examples"][0], REDACTED);
        assert_eq!(schema["required"], json!(["password"]));
        assert_eq!(schema["properties"]["count"]["default"], 10);
    }
    #[test]
    fn discovers_labeled_secrets_before_scrubbing_every_message() {
        let request: crate::ClaudeRequest = serde_json::from_value(json!({
            "model":"claude-sonnet-4", "max_tokens":128,
            "messages":[
                {"role":"user", "content":"history password=hunter2"},
                {"role":"assistant", "content":"hunter2 was echoed here"},
                {"role":"user", "content":"repeat hunter2 without a label"}
            ]
        }))
        .unwrap();
        let mut payload = crate::claude_to_kiro(
            &request,
            &crate::TranslationOptions::new("claude-sonnet-4", "AI_EDITOR"),
        );
        let stats = redact_kiro_payload(&mut payload, &[]);
        assert!(stats.redacted_values >= 3);
        assert!(!serde_json::to_string(&payload).unwrap().contains("hunter2"));
        assert_eq!(
            redact_kiro_payload(&mut payload, &[]),
            RedactionStats::default()
        );
        let known = ["hunter2"];
        assert_eq!(
            redact_text("hunter2 + password=hunter2", &known),
            "[REDACTED] + password=[REDACTED]"
        );
    }

    #[test]
    fn recognizes_chinese_labels_and_tokens_without_ascii_word_boundaries() {
        for source in [
            "密码: 中文密码内容",
            "密钥=opaque-secret",
            "口令：some-text",
        ] {
            assert!(redact_text(source, &[]).contains(REDACTED));
        }
        assert_eq!(redact_text("密钥ksk_fixture_secret", &[]), "密钥[REDACTED]");
        assert_eq!(
            redact_text("/tmp/prefix_ksk_fixture_secret", &[]),
            "/tmp/prefix_[REDACTED]"
        );
        assert_eq!(redact_text("sk_live_abcdefgh12345678", &[]), REDACTED);
        assert_eq!(
            redact_text("AIzaabcdefghijklmnopqrstuvwxyz1234", &[]),
            REDACTED
        );
        assert_eq!(
            redact_text("Basic setup and basic operations", &[]),
            "Basic setup and basic operations"
        );
    }

    #[test]
    fn cookies_and_numeric_values_do_not_leave_trailing_secrets() {
        let safe = redact_text("Cookie: session=opaque-one; other=opaque-two\n", &[]);
        assert_eq!(safe, "Cookie: [REDACTED]\n");
        let safe = redact_text(r#"{"password":1234,"max_tokens":256}"#, &[]);
        assert!(!safe.contains("1234"));
        assert!(safe.contains("\"max_tokens\":256"));
        assert_eq!(redact_text(&safe, &[]), safe);
    }
    #[test]
    fn data_dictionary_keys_are_filtered_without_losing_values_on_collisions() {
        let mut value =
            json!({"ksk_synthetic_one":"first", "ksk_synthetic_two":"second", "ordinary":"safe"});
        let mut redactor = Redactor::new(&[]);
        redactor.data(&mut value, false);
        assert_eq!(
            value,
            json!({"[REDACTED]":"first", "[REDACTED]_2":"second", "ordinary":"safe"})
        );
        let before = value.clone();
        redactor.data(&mut value, false);
        assert_eq!(value, before);
        assert_eq!(
            redact_text("APP_SECRET=opaque-pass\nappSecret: opaque-pass", &[]),
            "APP_SECRET=[REDACTED]\nappSecret: [REDACTED]"
        );
    }
    #[test]
    fn unquoted_punctuation_and_yaml_block_values_are_removed_completely() {
        assert_eq!(
            redact_text("PASSWORD=alpha,beta&gamma;delta\nkeep=value", &[]),
            "PASSWORD=[REDACTED]\nkeep=value"
        );
        for indicator in ["|", ">", "|-"] {
            for key in ["password", "\"password\"", "'password'"] {
                let source =
                    format!("{key}: {indicator}\n  opaque secret\n  second line\nordinary: keep");
                let safe = redact_text(&source, &[]);
                assert_eq!(safe, format!("{key}: [REDACTED]\nordinary: keep"));
                assert_eq!(redact_text(&safe, &[]), safe);
            }
        }
        assert_eq!(
            redact_text("PASSWORD=[REDACTED]hidden-suffix", &[]),
            "PASSWORD=[REDACTED]"
        );
        assert_eq!(
            redact_text("PASSWORD=[REDACTED],hidden-suffix", &[]),
            "PASSWORD=[REDACTED]"
        );
        assert_eq!(
            redact_text("--password alpha,beta --verbose", &[]),
            "--password [REDACTED] --verbose"
        );
        assert_eq!(
            redact_text(
                "https://example.invalid/?password=alpha,beta;gamma&limit=10",
                &[]
            ),
            "https://example.invalid/?password=[REDACTED]&limit=10"
        );
        assert_eq!(
            redact_text(
                "https://example.invalid/?password=[REDACTED],suffix&limit=10",
                &[]
            ),
            "https://example.invalid/?password=[REDACTED]&limit=10"
        );
        assert_eq!(
            redact_text("password = \"\"\"multiline\nopaque secret\"\"\"", &[]),
            "password = \"\"\"[REDACTED]\"\"\""
        );
    }

    #[test]
    fn reversible_markers_are_stable_isolated_and_do_not_leak_through_debug() {
        let session = RedactionSession::new();
        let original = "token=opaque-secret-1234\nrepeat opaque-secret-1234";
        let safe = session.redact_text(original, &[]);
        assert!(!safe.contains("opaque-secret-1234"));
        let markers: Vec<_> = marker_pattern().find_iter(&safe).collect();
        assert_eq!(markers.len(), 2);
        assert_eq!(markers[0].as_str(), markers[1].as_str());
        assert_eq!(session.restore_text(&safe), original);
        assert_eq!(session.clone().restore_text(&safe), original);
        assert_eq!(session.redact_text(&safe, &[]), safe);
        let unrelated = RedactionSession::new();
        assert_eq!(unrelated.restore_text(&safe), safe);
        assert_ne!(unrelated.redact_text(original, &[]), safe);
        let debug = format!("{session:?}");
        assert!(!debug.contains("opaque-secret-1234"));
        assert!(!debug.contains(markers[0].as_str()));
        assert!(!session.is_empty());
    }

    #[test]
    fn reversible_json_retains_types_escaping_and_dictionary_keys() {
        let session = RedactionSession::new();
        let original = json!({
            "password":1234,
            "nested":{"token":false},
            "api_key":"quotes \" and \\ backslash\n中文😀",
            "command":"curl -H 'Authorization: Bearer opaque-credential' https://example.invalid/",
            "ksk_dictionary_secret":"safe value"
        });
        let mut safe = original.clone();
        let mut redactor = Redactor::new(&[]);
        redactor.session = Some(session.clone());
        redactor.data(&mut safe, false);
        assert!(safe["password"].is_string());
        assert!(safe["nested"]["token"].is_string());
        let wire = serde_json::to_string(&safe).unwrap();
        let mut returned: Value = serde_json::from_str(&wire).unwrap();
        session.restore_tool_input(&mut returned);
        assert_eq!(returned, original);
        assert!(returned["password"].is_number());
        assert!(returned["nested"]["token"].is_boolean());
    }

    #[test]
    fn restored_original_marker_text_is_never_recursively_expanded() {
        let session = RedactionSession::new();
        let first = session.marker(Value::String("first-real-secret".into()));
        let literal = format!("literal {first} embedded in a different value");
        let second = session.marker(Value::String(literal.clone()));
        assert_eq!(session.restore_text(&second), literal);
        let mut value = Value::String(second);
        session.restore_tool_input(&mut value);
        assert_eq!(value, Value::String(literal));
        let forged = "__CROSSLANE_SECRET_00000000000000000000000000000000_0__";
        assert_eq!(session.restore_text(forged), forged);
    }

    #[test]
    fn shell_and_environment_references_are_preserved_in_sensitive_contexts() {
        for reference in [
            "$API_TOKEN",
            "${API_TOKEN}",
            "%API_TOKEN%",
            "$env:API_TOKEN",
            "process.env.API_TOKEN",
            "os.environ[\"API_TOKEN\"]",
            "os.getenv(\"API_TOKEN\")",
        ] {
            let session = RedactionSession::new();
            let command =
                format!("curl -H 'Authorization: Bearer {reference}' https://example.invalid/");
            assert_eq!(session.redact_text(&command, &[]), command);
            assert_eq!(redact_text(&command, &[]), command);
            let mut value = json!({"token":reference,"headers":{"Authorization":format!("Bearer {reference}")}});
            let original = value.clone();
            let mut redactor = Redactor::new(&[]);
            redactor.session = Some(session.clone());
            redactor.data(&mut value, false);
            assert_eq!(value, original);
            assert!(session.is_empty());
        }
        let command = "API_TOKEN=$TOKEN curl --token=${TOKEN} --password %PASSWORD%";
        let session = RedactionSession::new();
        assert_eq!(session.redact_text(command, &[]), command);
    }

    #[test]
    fn structured_literal_passwords_with_dollars_are_fully_masked_and_restored() {
        let session = RedactionSession::new();
        let original = json!({
            "password":"abc$def",
            "token":"literal-${PART}-password",
            "api_key":"prefix%VALUE%suffix"
        });
        let mut safe = original.clone();
        let mut redactor = Redactor::new(&[]);
        redactor.session = Some(session.clone());
        redactor.data(&mut safe, false);
        for value in safe.as_object().unwrap().values() {
            assert!(value.as_str().unwrap().starts_with("__CROSSLANE_SECRET_"));
            assert!(!value.as_str().unwrap().contains('$'));
            assert!(!value.as_str().unwrap().contains('%'));
        }
        assert!(session.known_values().contains(&"abc$def".to_owned()));
        let repeated = session.redact_text("Use abc$def now", &[]);
        assert!(!repeated.contains("$def"));
        assert_eq!(session.restore_text(&repeated), "Use abc$def now");
        let known = session.known_values();
        let known_refs: Vec<_> = known.iter().map(String::as_str).collect();
        let mut http = json!({"value":"abc$def"});
        let mut redactor = Redactor::new(&known_refs);
        redactor.session = Some(session.clone());
        redactor.data(&mut http, false);
        assert!(!http.to_string().contains("$def"));
        session.restore_tool_input(&mut http);
        assert_eq!(http["value"], "abc$def");
        assert_eq!(
            session.redact_text("$abcdefgh$ijklmnop", &["abcdefgh", "ijklmnop"]),
            "$abcdefgh$ijklmnop"
        );
        session.restore_tool_input(&mut safe);
        assert_eq!(safe, original);
    }

    #[test]
    fn prefix_prefilters_preserve_case_insensitive_and_unicode_token_rules() {
        for source in [
            "KSK_fixture_value",
            "ſk-abcdefgh12345678",
            "KSK_fixture_value",
            "bEaReR fixture-token",
        ] {
            assert!(redact_text(source, &[]).contains(REDACTED));
        }
    }

    #[test]
    fn native_model_controls_survive_secret_collisions_while_extensions_are_masked() {
        let controls = json!({
            "thinking":{"type":"enabled","display":"summarized","budget_tokens":1234},
            "reasoning":{"effort":"high"},
            "output_config":{"effort":"high"}
        });
        for password in ["enabled", "high", "thinking", "effort"] {
            let mut payload: KiroPayload = serde_json::from_value(json!({
                "conversationState":{
                    "chatTriggerType":"MANUAL","conversationId":"native-controls",
                    "history":[{"assistantResponseMessage":{
                        "content":"", "toolUses":[{"toolUseId":"source","name":"http_request","input":{"password":password,"value":password}}]
                    }}],
                    "currentMessage":{"userInputMessage":{
                        "content":"continue","modelId":"model","origin":"AI_EDITOR"
                    }}
                }
            })).unwrap();
            let mut fields = controls.clone();
            fields["extension"] = json!({"text":password});
            payload.additional_model_request_fields = Some(fields);
            let session = RedactionSession::new();
            session.redact_payload(&mut payload, &[]);
            assert!(payload.thinking_enabled());
            let fields = payload.additional_model_request_fields.as_ref().unwrap();
            for name in ["thinking", "reasoning", "output_config"] {
                assert_eq!(fields[name], controls[name]);
            }
            assert_ne!(fields["extension"]["text"], password);
            let mut returned = payload.conversation_state.history[0]
                .assistant_response_message
                .as_ref()
                .unwrap()
                .tool_uses[0]
                .input
                .clone();
            assert_ne!(returned["value"], password);
            session.restore_tool_input(&mut returned);
            assert_eq!(returned, json!({"password":password,"value":password}));
        }
        let session = RedactionSession::new();
        let mut fields =
            json!({"thinking":{"type":"enabled","password":"opaque-control-extension"}});
        let mut redactor = Redactor::new(&[]);
        redactor.session = Some(session.clone());
        redactor.additional_model_fields(&mut fields);
        assert!(!fields.to_string().contains("opaque-control-extension"));
        assert!(!native_model_control(
            "reasoning",
            &json!({"effort":"opaque-unknown-enum"})
        ));
    }

    #[test]
    fn bounded_assignment_probes_match_the_original_capture_rule() {
        let original = Regex::new(r#"(?P<name>(?:\\?["'])?[\p{L}_][\p{L}\p{N}_.-]{0,127}(?:\\?["'])?)[ \t]*(?:=|:|：|＝)[ \t]*"#).unwrap();
        let sources = [
            "password: 中文\n--token=fixture&limit=1".to_owned(),
            r#"{\"password\" : \"secret\", 'token': 'other'}"#.to_owned(),
            format!("{}password = value", "a".repeat(200)),
            format!("{}密码 ＝secret", "密".repeat(140)),
            format!("SecretKey{}=\t  value", " ".repeat(65536)),
            "a=   b:   c\n'x   =    y' token=next\nC:\\password=fixture".to_owned(),
        ];
        for source in sources {
            let expected: Vec<_> = original
                .captures_iter(&source)
                .map(|captures| {
                    let whole = captures.get(0).unwrap();
                    (
                        captures.name("name").unwrap().as_str().to_owned(),
                        whole.start(),
                        whole.end(),
                    )
                })
                .collect();
            let actual: Vec<_> = assignments(&source)
                .map(|found| (found.name.to_owned(), found.start, found.value_start))
                .collect();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn allocation_bounds_allow_large_plain_secrets_but_reject_repeated_expansion() {
        let session = RedactionSession::new();
        let secret = "a".repeat(3 * 1024 * 1024);
        let marker = session.marker(json!(secret));
        let input = json!({"token":marker});
        assert!(session.tool_input_expansion_bound(&input) < 16 * 1024 * 1024);
        assert!(
            session.tool_input_expansion_bound(&json!({"tokens":vec![marker;20]}))
                > 16 * 1024 * 1024
        );
        for literal in ["a'b\\c\"d$e`f\n\r\t", "\u{1}\u{0}", "中文😀"] {
            let marker = session.marker(json!(literal));
            let mut input = json!({"command":format!("printf '%s' {marker}")});
            let bound = session.tool_input_expansion_bound(&input);
            session.restore_tool_input_for_tool("Bash", &mut input);
            assert!(bound >= input.to_string().len());
        }
    }

    #[test]
    fn known_shell_history_registers_semantic_secrets_for_other_tool_inputs() {
        let command = r#"printf '%s' --token "opaque\\fixture""#;
        let semantic = r"opaque\fixture";
        let mut payload: KiroPayload = serde_json::from_value(json!({
            "conversationState":{
                "chatTriggerType":"MANUAL","conversationId":"shell-provenance",
                "history":[{"assistantResponseMessage":{
                    "content":"", "toolUses":[{
                        "toolUseId":"shell-source", "name":"Bash", "input":{"command":command}
                    }]
                }}],
                "currentMessage":{"userInputMessage":{
                    "content":format!("Use {semantic} next"),"modelId":"model","origin":"AI_EDITOR"
                }}
            }
        }))
        .unwrap();
        let session = RedactionSession::new();
        session.redact_payload(&mut payload, &[]);
        let masked = &payload.conversation_state.history[0]
            .assistant_response_message
            .as_ref()
            .unwrap()
            .tool_uses[0]
            .input;
        let marker = marker_pattern()
            .find(masked["command"].as_str().unwrap())
            .unwrap()
            .as_str();
        let mut http = json!({"token":marker});
        session.restore_tool_input(&mut http);
        assert_eq!(http["token"], semantic);
        assert!(!payload
            .conversation_state
            .current_message
            .user_input_message
            .content
            .contains(semantic));
        let mut replay = masked.clone();
        session.restore_tool_input_for_tool("Bash", &mut replay);
        assert_eq!(replay["command"], command);
        let ansi = "cloud --token $'opaque\\nsecret'";
        let safe = session.redact_text(ansi, &[]);
        assert!(safe.contains("$'__CROSSLANE_SECRET_"));
        assert_eq!(session.restore_text(&safe), ansi);
    }

    #[test]
    fn reversible_payload_scrubs_repeated_secrets_before_restoring_tool_arguments() {
        let request: crate::ClaudeRequest = serde_json::from_value(json!({
            "model":"claude-sonnet-4", "max_tokens":128,
            "tools":[{"name":"request","description":"send","input_schema":{"type":"object"}}],
            "messages":[
                {"role":"user","content":"token=opaque-cross-message-secret"},
                {"role":"assistant","content":[{"type":"tool_use","id":"tool-1","name":"request","input":{"token":"opaque-cross-message-secret","count":3}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"tool-1","content":"opaque-cross-message-secret was accepted"}]}
            ]
        })).unwrap();
        let mut payload = crate::claude_to_kiro(
            &request,
            &crate::TranslationOptions::new("claude-sonnet-4", "AI_EDITOR"),
        );
        let session = RedactionSession::new();
        session.redact_payload(&mut payload, &[]);
        assert!(!serde_json::to_string(&payload)
            .unwrap()
            .contains("opaque-cross-message-secret"));
        let mut arguments = payload
            .conversation_state
            .history
            .iter()
            .filter_map(|item| item.assistant_response_message.as_ref())
            .flat_map(|message| message.tool_uses.iter())
            .next()
            .unwrap()
            .input
            .clone();
        session.restore_tool_input(&mut arguments);
        assert_eq!(
            arguments,
            json!({"token":"opaque-cross-message-secret","count":3})
        );
    }

    #[test]
    fn reference_fallback_literals_and_markdown_code_are_not_secret_bypasses() {
        let session = RedactionSession::new();
        let source = "curl -H 'Authorization: Bearer ${API_TOKEN:-ksk_literal_fallback}' https://example.invalid/";
        let safe = session.redact_text(source, &[]);
        assert!(safe.contains("${API_TOKEN:-__CROSSLANE_SECRET_"));
        assert!(!safe.contains("ksk_literal_fallback"));
        assert_eq!(session.restore_text(&safe), source);
        let mut value = json!({"token":"${ENV:-opaque-default-value}"});
        let original = value.clone();
        let mut redactor = Redactor::new(&[]);
        redactor.session = Some(session.clone());
        redactor.data(&mut value, false);
        assert!(value["token"]
            .as_str()
            .unwrap()
            .starts_with("${ENV:-__CROSSLANE_SECRET_"));
        session.restore_tool_input(&mut value);
        assert_eq!(value, original);
        for source in [
            "The key is `ksk_literal_markdown_secret`.",
            "$(printf ksk_literal_command_secret)",
        ] {
            let safe = session.redact_text(source, &[]);
            assert!(!safe.contains("ksk_literal_"));
            assert_eq!(session.restore_text(&safe), source);
        }
    }

    #[test]
    fn same_session_remembers_unlabeled_values_after_summary_restoration() {
        let session = RedactionSession::new();
        let first = session.redact_text("password=opaque-continuation-secret", &[]);
        let marker = marker_pattern().find(&first).unwrap().as_str();
        let summary = session.restore_text(&format!("Use {marker} for the next tool request"));
        assert!(summary.contains("opaque-continuation-secret"));
        let next = session.redact_text(&summary, &[]);
        assert!(!next.contains("opaque-continuation-secret"));
        assert!(next.contains(marker));
        let mut payload: KiroPayload = serde_json::from_value(json!({
            "conversationState":{
                "chatTriggerType":"MANUAL","conversationId":"continuation",
                "history":[],"currentMessage":{"userInputMessage":{
                    "content":summary,"modelId":"model","origin":"AI_EDITOR"
                }}
            }
        }))
        .unwrap();
        session.redact_payload(&mut payload, &[]);
        assert!(!payload
            .conversation_state
            .current_message
            .user_input_message
            .content
            .contains("opaque-continuation-secret"));
        assert!(payload
            .conversation_state
            .current_message
            .user_input_message
            .content
            .contains(marker));
    }

    #[test]
    fn sensitive_schema_properties_accept_session_scoped_placeholders() {
        let request: crate::ClaudeRequest = serde_json::from_value(json!({
            "model":"claude-sonnet-4", "max_tokens":128,
            "tools":[{"name":"validate","description":"validate fixture","input_schema":{
                "type":"object","properties":{
                    "password":{"type":"integer","default":1234,"const":1234},
                    "token":{"type":"boolean","enum":[true,false]},
                    "limit":{"type":"integer","minimum":1,"maximum":10}
                },"required":["password","token"],"additionalProperties":false
            }}],
            "messages":[{"role":"user","content":"validate configured credentials"}]
        }))
        .unwrap();
        let mut payload = crate::claude_to_kiro(
            &request,
            &crate::TranslationOptions::new("claude-sonnet-4", "AI_EDITOR"),
        );
        let session = RedactionSession::new();
        session.redact_payload(&mut payload, &[]);
        let schema = &payload
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .as_ref()
            .unwrap()
            .tools[0]
            .specification()
            .unwrap()
            .input_schema
            .json;
        let password = &schema["properties"]["password"];
        let token = &schema["properties"]["token"];
        assert_eq!(password["anyOf"][0]["type"], "integer");
        assert_eq!(token["anyOf"][0]["type"], "boolean");
        let number_marker = password["anyOf"][0]["default"].clone();
        assert_schema_allows_marker(password, &number_marker);
        let bool_marker = token["anyOf"][0]["enum"][0].clone();
        assert_schema_allows_marker(token, &bool_marker);
        assert_eq!(
            schema["properties"]["limit"]["anyOf"][0],
            json!({"type":"integer","minimum":1,"maximum":10})
        );
        assert_eq!(schema["required"], json!(["password", "token"]));
        let mut input = json!({"password":number_marker,"token":bool_marker,"limit":3});
        session.restore_tool_input(&mut input);
        assert_eq!(input, json!({"password":1234,"token":true,"limit":3}));
        let before = serde_json::to_value(&payload).unwrap();
        assert_eq!(
            session.redact_payload(&mut payload, &[]),
            RedactionStats::default()
        );
        assert_eq!(serde_json::to_value(&payload).unwrap(), before);
    }

    #[test]
    fn ordinary_schema_names_with_masked_constants_receive_scoped_alternatives() {
        let request: crate::ClaudeRequest = serde_json::from_value(json!({
            "model":"claude-sonnet-4", "max_tokens":128,
            "tools":[{"name":"check","description":"check","input_schema":{
                "type":"object","properties":{
                    "license":{"type":"string","pattern":"^ksk_","const":"ksk_synthetic_license"},
                    "label":{"type":"string","pattern":"^prefix_ksk_","enum":["prefix_ksk_synthetic_label"]},
                    "mode":{"type":"string","enum":["safe","normal"]}
                }
            }}],"messages":[{"role":"user","content":"check the configured license"}]
        })).unwrap();
        let mut payload = crate::claude_to_kiro(
            &request,
            &crate::TranslationOptions::new("claude-sonnet-4", "AI_EDITOR"),
        );
        let session = RedactionSession::new();
        session.redact_payload(&mut payload, &[]);
        let schema = &payload
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .as_ref()
            .unwrap()
            .tools[0]
            .specification()
            .unwrap()
            .input_schema
            .json;
        let license = &schema["properties"]["license"];
        let label = &schema["properties"]["label"];
        assert_eq!(license["anyOf"][0]["pattern"], "^ksk_");
        let license_value = license["anyOf"][0]["const"].clone();
        let label_value = label["anyOf"][0]["enum"][0].clone();
        assert_schema_allows_marker(license, &license_value);
        assert_schema_allows_marker(label, &label_value);
        assert_eq!(
            schema["properties"]["mode"]["anyOf"][0],
            json!({"type":"string","enum":["safe","normal"]})
        );
        let mut input = json!({"license":license_value,"label":label_value});
        session.restore_tool_input(&mut input);
        assert_eq!(
            input,
            json!({"license":"ksk_synthetic_license","label":"prefix_ksk_synthetic_label"})
        );
        let before = serde_json::to_value(&payload).unwrap();
        session.redact_payload(&mut payload, &[]);
        assert_eq!(serde_json::to_value(&payload).unwrap(), before);
    }

    #[test]
    fn json_tool_result_secrets_restore_from_semantic_values_not_encoded_text() {
        let session = RedactionSession::new();
        let source = r#"{"password":"a\"b\\c\n中文","count":12}"#;
        let safe = session.redact_text(source, &[]);
        let mut model_input: Value = serde_json::from_str(&safe).unwrap();
        session.restore_tool_input(&mut model_input);
        assert_eq!(model_input, serde_json::from_str::<Value>(source).unwrap());
        assert_eq!(model_input["password"], "a\"b\\c\n中文");
        assert_eq!(
            serde_json::from_str::<Value>(&session.restore_text(&safe)).unwrap(),
            serde_json::from_str::<Value>(source).unwrap()
        );
        assert_eq!(session.redact_text(&safe, &[]), safe);
    }

    #[test]
    fn exact_json_text_replay_preserves_signed_body_bytes_and_expansion_bound() {
        let session = RedactionSession::new();
        let first = "{\n  \"password\": \"same-json-fixture\",\n  \"value\": 1e+02\n}\n";
        let second = "{\"password\":\"same-json-fixture\",\"value\":100.0}";
        let safe_first = session.redact_text(first, &[]);
        let safe_second = session.redact_text(second, &[]);
        assert_ne!(safe_first, safe_second);
        for (safe, original) in [(safe_first, first), (safe_second, second)] {
            assert!(!safe.contains("same-json-fixture"));
            let mut input = json!({"body":safe});
            assert!(session.tool_input_expansion_bound(&input) >= original.len());
            session.restore_tool_input(&mut input);
            assert_eq!(input["body"], original);
        }
        let padded = format!("{{{}\"token\":\"small-secret\"}}", " ".repeat(2_200_000));
        let safe = session.redact_text(&padded, &[]);
        assert!(safe.len() < 1000);
        let input = json!({"body":safe});
        assert!(session.tool_input_expansion_bound(&input) >= padded.len());
        assert!(session.tool_input_expansion_bound(&input) < 16 * 1024 * 1024);
        assert!(
            session.tool_input_expansion_bound(&json!({"bodies":vec![input["body"].clone();20]}))
                > 16 * 1024 * 1024
        );
    }

    #[test]
    fn constrained_ordinary_fields_and_scalar_compositions_accept_copied_markers() {
        let session = RedactionSession::new();
        let marker = session.marker(json!("opaque-unlabeled-fixture"));
        let entries = vec![(marker.clone(), json!("opaque-unlabeled-fixture"))];
        let original = json!({"type":"object","properties":{
            "value":{"type":"string","pattern":"^[a-z-]+$","maxLength":32},
            "choice":{"oneOf":[{"type":"string","pattern":"^opaque-"},{"type":"string","const":"other"}]},
            "plain":{"type":"string"}
        }});
        let mut schema = original.clone();
        allow_schema_markers(&mut schema, false, &entries);
        let marker_value = json!(marker);
        assert_schema_allows_marker(&schema["properties"]["value"], &marker_value);
        assert_schema_allows_marker(&schema["properties"]["choice"], &marker_value);
        assert_eq!(
            schema["properties"]["choice"]["anyOf"][0],
            original["properties"]["choice"]
        );
        assert_eq!(
            schema["properties"]["plain"],
            original["properties"]["plain"]
        );
        let before = schema.clone();
        allow_schema_markers(&mut schema, false, &entries);
        assert_eq!(schema, before);
        let mut untouched = original.clone();
        allow_schema_markers(&mut untouched, false, &[]);
        assert_eq!(untouched, original);
        let mut input = json!({"value":marker_value,"choice":marker_value});
        session.restore_tool_input(&mut input);
        assert_eq!(input["value"], "opaque-unlabeled-fixture");
        assert_eq!(input["choice"], "opaque-unlabeled-fixture");
    }

    #[test]
    fn object_one_of_with_shared_marker_properties_uses_transport_any_of() {
        let session = RedactionSession::new();
        let marker = session.marker(json!("UPPERCASE"));
        let entries = vec![(marker, json!("UPPERCASE"))];
        let original = json!({"oneOf":[
            {"type":"object","properties":{"value":{"type":"string","pattern":"^[A-Z]+$"}},"required":["value"]},
            {"type":"object","properties":{"value":{"type":"string","pattern":"^[a-z]+$"}},"required":["value"]}
        ]});
        let mut safe = original.clone();
        allow_schema_markers(&mut safe, false, &entries);
        assert!(safe.get("oneOf").is_none());
        let branches = safe["allOf"][0]["anyOf"].as_array().unwrap();
        for branch in branches {
            assert_schema_allows_marker(&branch["properties"]["value"], &json!(entries[0].0));
        }
        let once = safe.clone();
        allow_schema_markers(&mut safe, false, &entries);
        assert_eq!(safe, once);
        let mut untouched = original.clone();
        allow_schema_markers(&mut untouched, false, &[]);
        assert_eq!(untouched, original);
    }

    #[test]
    fn checkpoint_capsules_survive_new_sessions_without_exposing_opaque_secrets() {
        let first = RedactionSession::new();
        let mut values = json!({
            "token":"opaque \\\" fixture\n</summary> 中文",
            "password":1234,
            "secret":false
        });
        let original = values.clone();
        let mut redactor = Redactor::new(&[]);
        redactor.session = Some(first.clone());
        redactor.data(&mut values, false);
        let summary = first.checkpoint_text(&format!("Checkpoint {}", values));
        assert_eq!(
            first.checkpoint_expansion_bound(&format!("Checkpoint {}", values)),
            summary.len()
        );
        assert!(summary.contains("__CROSSLANE_CHECKPOINT_"));
        assert!(!summary.contains("opaque"));
        assert!(!summary.contains("</summary>"));
        let irreversible = redact_text(&summary, &[]);
        assert!(!irreversible.contains("__CROSSLANE_CHECKPOINT_"));
        assert!(irreversible.contains(REDACTED));
        let second = RedactionSession::new();
        let replay = format!("{summary}\nUse opaque \\\" fixture\n</summary> 中文 again.");
        let safe = second.redact_text(&replay, &[]);
        assert!(!safe.contains("__CROSSLANE_CHECKPOINT_"));
        assert!(!safe.contains("opaque"));
        let safe_json = safe
            .strip_prefix("Checkpoint ")
            .unwrap()
            .lines()
            .next()
            .unwrap();
        let mut tool: Value = serde_json::from_str(safe_json).unwrap();
        second.restore_tool_input(&mut tool);
        assert_eq!(tool, original);
        let auth = format!("Bearer {}", values["token"].as_str().unwrap());
        let checkpoint_auth = first.checkpoint_text(&auth);
        let mut auth_data = json!({"Authorization":checkpoint_auth});
        let mut redactor = Redactor::new(&[]);
        redactor.session = Some(second.clone());
        redactor.data(&mut auth_data, false);
        assert!(!auth_data.to_string().contains("CHECKPOINT"));
        second.restore_tool_input(&mut auth_data);
        assert_eq!(
            auth_data["Authorization"],
            format!("Bearer {}", original["token"].as_str().unwrap())
        );
    }
}
