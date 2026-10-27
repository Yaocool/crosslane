//! Local, deterministic secret filtering at the Kiro transport boundary.
//!
//! These rules never make a model or network request. They intentionally operate
//! on model-visible data, not protocol identifiers or authentication headers.
//! Arbitrary unlabeled secrets and opaque binary attachments cannot be classified
//! reliably by text rules; callers must not treat this as a general DLP guarantee.

mod auth;
mod embedded_json;
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
    enabled: bool,
    state: Arc<Mutex<SessionState>>,
}

struct SessionState {
    nonce: String,
    by_value: HashMap<String, String>,
    by_marker: HashMap<String, Value>,
    source_texts: HashMap<String, String>,
    #[cfg(feature = "crs")]
    transmitted_redactions: bool,
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
            .field("enabled", &self.enabled)
            .field("mapped_values", &state.by_marker.len())
            .finish()
    }
}

impl RedactionSession {
    pub fn new() -> Self {
        Self::with_enabled(true)
    }

    /// Freeze the request's redaction policy. Clones and internal continuations
    /// retain it even when the administrator changes the default for new work.
    pub fn with_enabled(enabled: bool) -> Self {
        Self {
            enabled,
            state: Arc::new(Mutex::new(SessionState {
                nonce: uuid::Uuid::new_v4().simple().to_string(),
                by_value: HashMap::new(),
                by_marker: HashMap::new(),
                source_texts: HashMap::new(),
                #[cfg(feature = "crs")]
                transmitted_redactions: false,
            })),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Record protection on an actual outgoing generation payload, rather than
    /// inferring it from mappings discovered by an input-budget estimate.
    #[cfg(feature = "crs")]
    pub fn record_transmitted_payload(&self, payload: &Value) {
        if !self.enabled {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.transmitted_redactions || state.by_marker.is_empty() {
            return;
        }
        let prefix = format!("__CROSSLANE_SECRET_{}_", state.nonce);
        fn contains(value: &Value, prefix: &str, state: &SessionState) -> bool {
            let text_contains = |text: &str| {
                text.contains(prefix)
                    && marker_pattern()
                        .find_iter(text)
                        .any(|marker| state.by_marker.contains_key(marker.as_str()))
            };
            match value {
                Value::String(text) => text_contains(text),
                Value::Array(values) => values.iter().any(|value| contains(value, prefix, state)),
                Value::Object(fields) => fields
                    .iter()
                    .any(|(key, value)| text_contains(key) || contains(value, prefix, state)),
                _ => false,
            }
        }
        state.transmitted_redactions = contains(payload, &prefix, &state);
    }

    /// Safe request-local evidence; neither credential values nor markers leave
    /// this mapping, and disabled sessions can never produce a notice.
    #[cfg(feature = "crs")]
    pub fn has_transmitted_redactions(&self) -> bool {
        self.enabled
            && self
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .transmitted_redactions
    }

    pub fn is_empty(&self) -> bool {
        if !self.enabled {
            return true;
        }
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
        if !self.enabled {
            decode_checkpoint_payload(payload);
            return RedactionStats::default();
        }
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
        if !self.enabled {
            return decode_checkpoint_text(text).0;
        }
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

fn exact_checkpoint_value(text: &str) -> Option<Value> {
    checkpoint_pattern()
        .captures(text)
        .filter(|captures| {
            captures
                .get(0)
                .is_some_and(|capsule| capsule.start() == 0 && capsule.end() == text.len())
        })
        .and_then(|captures| checkpoint_value(&captures))
}

/// Disabling detection must still allow a client to replay a checkpoint made
/// while filtering was enabled. Decode only the reserved, validated capsule
/// representation; this does not classify, remember, or newly hide any value.
fn decode_checkpoint_text(text: &str) -> (String, bool) {
    if !text.contains("__CROSSLANE_CHECKPOINT_") {
        return (text.to_owned(), false);
    }
    if let Ok(mut document) = serde_json::from_str::<Value>(text) {
        if decode_checkpoint_value(&mut document) {
            return (document.to_string(), true);
        }
        return (text.to_owned(), false);
    }
    decode_checkpoint_text_fragment(text)
}

fn decode_checkpoint_value(value: &mut Value) -> bool {
    match value {
        Value::String(text) if text.contains("__CROSSLANE_CHECKPOINT_") => {
            if let Some(original) = exact_checkpoint_value(text) {
                *value = original;
                return true;
            }
            // A string may itself carry a complete JSON HTTP body. Handle that
            // layer semantically so quotes/newlines in a credential remain
            // valid JSON rather than being spliced into its encoded source.
            let (decoded, changed) = if text.trim_start().starts_with(['{', '[']) {
                decode_checkpoint_text(text)
            } else {
                decode_checkpoint_text_fragment(text)
            };
            if changed {
                *text = decoded;
            }
            changed
        }
        Value::Array(values) => {
            let mut changed = false;
            for child in values {
                changed |= decode_checkpoint_value(child);
            }
            changed
        }
        Value::Object(fields) => {
            let mut changed = false;
            let original = std::mem::take(fields);
            for (name, mut child) in original {
                let (name, renamed) = if name.contains("__CROSSLANE_CHECKPOINT_") {
                    decode_checkpoint_text(&name)
                } else {
                    (name, false)
                };
                changed |= renamed;
                changed |= decode_checkpoint_value(&mut child);
                fields.insert(name, child);
            }
            changed
        }
        _ => false,
    }
}

fn decode_checkpoint_text_fragment(text: &str) -> (String, bool) {
    let mut changed = false;
    let decoded = checkpoint_pattern()
        .replace_all(text, |captures: &regex::Captures<'_>| {
            checkpoint_value(captures).map_or_else(
                || captures.get(0).expect("checkpoint").as_str().to_owned(),
                |value| {
                    changed = true;
                    value
                        .as_str()
                        .map_or_else(|| value.to_string(), str::to_owned)
                },
            )
        })
        .into_owned();
    (decoded, changed)
}

fn decode_checkpoint_payload(payload: &mut KiroPayload) {
    fn text(value: &mut String) -> bool {
        if value.contains("__CROSSLANE_CHECKPOINT_") {
            let (decoded, changed) = decode_checkpoint_text(value);
            if changed {
                *value = decoded;
            }
            return changed;
        }
        false
    }
    fn user(user: &mut KiroUserInputMessage) {
        text(&mut user.content);
        for document in &mut user.documents {
            text(&mut document.name);
            if matches!(
                document.format.to_ascii_lowercase().as_str(),
                "txt" | "md" | "csv" | "html"
            ) {
                if let Ok(bytes) =
                    base64::engine::general_purpose::STANDARD.decode(&document.source.bytes)
                {
                    if let Ok(mut content) = String::from_utf8(bytes) {
                        if text(&mut content) {
                            document.source.bytes =
                                base64::engine::general_purpose::STANDARD.encode(content);
                        }
                    }
                }
            }
        }
        if let Some(context) = &mut user.user_input_message_context {
            for result in &mut context.tool_results {
                for part in &mut result.content {
                    text(&mut part.text);
                }
            }
            for tool in &mut context.tools {
                if let Some(spec) = tool.specification_mut() {
                    text(&mut spec.description);
                    decode_checkpoint_value(&mut spec.input_schema.json);
                }
            }
            if let Some(env) = &mut context.env_state {
                if let Some(value) = &mut env.current_working_directory {
                    text(value);
                }
                if let Some(value) = &mut env.operating_system {
                    text(value);
                }
            }
        }
    }
    user(
        &mut payload
            .conversation_state
            .current_message
            .user_input_message,
    );
    for item in &mut payload.conversation_state.history {
        if let Some(message) = &mut item.user_input_message {
            user(message);
        }
        if let Some(assistant) = &mut item.assistant_response_message {
            text(&mut assistant.content);
            for tool in &mut assistant.tool_uses {
                decode_checkpoint_tool_input(&tool.name, &mut tool.input);
            }
        }
    }
    if let Some(fields) = &mut payload.additional_model_request_fields {
        decode_checkpoint_value(fields);
    }
}

fn decode_checkpoint_tool_input(tool_name: &str, input: &mut Value) {
    const COMMAND_FIELDS: [&str; 4] = ["command", "cmd", "script", "input"];
    let Value::Object(fields) = input else {
        decode_checkpoint_value(input);
        return;
    };
    // Decode metadata such as `shell` first, but defer possible command text
    // until its executor is known. Ordinary HTTP fields use the generic path.
    let original = std::mem::take(fields);
    for (name, mut value) in original {
        let name = if name.contains("__CROSSLANE_CHECKPOINT_") {
            decode_checkpoint_text(&name).0
        } else {
            name
        };
        if !COMMAND_FIELDS.contains(&name.as_str()) || !value.is_string() {
            decode_checkpoint_value(&mut value);
        }
        fields.insert(name, value);
    }
    let posix = shell::command_sources(tool_name, input)
        .iter()
        .any(|command| command.contains("__CROSSLANE_CHECKPOINT_"));
    if !posix {
        for field in COMMAND_FIELDS {
            if let Some(value) = input.get_mut(field) {
                decode_checkpoint_value(value);
            }
        }
        return;
    }
    let compatibility = RedactionSession::new();
    for field in COMMAND_FIELDS {
        let Some(value) = input.get_mut(field) else {
            continue;
        };
        let Value::String(command) = value else {
            continue;
        };
        if !command.contains("__CROSSLANE_CHECKPOINT_") {
            continue;
        }
        if let Some(original) = exact_checkpoint_value(command) {
            // A complete source command is code, not one shell argument.
            *value = original;
        } else {
            // Reuse established shell quoting for embedded scalar capsules.
            // This temporary map only imports explicit checkpoint values; it
            // performs no sensitive-data detection and never reaches Kiro.
            *command = replace_checkpoint_capsules(command, &compatibility).0;
        }
    }
    if !compatibility.is_empty() {
        compatibility.restore_tool_input_for_tool(tool_name, input);
    }
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
    // Use the same semantic traversal for collection and replacement. Scanning
    // raw JSON first could learn a public fetch `credentials: "include"` value
    // as a secret and then redact its otherwise exempt decoded field.
    let mut collector = Redactor::new(&[]);
    collector.collecting = true;
    collector.text(&mut text.to_owned());
    let mut discovered = collector.discovered;
    discovered.sort_unstable();
    discovered.dedup();
    remember_literal_getters(session, &discovered);
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
    remember_literal_getters(session, &collector.discovered);
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

fn remember_literal_getters(session: Option<&RedactionSession>, discovered: &[String]) {
    if let Some(session) = session {
        for literal in discovered
            .iter()
            .filter(|text| named_getter_reference(text))
        {
            // An enclosing Basic-auth pair may become one marker. Retain the
            // getter-shaped password too, so future rounds cannot reclassify
            // that proven literal credential as a public environment reference.
            session.marker(Value::String(literal.clone()));
        }
    }
}

struct Redactor<'a> {
    known_secrets: &'a [&'a str],
    matcher: Option<AhoCorasick>,
    stats: RedactionStats,
    collecting: bool,
    discovered: Vec<String>,
    session: Option<RedactionSession>,
    shell_commands: Vec<String>,
    decode_auth_literals: bool,
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
            decode_auth_literals: true,
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
                    let previous_auth_literals = std::mem::replace(
                        &mut self.decode_auth_literals,
                        !shell::explicitly_non_posix(&tool.input),
                    );
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
                    self.decode_auth_literals = previous_auth_literals;
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
        if self.embedded_documents(value, shell_source) {
            return;
        }
        if self.collecting {
            self.discovered.extend(discover_text_secrets_with_context(
                value,
                shell_source,
                self.decode_auth_literals,
            ));
            return;
        }
        let (safe, count) = redact_text_impl_with_context(
            value,
            self.known_secrets,
            self.matcher.as_ref(),
            self.session.as_ref(),
            shell_source,
            self.decode_auth_literals,
        );
        if count > 0 {
            *value = self.session.as_ref().map_or_else(
                || safe.clone(),
                |session| session.remember_source_text(value, safe.clone()),
            );
            self.stats.redacted_values += count;
        }
    }

    fn embedded_documents(&mut self, value: &mut String, shell_source: bool) -> bool {
        let Some(first) = embedded_json::documents(value, 0).next() else {
            return false;
        };
        let original = value.clone();
        let auth = auth::scan(&original);
        let guards = secret_ranges_with_auth(
            &original,
            self.known_secrets,
            self.matcher.as_ref(),
            shell_source,
            self.decode_auth_literals,
            &auth,
        );
        let after_first = first.range.end;
        let boundaries = merge_ranges(guards.clone());
        let documents: Vec<_> = std::iter::once(first)
            .chain(embedded_json::documents(&original, after_first))
            .filter(|document| {
                // Keep enclosing or boundary-crossing credentials atomic.
                let first = boundaries.partition_point(|range| range.end <= document.range.start);
                let end = boundaries.partition_point(|range| range.start < document.range.end);
                first == end
                    || (boundaries[first].start > document.range.start
                        && boundaries[end - 1].end <= document.range.end)
            })
            .collect();
        if documents.is_empty() {
            return false;
        }
        let outside = |range: &Range<usize>| {
            let index = documents.partition_point(|document| document.range.end <= range.start);
            documents
                .get(index)
                .is_none_or(|document| document.range.start >= range.end)
        };
        // Classify surrounding text in its complete source context. Splitting
        // it first would lose shell boundaries and enclosing language fences.
        let ranges: Vec<_> = guards.into_iter().filter(outside).collect();
        if self.collecting {
            self.discovered.extend(checkpoint_secrets(&original));
            if self.decode_auth_literals {
                self.discovered.extend(
                    auth.literal_entries()
                        .filter(|(range, secret)| outside(range) && reusable_secret(secret))
                        .map(|(_, secret)| secret.to_owned()),
                );
            }
            self.discovered.extend(ranges.iter().filter_map(|range| {
                let secret = &original[range.clone()];
                reusable_secret(secret).then(|| secret.to_owned())
            }));
            for mut document in documents {
                self.data(&mut document.value, document.opaque);
            }
            return true;
        }
        let ranges = merge_ranges(ranges);
        self.stats.redacted_values += ranges.len();
        let mut replacements: Vec<_> = ranges
            .into_iter()
            .map(|range| {
                let replacement = range_replacement(
                    &original,
                    &range,
                    self.session.as_ref(),
                    self.decode_auth_literals.then_some(&auth),
                );
                (range, replacement)
            })
            .collect();
        for mut document in documents {
            let before = self.stats.redacted_values;
            self.data(&mut document.value, document.opaque);
            if self.stats.redacted_values > before {
                let safe = if document.opaque {
                    document
                        .value
                        .as_str()
                        .expect("opaque JSON string")
                        .to_owned()
                } else {
                    document.value.to_string()
                };
                replacements.push((document.range, safe));
            }
        }
        if replacements.is_empty() {
            return true;
        }
        replacements.sort_unstable_by_key(|(range, _)| range.start);
        let mut safe = String::with_capacity(original.len());
        let mut offset = 0;
        for (range, replacement) in replacements {
            safe.push_str(&original[offset..range.start]);
            safe.push_str(&replacement);
            offset = range.end;
        }
        safe.push_str(&original[offset..]);
        *value = self.session.as_ref().map_or_else(
            || safe.clone(),
            |session| session.remember_source_text(&original, safe.clone()),
        );
        true
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
                let fetch_options = auth::fetch_options(fields);
                let oauth_fields: Vec<String> = fields
                    .keys()
                    .filter(|name| auth::oauth_field(name, fields))
                    .cloned()
                    .collect();
                let original = std::mem::take(fields);
                for (mut name, mut child) in original {
                    let secret_field = !auth::public_field(&name, &child)
                        && !(fetch_options && name == "credentials")
                        && (sensitive
                            || sensitive_name(&name)
                            || oauth_fields.contains(&name)
                            || child
                                .as_str()
                                .is_some_and(|text| auth::credential_field(&name, text)));
                    if !secret_field {
                        for password in auth::basic_passwords(&name, &mut child) {
                            self.authentication_literal(password);
                        }
                    }
                    self.text(&mut name);
                    if !secret_field && auth::challenge_header(&name) {
                        self.challenge(&mut child);
                    } else {
                        self.data(&mut child, secret_field);
                    }
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
                let header = values.len() == 2
                    && values[0].as_str().is_some_and(|name| {
                        auth::header_name(name) && !auth::public_field(name, &values[1])
                    });
                let challenge =
                    values.len() == 2 && values[0].as_str().is_some_and(auth::challenge_header);
                for (index, child) in values.iter_mut().enumerate() {
                    if !sensitive && challenge && index == 1 {
                        self.challenge(child);
                    } else {
                        self.data(child, sensitive || (header && index == 1));
                    }
                }
            }
            Value::Null => {}
            Value::String(text) if sensitive && auth::environment_expression(text).is_some() => {
                let range =
                    auth::environment_expression(text).expect("checked environment expression");
                let ranges = literal_default_ranges(text, range);
                if self.collecting {
                    for range in ranges {
                        if reusable_secret(&text[range.clone()]) {
                            self.discovered.push(text[range].to_owned());
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
            Value::String(text)
                if sensitive
                    && ((structured_reference(text)
                        && !self.known_secrets.contains(&text.as_str()))
                        || marker_pattern().is_match(text)) =>
            {
                let ranges = exclude_references(text, std::iter::once(0..text.len()).collect())
                    .into_iter()
                    .filter(|range| !auth::is_scheme(text[range.clone()].trim()))
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

    fn challenge(&mut self, value: &mut Value) {
        if let Value::String(text) = value {
            // Preserve header context when the JSON traversal reaches only the
            // value, so public `Bearer realm=...` is not mistaken for a token.
            const PREFIX: &str = "WWW-Authenticate: ";
            let mut tagged = format!("{PREFIX}{text}");
            self.text(&mut tagged);
            if let Some(safe) = tagged.strip_prefix(PREFIX) {
                *text = safe.to_owned();
                return;
            }
        }
        self.data(value, false);
    }

    fn authentication_literal(&mut self, value: &mut Value) {
        if let Some(text) = value.as_str().filter(|text| named_getter_reference(text)) {
            // A structured Basic-auth tuple/object contains an actual password,
            // not source code that will evaluate a getter expression.
            if self.collecting {
                self.discovered.push(text.to_owned());
            } else {
                let marker = self.session.as_ref().map_or_else(
                    || REDACTED.to_owned(),
                    |session| session.marker(value.clone()),
                );
                *value = Value::String(marker);
                self.stats.redacted_values += 1;
            }
        } else {
            self.data(value, true);
        }
    }

    // JSON Schema property names, types, required names and structural keywords
    // must survive. Only human text and data-bearing default/example/const/enum
    // values are eligible for masking.
    fn schema(&mut self, value: &mut Value, sensitive_property: bool) {
        self.schema_property(value, sensitive_property, None);
    }

    fn schema_property(
        &mut self,
        value: &mut Value,
        sensitive_property: bool,
        property_name: Option<&str>,
    ) {
        match value {
            Value::Object(fields) => {
                for (name, child) in fields {
                    match name.as_str() {
                        "properties" | "patternProperties" | "$defs" | "definitions"
                        | "dependentSchemas" => {
                            if let Value::Object(properties) = child {
                                let basic_auth = property_name
                                    .is_some_and(auth::basic_auth_property)
                                    && (properties.contains_key("user")
                                        || properties.contains_key("username"));
                                for (property, schema) in properties {
                                    self.schema_property(
                                        schema,
                                        sensitive_property
                                            || (basic_auth && property == "pass")
                                            || (sensitive_name(property)
                                                && !auth::public_boolean_schema(property, schema)),
                                        Some(property),
                                    );
                                }
                            }
                        }
                        "prefixItems" | "items"
                            if child.is_array()
                                && property_name.is_some_and(auth::basic_auth_property) =>
                        {
                            for (index, schema) in child
                                .as_array_mut()
                                .expect("checked array")
                                .iter_mut()
                                .enumerate()
                            {
                                self.schema_property(
                                    schema,
                                    sensitive_property || index == 1,
                                    Some(if index == 1 { "password" } else { "username" }),
                                );
                            }
                        }
                        "examples" | "enum" if child.is_array() => {
                            for example in child.as_array_mut().expect("checked array") {
                                self.schema_data(example, sensitive_property, property_name);
                            }
                        }
                        "default" | "example" | "examples" | "enum" | "const" => {
                            self.schema_data(child, sensitive_property, property_name)
                        }
                        "description" | "title" | "$comment" => {
                            if let Value::String(text) = child {
                                self.text(text);
                            }
                        }
                        _ => {
                            if child.is_object() || child.is_array() {
                                self.schema_property(child, sensitive_property, property_name);
                            }
                        }
                    }
                }
            }
            Value::Array(values) => {
                for child in values {
                    self.schema_property(child, sensitive_property, property_name);
                }
            }
            _ => {}
        }
    }

    fn schema_data(&mut self, value: &mut Value, sensitive: bool, property_name: Option<&str>) {
        let Some(name) = property_name else {
            self.data(value, sensitive);
            return;
        };
        let sensitive = (sensitive && !auth::public_field(name, value))
            || value
                .as_str()
                .is_some_and(|text| auth::credential_field(name, text));
        if !sensitive {
            for password in auth::basic_passwords(name, value) {
                self.authentication_literal(password);
            }
            if auth::challenge_header(name) {
                self.challenge(value);
                return;
            }
        }
        if sensitive {
            self.authentication_literal(value);
        } else {
            self.data(value, false);
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
    // ADC takes a credential file path; the file's contents have their own
    // private_key/client_secret rules. This variable is not an inline secret.
    if trimmed == "GOOGLE_APPLICATION_CREDENTIALS" {
        return false;
    }
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
            | "dpop"
            | "clientassertion"
            | "authorizationcode"
            | "devicecode"
            | "oauthverifier"
            | "oauthsignature"
            | "oauthconsumerkey"
            | "samlresponse"
            | "xwsse"
            | "passphrase"
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
        "identitytoken",
        "registrytoken",
        "passphrase",
        "clientassertion",
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
        && !marker_pattern()
            .find(secret)
            .is_some_and(|matched| matched.as_str() == secret)
}

fn symbolic_pattern() -> &'static Regex {
    static SYMBOLIC: OnceLock<Regex> = OnceLock::new();
    regex(
        &SYMBOLIC,
        // Only named environment lookups without defaults or arbitrary call
        // arguments are references. Matching quotes and identifier boundaries
        // matter: a lookalike function or malformed literal is not exempt.
        concat!(
            r#"\$\{(?:(?i:env):)?[A-Za-z_][A-Za-z0-9_]*(?::[-+?=][^{}\r\n]*)?\}"#,
            r#"|\$_ENV\[\s*(?:"[A-Za-z_][A-Za-z0-9_]*"|'[A-Za-z_][A-Za-z0-9_]*')\s*\]"#,
            r#"|\$(?:(?i:env):)?[A-Za-z_][A-Za-z0-9_]*|%[A-Za-z_][A-Za-z0-9_]*%"#,
            r#"|\b(?:process|Bun|import\.meta)\.env(?:\.[A-Za-z_][A-Za-z0-9_]*|\[\s*(?:"[A-Za-z_][A-Za-z0-9_]*"|'[A-Za-z_][A-Za-z0-9_]*')\s*\])"#,
            r#"|\b(?:os\.(?:getenv|Getenv|environ\.get)|System\.getenv|(?:System\.)?Environment\.GetEnvironmentVariable|(?:std::)?env::var(?:_os)?|Deno\.env\.get|ENV\.fetch|getenv)\s*\(\s*(?:"[A-Za-z_][A-Za-z0-9_]*"|'[A-Za-z_][A-Za-z0-9_]*')\s*\)"#,
            r#"|\b(?:os\.environ|ENV)\[\s*(?:"[A-Za-z_][A-Za-z0-9_]*"|'[A-Za-z_][A-Za-z0-9_]*')\s*\]"#,
        ),
    )
}

fn symbolic_at_start() -> &'static Regex {
    static AT_START: OnceLock<Regex> = OnceLock::new();
    AT_START.get_or_init(|| {
        Regex::new(&format!(r"\A(?:{})", symbolic_pattern().as_str()))
            .expect("anchored environment reference")
    })
}

fn named_getter_reference(text: &str) -> bool {
    (!text.starts_with(['$', '%']) || text.starts_with("$_ENV["))
        && symbolic_at_start()
            .find(text)
            .is_some_and(|reference| reference.end() == text.len())
}

fn source_contexts(text: &str) -> Vec<Range<usize>> {
    static DECLARATION: OnceLock<Regex> = OnceLock::new();
    let declaration = regex(
        &DECLARATION,
        r"\A[ \t]*(?:(?:const|let|var|String|java\.lang\.String|final[ \t]+String)[ \t]+(?:mut[ \t]+)?[A-Za-z_][A-Za-z0-9_]*(?:[ \t]*:[ \t]*[A-Za-z_][A-Za-z0-9_:<> ,&]*)?[ \t]*=|\$[A-Za-z_][A-Za-z0-9_]*[ \t]*=)",
    );
    let mut contexts = Vec::new();
    let mut fence: Option<(char, usize, usize, bool)> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start_matches([' ', '\t']);
        if let Some(marker @ ('`' | '~')) = trimmed.chars().next() {
            let count = trimmed.chars().take_while(|ch| *ch == marker).count();
            if count >= 3 {
                let suffix = trimmed[count..].trim();
                if let Some((opened, length, start, source)) = fence {
                    if marker == opened && count >= length && suffix.is_empty() {
                        if source {
                            contexts.push(start..offset);
                        }
                        fence = None;
                    }
                } else {
                    let source = matches!(
                        suffix.to_ascii_lowercase().as_str(),
                        "go" | "golang"
                            | "java"
                            | "cs"
                            | "csharp"
                            | "c#"
                            | "rust"
                            | "rs"
                            | "ruby"
                            | "rb"
                            | "php"
                            | "javascript"
                            | "js"
                            | "typescript"
                            | "ts"
                    );
                    fence = Some((marker, count, offset + line.len(), source));
                }
                offset += line.len();
                continue;
            }
        }
        if fence.is_none() && declaration.is_match(line) {
            contexts.push(offset..offset + line.len());
        }
        offset += line.len();
    }
    if let Some((_, _, start, true)) = fence {
        contexts.push(start..text.len());
    }
    contexts
}

fn inside(ranges: &[Range<usize>], offset: usize) -> bool {
    let index = ranges.partition_point(|range| range.start <= offset);
    index > 0 && ranges[index - 1].contains(&offset)
}

fn intersects(ranges: &[Range<usize>], range: &Range<usize>) -> bool {
    let index = ranges.partition_point(|candidate| candidate.end <= range.start);
    ranges
        .get(index)
        .is_some_and(|candidate| candidate.start < range.end)
}

fn legacy_reference(reference: &str) -> bool {
    (reference.starts_with(['$', '%']) && !reference.starts_with("$_ENV["))
        || ["process.env", "os.getenv", "os.environ"]
            .iter()
            .any(|prefix| reference.starts_with(prefix))
}

// Existing explicit environment substitutions remain supported in structured
// parameters. Newly recognized source-language getters are only exempt in code:
// a JSON password/header string does not evaluate Go/Java/Rust/PHP source.
fn structured_reference(value: &str) -> bool {
    let trimmed = value.trim();
    let candidate = trimmed
        .split_once(char::is_whitespace)
        .filter(|(scheme, _)| auth::is_scheme(scheme))
        .map_or(trimmed, |(_, reference)| reference.trim());
    let mut offset = 0;
    let mut found = false;
    for reference in symbolic_pattern().find_iter(candidate) {
        if !legacy_reference(reference.as_str())
            || !candidate[offset..reference.start()]
                .chars()
                .all(|ch| ch.is_whitespace() || matches!(ch, ':' | '"' | '\''))
        {
            return false;
        }
        found = true;
        offset = reference.end();
    }
    found
        && candidate[offset..]
            .chars()
            .all(|ch| ch.is_whitespace() || matches!(ch, ':' | '"' | '\''))
}

fn may_contain_symbolic_reference(text: &str) -> bool {
    text.contains('$')
        || text.contains('%')
        || text.contains("env")
        || text.contains("Env")
        || text.contains("ENV")
}

fn exclude_references(text: &str, ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    if ranges.is_empty() {
        return ranges;
    }
    let protected = protected_reference_ranges(text);
    let mut safe = Vec::new();
    for range in ranges {
        if auth::only_references(&text[range.clone()])
            && symbolic_pattern()
                .find_iter(&text[range.clone()])
                .all(|reference| {
                    legacy_reference(reference.as_str()) || {
                        let start = range.start + reference.start();
                        let index = protected.partition_point(|candidate| candidate.start <= start);
                        index > 0 && protected[index - 1].end >= range.start + reference.end()
                    }
                })
        {
            continue;
        }
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

// Quoted getter defaults are literal data, even when they contain `$TOKEN`.
// Existing opaque references remain atomic on repeated redaction passes.
fn literal_default_ranges(text: &str, range: Range<usize>) -> Vec<Range<usize>> {
    if &text[range.clone()] == REDACTED {
        return Vec::new();
    }
    let mut output = Vec::new();
    let mut offset = range.start;
    for marker in marker_pattern().find_iter(&text[range.clone()]) {
        let start = range.start + marker.start();
        if offset < start {
            output.push(offset..start);
        }
        offset = range.start + marker.end();
    }
    if offset < range.end {
        output.push(offset..range.end);
    }
    output
}

fn protected_reference_ranges(text: &str) -> Vec<Range<usize>> {
    let mut protected = Vec::new();
    if may_contain_symbolic_reference(text) {
        let contexts = source_contexts(text);
        for reference in symbolic_pattern().find_iter(text) {
            if !legacy_reference(reference.as_str()) && !inside(&contexts, reference.start()) {
                continue;
            }
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
    discover_text_secrets_with_context(text, false, true)
}

fn discover_text_secrets_with_context(
    text: &str,
    shell_source: bool,
    decode_auth_literals: bool,
) -> Vec<String> {
    let mut secrets = checkpoint_secrets(text);
    let auth = auth::scan(text);
    if decode_auth_literals {
        secrets.extend(
            auth.literal_secrets()
                .filter(|secret| reusable_secret(secret))
                .map(str::to_owned),
        );
    }
    secrets.extend(
        secret_ranges_with_auth(text, &[], None, shell_source, decode_auth_literals, &auth)
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
    let auth = auth::scan(text);
    secret_ranges_with_auth(text, known_secrets, matcher, shell_source, true, &auth)
}

fn secret_ranges_with_auth(
    text: &str,
    known_secrets: &[&str],
    matcher: Option<&AhoCorasick>,
    shell_source: bool,
    decode_auth_literals: bool,
    auth: &auth::Scan,
) -> Vec<Range<usize>> {
    let mut ranges = Vec::<Range<usize>>::new();
    let mut literal_defaults = auth.literal_defaults.clone();
    if decode_auth_literals {
        literal_defaults.extend(auth.posix_literals.iter().cloned());
    }
    let contexts = if may_contain_symbolic_reference(text) {
        source_contexts(text)
    } else {
        Vec::new()
    };
    let source_getters: Vec<Range<usize>> = if may_contain_symbolic_reference(text) {
        symbolic_pattern()
            .find_iter(text)
            .filter(|reference| !legacy_reference(reference.as_str()))
            .map(|reference| reference.range())
            .collect()
    } else {
        Vec::new()
    };
    ranges.extend(auth.secrets.iter().cloned());
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
        for range in pattern
                .find_iter(text)
                .filter(|matched| {
                    matched.start() == 0
                        || !text[..matched.start()]
                            .chars()
                            .next_back()
                            .is_some_and(|ch| ch.is_ascii_alphanumeric())
                })
                .map(|matched| matched.range())
        {
            if intersects(&source_getters, &range) { literal_defaults.push(range.clone()); }
            ranges.push(range);
        }
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
                if auth::in_challenge(text, secret.start()) {
                    continue;
                }
                let basic = captures
                    .name("scheme")
                    .is_some_and(|scheme| scheme.as_str().eq_ignore_ascii_case("basic"));
                if !basic
                    && matches!(
                        secret.as_str().to_ascii_lowercase().as_str(),
                        "token"
                            | "tokens"
                            | "authentication"
                            | "authorization"
                            | "auth"
                            | "scheme"
                            | "header"
                    )
                {
                    // Public prose such as "Bearer authentication" has no
                    // credential syntax. Labelled Authorization/--token fields
                    // are still filtered below even with these weak values.
                    continue;
                }
                if !basic || auth::encoded_basic(secret.as_str()) {
                    if intersects(&source_getters, &secret.range()) {
                        literal_defaults.push(secret.range());
                    }
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
                if intersects(&source_getters, &secret.range()) {
                    literal_defaults.push(secret.range());
                }
                ranges.push(secret.range());
            }
        }
    }
    // Long opaque keys and Base64 data usually contain no assignment delimiter.
    // Avoid running the bounded identifier matcher at every byte in that case.
    for assignment in assignments(text) {
        let name = assignment.name;
        if auth.manages(assignment.start) {
            continue;
        }
        if sensitive_name(name) {
            if let Some(range) = auth::environment_literal_at(text, assignment.value_start) {
                literal_defaults.push(range);
                continue;
            }
        }
        let encoded_auth = auth::credential_field(
            name,
            text[assignment.value_start..]
                .trim_start_matches(['"', '\''])
                .split(['"', '\'', '\r', '\n', ',', ' ', '\t'])
                .next()
                .unwrap_or(""),
        );
        if sensitive_name(name)
            || encoded_auth
            || auth::oauth_assignment(text, name, assignment.value_start)
        {
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
            } else if let Some(context) = auth::delimited_context(text, assignment.value_start) {
                context
            } else if whole_line {
                ValueContext::Line
            } else {
                ValueContext::Json
            };
            if let Some(range) = value_range(text, assignment.value_start, context, name) {
                if !auth::public_assignment(name, &text[range.clone()]) {
                    if decode_auth_literals
                        && (text[assignment.value_start..].starts_with('\'')
                            || text[assignment.value_start..].starts_with("$'"))
                    {
                        literal_defaults.push(range.clone());
                    }
                    if matches!(
                        context,
                        ValueContext::Query | ValueContext::Connection | ValueContext::Shell
                    ) || ["\"", "'", "\\\"", "\\'", "|", ">"]
                        .iter()
                        .any(|delimiter| text[assignment.value_start..].starts_with(delimiter))
                    {
                        for reference in symbolic_pattern().find_iter(&text[range.clone()]) {
                            if named_getter_reference(reference.as_str()) {
                                literal_defaults.push(
                                    range.start + reference.start()..range.start + reference.end(),
                                );
                            }
                        }
                    }
                    if expression_value_range(text, assignment.value_start, context).as_ref()
                        == Some(&range)
                    {
                        if let Some(literal) = reference_expression_literal(text, range.clone())
                            .filter(|_| inside(&contexts, range.start))
                        {
                            // A fully recognized getter/default expression has
                            // public operators and quote delimiters. Keep them
                            // usable; its marker represents just the credential.
                            literal_defaults.push(literal);
                            continue;
                        }
                        // Quoted defaults/concatenated strings are literal data,
                        // including strings that happen to spell `$TOKEN`.
                        literal_defaults.extend(expression_literals(text, range.clone()));
                    }
                    ranges.push(range);
                }
            }
        }
    }
    if text.contains("--") {
        for captures in
            regex(&FLAG, r"--(?P<name>[A-Za-z_][A-Za-z0-9_.-]{0,127})[ \t]+").captures_iter(text)
        {
            let name = captures.name("name").expect("flag name");
            if sensitive_name(name.as_str()) && !auth.manages(name.start()) {
                let start = captures.get(0).expect("flag").end();
                if let Some(range) = value_range(text, start, ValueContext::Shell, name.as_str()) {
                    if decode_auth_literals
                        && shell::literal_word_fragment(
                            &text[start..range.end],
                            range.start - start..range.end - start,
                        )
                        .is_some()
                    {
                        literal_defaults.push(range.clone());
                    }
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
            if intersects(&source_getters, range) {
                return true;
            }
            // A complete reference spelling already identified as an actual
            // literal credential takes precedence. Substrings that merely match
            // part of an environment variable's name remain protected below.
            if protected.iter().any(|reference| reference == range) {
                return true;
            }
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
    for range in literal_defaults {
        safe.extend(literal_default_ranges(text, range));
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
    decode_auth_literals: bool,
) -> (String, usize) {
    let auth = auth::scan(text);
    let ranges = secret_ranges_with_auth(
        text,
        known_secrets,
        matcher,
        shell_source,
        decode_auth_literals,
        &auth,
    );
    replace_ranges_impl(
        text,
        ranges,
        session,
        (!shell_source && decode_auth_literals).then_some(&auth),
    )
}

fn replace_ranges(
    text: &str,
    ranges: Vec<Range<usize>>,
    session: Option<&RedactionSession>,
) -> (String, usize) {
    replace_ranges_impl(text, ranges, session, None)
}

fn replace_ranges_impl(
    text: &str,
    ranges: Vec<Range<usize>>,
    session: Option<&RedactionSession>,
    auth: Option<&auth::Scan>,
) -> (String, usize) {
    let merged = merge_ranges(ranges);
    let mut safe = String::with_capacity(text.len());
    let mut offset = 0;
    for range in &merged {
        safe.push_str(&text[offset..range.start]);
        safe.push_str(&range_replacement(text, range, session, auth));
        offset = range.end;
    }
    safe.push_str(&text[offset..]);
    (safe, merged.len())
}

fn merge_ranges(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.retain(|range| range.start < range.end);
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
    merged
}

fn range_replacement(
    text: &str,
    range: &Range<usize>,
    session: Option<&RedactionSession>,
    auth: Option<&auth::Scan>,
) -> String {
    if let Some(session) = session {
        let literal = auth
            .and_then(|auth| auth.literal(range))
            .unwrap_or(&text[range.clone()]);
        if literal != &text[range.clone()] {
            // Keep both spellings private across continuations, as the
            // shell-history normalization path already does. A later raw
            // tool result may repeat the escaped source spelling.
            session.marker(Value::String(text[range.clone()].to_owned()));
        }
        session.marker(Value::String(literal.to_owned()))
    } else {
        REDACTED.to_owned()
    }
}

#[derive(Clone, Copy)]
enum ValueContext {
    Line,
    Json,
    Shell,
    Query,
    Connection,
}

impl ValueContext {
    fn ends_at(self, ch: char) -> bool {
        match self {
            Self::Line => matches!(ch, '\r' | '\n'),
            Self::Json => matches!(ch, '\r' | '\n' | ',' | '}' | ']' | '"' | '\''),
            Self::Shell => ch.is_whitespace() || matches!(ch, ';' | '&' | '|' | '"' | '\'' | '`'),
            Self::Query => ch.is_whitespace() || matches!(ch, '&' | '#' | '"' | '\'' | '`'),
            Self::Connection => matches!(ch, ';' | '\r' | '\n' | '"' | '\''),
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
    if let Some(range) = expression_value_range(text, start, context) {
        return Some(range);
    }
    // A pure reference is not its runtime value. Stop at its end so a shell
    // command following it survives; mixed expressions were handled above.
    if let Some(reference) = symbolic_at_start().find(tail) {
        return Some(start..start + reference.end());
    }
    if let Some(rest) = tail.strip_prefix(REDACTED) {
        if rest.is_empty() || rest.starts_with(|ch| context.ends_at(ch)) || rest.starts_with(" #") {
            return None;
        }
    }
    // JSON nested in a quoted command often has escaped quote delimiters.
    if matches!(context, ValueContext::Connection) && tail.starts_with('{') {
        let mut offset = 1;
        while let Some(end) = tail[offset..].find('}') {
            offset += end;
            if tail[offset..].starts_with("}}") {
                offset += 2;
            } else {
                return Some(start + 1..start + offset);
            }
        }
        return Some(start + 1..text.len());
    }
    for delimiter in ["\"\"\"", "'''", "\\\"", "\\'", "$'", "\"", "'"] {
        if let Some(inner) = tail.strip_prefix(delimiter) {
            let content_start = start + delimiter.len();
            let closing = if delimiter == "$'" { "'" } else { delimiter };
            let mut offset = 0;
            while offset < inner.len() {
                if inner[offset..].starts_with(closing) {
                    if closing.len() == 1 && inner[offset + 1..].starts_with(closing) {
                        offset += 2;
                        continue;
                    }
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
    if matches!(context, ValueContext::Shell) {
        return auth::shell_value_range(text, start);
    }
    let end = tail.find(|ch| context.ends_at(ch)).unwrap_or(tail.len());
    let raw = &tail[..end];
    let end = raw.find(" #").unwrap_or(raw.len());
    let length = raw[..end].trim_end().len();
    (length > 0).then_some(start..start + length)
}

fn expression_value_range(text: &str, start: usize, context: ValueContext) -> Option<Range<usize>> {
    let tail = text.get(start..)?;
    if let Some(reference) = symbolic_at_start().find(tail) {
        let raw_rest = &tail[reference.end()..];
        let rest = raw_rest.trim_start_matches([' ', '\t']);
        let same_argument =
            !matches!(context, ValueContext::Shell) || !raw_rest.starts_with(char::is_whitespace);
        return (same_argument
            && (rest.starts_with(['+', '-', '*', '/', '.', '?', '|', '&'])
                || rest.starts_with("or ")
                || rest.starts_with("and ")))
        .then(|| expression_range(text, start, context));
    }
    static CALL: OnceLock<Regex> = OnceLock::new();
    // Commas and quotes inside an unquoted call with default arguments belong
    // to the expression. They must not end it before a literal credential.
    regex(&CALL, r"\A[A-Za-z_$][A-Za-z0-9_$.:]*[ \t]*\(")
        .is_match(tail)
        .then(|| expression_range(text, start, context))
}

fn expression_literals(text: &str, range: Range<usize>) -> Vec<Range<usize>> {
    let mut literals = Vec::new();
    let mut offset = range.start;
    let mut references = symbolic_pattern()
        .find_iter(&text[range.clone()])
        .peekable();
    while offset < range.end {
        while references
            .peek()
            .is_some_and(|reference| range.start + reference.end() <= offset)
        {
            references.next();
        }
        if let Some(reference) = references
            .peek()
            .filter(|reference| range.start + reference.start() == offset)
        {
            offset = range.start + reference.end();
            continue;
        }
        let ch = text[offset..].chars().next().expect("character boundary");
        offset += ch.len_utf8();
        if matches!(ch, '"' | '\'' | '`') {
            let start = offset;
            let mut escaped = false;
            while offset < range.end {
                let next = text[offset..].chars().next().expect("character boundary");
                if !escaped && next == ch {
                    break;
                }
                escaped = !escaped && next == '\\';
                offset += next.len_utf8();
            }
            literals.push(start..offset);
            if offset < range.end {
                offset += ch.len_utf8();
            }
        }
    }
    literals
}

fn reference_expression_literal(text: &str, range: Range<usize>) -> Option<Range<usize>> {
    let expression = &text[range.clone()];
    let reference = symbolic_at_start().find(expression)?;
    if !named_getter_reference(reference.as_str()) {
        return None;
    }
    static DEFAULT: OnceLock<Regex> = OnceLock::new();
    let pattern = DEFAULT.get_or_init(|| {
        let literal = r#"(?:"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*')"#;
        Regex::new(&format!(
            r#"\A(?:\s*(?:\?\?|\|\||\?:|\+|or\b)\s*(?P<value>{literal})\s*|\.(?:unwrap_or|concat)\(\s*(?P<call>{literal})\s*\)|\.unwrap_or_else\(\s*\|\|\s*(?P<closure>{literal})\s*\))\s*\z"#
        )).expect("literal getter fallback or concatenation")
    });
    let captures = pattern.captures(&expression[reference.end()..])?;
    let literal = captures
        .name("value")
        .or_else(|| captures.name("call"))
        .or_else(|| captures.name("closure"))?;
    let start = range.start + reference.end() + literal.start() + 1;
    Some(start..range.start + reference.end() + literal.end() - 1)
}

/// Bound a labelled expression without evaluating it. Its nested calls and
/// string literals belong to the value; only a top-level delimiter ends it.
/// Malformed/unclosed expressions retain the conservative mask-to-end policy.
fn expression_range(text: &str, start: usize, context: ValueContext) -> Range<usize> {
    let mut nesting = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    for (relative, ch) in text[start..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if let Some(delimiter) = quote {
            if ch == '\\' {
                escaped = true;
            } else if ch == delimiter {
                quote = None;
            }
            continue;
        }
        if nesting.is_empty() && context.ends_at(ch) && !matches!(ch, '"' | '\'' | '`') {
            return start..start + relative;
        }
        match ch {
            '"' | '\'' | '`' => quote = Some(ch),
            '(' => nesting.push(')'),
            '[' => nesting.push(']'),
            '{' => nesting.push('}'),
            ')' | ']' | '}' if nesting.pop() != Some(ch) => return start..text.len(),
            _ => {}
        }
    }
    start..text.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[cfg(feature = "crs")]
    #[test]
    fn transmitted_redactions_require_a_known_reference_on_the_actual_wire() {
        let session = RedactionSession::new();
        let masked = session.redact_text("token=notice-fixture-secret", &[]);
        assert!(!session.has_transmitted_redactions());
        for value in [
            json!({"content":"token=$CLOUD_TOKEN"}),
            json!({"content":"__CROSSLANE_SECRET_00000000000000000000000000000000_0__"}),
            json!({"content":"[Gateway credential references] __CROSSLANE_SECRET_"}),
        ] {
            session.record_transmitted_payload(&value);
            assert!(!session.has_transmitted_redactions());
        }
        session
            .clone()
            .record_transmitted_payload(&json!({"nested":[{"content":masked}]}));
        assert!(session.has_transmitted_redactions());
        session.record_transmitted_payload(&json!({"content":"later clean continuation"}));
        assert!(session.has_transmitted_redactions());
        let disabled = RedactionSession::with_enabled(false);
        disabled.record_transmitted_payload(&json!({"content":masked}));
        assert!(!disabled.has_transmitted_redactions());
    }

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
            let executable = if reference.starts_with('%') || reference.starts_with("$env:") {
                "curl.exe"
            } else {
                "curl"
            };
            let command = format!(
                "{executable} -H \"Authorization: Bearer {reference}\" https://example.invalid/"
            );
            if reference.starts_with(['$', '%']) {
                assert_eq!(session.redact_text(&command, &[]), command);
                assert_eq!(redact_text(&command, &[]), command);
            }
            let mut value = json!({"token":reference,"headers":{"Authorization":format!("Bearer {reference}")}});
            let original = value.clone();
            let mut redactor = Redactor::new(&[]);
            redactor.session = Some(session.clone());
            redactor.data(&mut value, false);
            assert_eq!(value, original);
            assert!(session.is_empty());
        }
        let command = "API_TOKEN=$TOKEN curl --token=${TOKEN} --password \"$PASSWORD\"";
        let session = RedactionSession::new();
        assert_eq!(session.redact_text(command, &[]), command);
    }

    #[test]
    fn named_environment_getters_do_not_become_credentials_or_seed_repeated_matches() {
        for reference in [
            "os.Getenv(\"API_TOKEN\")",
            "System.getenv(\"API_TOKEN\")",
            "Environment.GetEnvironmentVariable(\"API_TOKEN\")",
            "System.Environment.GetEnvironmentVariable(\"API_TOKEN\")",
            "std::env::var(\"API_TOKEN\")",
            "env::var_os(\"API_TOKEN\")",
            "Deno.env.get('API_TOKEN')",
            "Bun.env.API_TOKEN",
            "Bun.env['API_TOKEN']",
            "import.meta.env.API_TOKEN",
            "ENV['API_TOKEN']",
            "ENV.fetch('API_TOKEN')",
            "getenv('API_TOKEN')",
            "$_ENV['API_TOKEN']",
        ] {
            let session = RedactionSession::new();
            let language = if reference.starts_with("ENV") {
                "ruby"
            } else if reference.starts_with("getenv") || reference.starts_with("$_ENV") {
                "php"
            } else if reference.contains("env::") {
                "rust"
            } else if reference.starts_with("os.Getenv") {
                "go"
            } else if reference.starts_with("System.getenv") {
                "java"
            } else if reference.contains("Environment.GetEnvironmentVariable") {
                "csharp"
            } else {
                "javascript"
            };
            let variable = if language == "php" { "$token" } else { "token" };
            let code = format!("```{language}\n{variable} = {reference}\n```");
            for source in [code.clone(), json!({"code":code}).to_string()] {
                assert_eq!(session.redact_text(&source, &[]), source, "{reference}");
            }
            assert!(
                session.is_empty(),
                "reference seeded a credential: {reference}"
            );
            // A real value obtained from a getter is still private when a tool
            // returns it. Only the expression, never its runtime output, is exempt.
            let source = json!({"token":"opaque-runtime-value"}).to_string();
            let safe = session.redact_text(&source, &[]);
            assert!(!safe.contains("opaque-runtime-value"));
            assert_eq!(session.restore_text(&safe), source);
            let repeated = session.redact_text("repeat opaque-runtime-value", &[]);
            assert!(!repeated.contains("opaque-runtime-value"));
        }
    }

    #[test]
    fn getter_defaults_concatenation_and_lookalikes_do_not_exempt_literal_credentials() {
        for expression in [
            "os.Getenv(\"API_TOKEN\") + \"opaque-inline-secret\"",
            "System.getenv(\"API_TOKEN\") ?? \"opaque-inline-secret\"",
            "Environment.GetEnvironmentVariable(\"API_TOKEN\") ?? \"opaque-inline-secret\"",
            "std::env::var(\"API_TOKEN\").unwrap_or(\"opaque-inline-secret\")",
            "env::var_os(\"API_TOKEN\").unwrap_or_else(|| \"opaque-inline-secret\")",
            "Deno.env.get('API_TOKEN') || 'opaque-inline-secret'",
            "Bun.env.API_TOKEN ?? 'opaque-inline-secret'",
            "import.meta.env.API_TOKEN || 'opaque-inline-secret'",
            "ENV.fetch('API_TOKEN', 'opaque-inline-secret')",
            "getenv('API_TOKEN') ?: 'opaque-inline-secret'",
            "$_ENV['API_TOKEN'] ?? 'opaque-inline-secret'",
            "os.getenv('API_TOKEN') or 'opaque-inline-secret'",
            "process.env.API_TOKEN + 'opaque-inline-secret'",
            "customgetenv('opaque-inline-secret')",
            "getenv('opaque-inline-secret\")",
            "os.Getenv(\"API_TOKEN\").concat('$INLINE_SECRET')",
        ] {
            for source in [
                format!("password = {expression}"),
                format!("options = {{password: {expression}}}"),
                json!({"password":expression}).to_string(),
            ] {
                let session = RedactionSession::new();
                let safe = session.redact_text(&source, &[]);
                assert!(!safe.contains("opaque-inline-secret"), "{safe}");
                assert!(!safe.contains("INLINE_SECRET"), "{safe}");
                assert_ne!(safe, source, "{source}");
                assert_eq!(session.restore_text(&safe), source);
                assert_eq!(session.redact_text(&safe, &[]), safe);
            }
        }
        for source in [
            "curl --token $API_TOKEN --password $PASSWORD https://example.invalid/",
            "TOKEN=${API_TOKEN} curl -H \"Authorization: Bearer $API_TOKEN\" https://example.invalid/",
        ] {
            assert_eq!(redact_text(source, &[]), source);
        }
    }

    #[test]
    fn authentication_literals_cannot_impersonate_environment_getters() {
        for reference in [
            "os.Getenv(\"API_TOKEN\")",
            "System.getenv(\"API_TOKEN\")",
            "std::env::var(\"API_TOKEN\")",
            "Environment.GetEnvironmentVariable(\"API_TOKEN\")",
            "process.env.API_TOKEN",
            "os.getenv(\"API_TOKEN\")",
            "getenv(\"API_TOKEN\")",
            "$_ENV[\"API_TOKEN\"]",
        ] {
            for source in [
                format!("curl --oauth2-bearer '{reference}' https://example.invalid/"),
                format!("curl -u 'user:{reference}' https://example.invalid/"),
                format!("HTTPBasicAuth('user', '{reference}')"),
                format!("machine example.invalid login user password {reference}\n"),
                format!("password = '{reference}'"),
            ] {
                let session = RedactionSession::new();
                let safe = session.redact_text(&source, &[]);
                assert!(!safe.contains(reference), "{safe}");
                assert_eq!(session.restore_text(&safe), source);
                // Once learned from an actual literal credential, its exact
                // spelling must also stay private in subsequent data/results.
                let repeated = session.redact_text(reference, &[]);
                assert_ne!(repeated, reference, "{reference}");
                let data = json!({"password":reference}).to_string();
                let safe = session.redact_text(&data, &[]);
                let value: Value = serde_json::from_str(&safe).unwrap();
                assert_ne!(value["password"], reference);
                assert_eq!(session.restore_text(&safe), data);
            }
        }
    }

    #[test]
    fn structured_basic_auth_passwords_are_literals_even_when_they_spell_getters() {
        for reference in [
            "os.Getenv(\"TOKEN\")",
            "System.getenv(\"TOKEN\")",
            "getenv('TOKEN')",
        ] {
            for original in [
                json!({"auth":["user",reference]}),
                json!({"basic_auth":["user",reference]}),
                json!({"auth":{"user":"user","pass":reference}}),
                json!({"auth":{"user":"user","pass":reference,"password":reference,"api_key":reference,"has_password":true}}),
                json!({"proxyAuth":{"username":"user","pass":reference}}),
            ] {
                let session = RedactionSession::new();
                let source = original.to_string();
                let safe = session.redact_text(&source, &[]);
                assert_ne!(safe, source);
                let mut returned: Value = serde_json::from_str(&safe).unwrap();
                session.restore_tool_input(&mut returned);
                assert_eq!(returned, original);
                assert_ne!(session.redact_text(reference, &[]), reference);
            }
        }
    }

    #[test]
    fn source_getters_inside_structured_passwords_and_schema_defaults_are_literal() {
        for reference in [
            "os.Getenv(\"TOKEN\")",
            "System.getenv(\"TOKEN\")",
            "std::env::var(\"TOKEN\")",
            "getenv('TOKEN')",
        ] {
            let session = RedactionSession::new();
            let original = json!({"password":reference,"headers":{"Authorization":format!("Bearer {reference}")}});
            let mut returned: Value =
                serde_json::from_str(&session.redact_text(&original.to_string(), &[])).unwrap();
            assert_ne!(returned, original);
            assert!(marker_pattern().is_match(returned["password"].as_str().unwrap()));
            assert!(
                marker_pattern().is_match(returned["headers"]["Authorization"].as_str().unwrap())
            );
            session.restore_tool_input(&mut returned);
            assert_eq!(returned, original);
            for source in [
                format!("PASSWORD={reference}"),
                format!("token: {reference}"),
                format!("```yaml\npassword: {reference}\n```"),
                format!("password: |\n  ```rust\n  {reference}\n  ```"),
            ] {
                let session = RedactionSession::new();
                let safe = session.redact_text(&source, &[]);
                assert!(!safe.contains(reference), "{safe}");
                assert_eq!(session.restore_text(&safe), source);
            }
        }
        let session = RedactionSession::new();
        let mut redactor = Redactor::new(&[]);
        redactor.session = Some(session.clone());
        let reference = "System.getenv(\"TOKEN\")";
        let mut schema = json!({"type":"object","properties":{"password":{"type":"string","default":reference}}});
        redactor.schema(&mut schema, false);
        assert_eq!(schema["properties"]["password"]["type"], "string");
        assert_ne!(schema["properties"]["password"]["default"], reference);
        let mut input = json!({"password":schema["properties"]["password"]["default"]});
        session.restore_tool_input(&mut input);
        assert_eq!(input["password"], reference);
    }

    #[test]
    fn getter_default_markers_represent_the_literal_without_operators_or_quotes() {
        for source in [
            "var token = os.Getenv(\"TOKEN\") + \"$LITERAL_DEFAULT\"",
            "String token = System.getenv(\"TOKEN\") + \"$LITERAL_DEFAULT\"",
            "let token = std::env::var(\"TOKEN\").unwrap_or(\"$LITERAL_DEFAULT\")",
        ] {
            let session = RedactionSession::new();
            let safe = session.redact_text(source, &[]);
            let reference = marker_pattern().find(&safe).unwrap().as_str();
            assert_eq!(safe, source.replace("$LITERAL_DEFAULT", reference));
            let mut input = json!({"token":reference});
            session.restore_tool_input(&mut input);
            assert_eq!(input["token"], "$LITERAL_DEFAULT");
            assert_eq!(session.restore_text(&safe), source);
        }
    }

    #[test]
    fn source_reference_exemptions_never_override_cloud_formats_or_known_credentials() {
        for secret in [
            "AKIA1234567890ABCDEF",
            "AKIDabcdefghijklmnopqrstuvwx",
            "LTAIabcdefghijklmnopqrst",
        ] {
            let source = format!("var value = os.Getenv(\"{secret}\")");
            let session = RedactionSession::new();
            let safe = session.redact_text(&source, &[]);
            assert!(!safe.contains(secret), "{safe}");
            assert_eq!(session.restore_text(&safe), source);
        }
        let source = "var value = os.Getenv(\"fixture_known\")";
        let session = RedactionSession::new();
        let safe = session.redact_text(source, &["fixture_known"]);
        assert!(!safe.contains("fixture_known"));
        assert_eq!(session.restore_text(&safe), source);
        for source in [
            r#"const url = 'https://example.invalid/?token=os.Getenv("TOKEN")';"#,
            r#"const dsn = 'Server=localhost;Password=os.Getenv("TOKEN");Database=db';"#,
        ] {
            let session = RedactionSession::new();
            let safe = session.redact_text(source, &[]);
            assert!(!safe.contains(r#"os.Getenv("TOKEN")"#), "{safe}");
            assert_eq!(session.restore_text(&safe), source);
        }
    }

    #[test]
    fn ambiguous_business_fields_and_weak_credentials_remain_private() {
        for (name, literal) in [
            ("key", "Enter"),
            ("token", "authentication."),
            ("password", "true"),
            ("secret", "include"),
            ("cache_key", "product:42"),
            ("密码", "至少8位字符"),
        ] {
            let session = RedactionSession::new();
            let source = json!({name:literal}).to_string();
            let safe = session.redact_text(&source, &[]);
            assert!(!safe.contains(literal), "{safe}");
            assert_eq!(session.restore_text(&safe), source);
        }
        let session = RedactionSession::new();
        session.redact_text("password=test", &[]);
        let source = "test and prefix-test-suffix";
        let safe = session.redact_text(source, &[]);
        assert!(!safe.contains("test"));
        assert_eq!(session.restore_text(&safe), source);
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
        let source = "curl -H \"Authorization: Bearer ${API_TOKEN:-ksk_literal_fallback}\" https://example.invalid/";
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
