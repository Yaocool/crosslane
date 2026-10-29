//! Bounded, content-free evidence from the actual outgoing payload.

use std::collections::{BTreeMap, BTreeSet};

use base64::Engine;
use serde::Serialize;
use serde_json::Value;

use super::{marker_pattern, RedactionSession, SessionState};

const MAX_SOURCES: usize = 32;
const MAX_PATH_BYTES: usize = 512;

/// Contains only fixed labels, structural indexes and occurrence counts.
/// No values, hashes, markers, tool names or user-controlled field names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RedactionSource {
    pub source: &'static str,
    pub path: String,
    pub rules: BTreeSet<&'static str>,
    pub redactions: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RedactionReport {
    pub enabled: bool,
    pub triggered: bool,
    pub redacted_values: usize,
    pub sources: Vec<RedactionSource>,
    pub sources_truncated: bool,
}

impl RedactionSession {
    /// Inspect final wire data without changing notice state. Budget-only
    /// mappings and credentials omitted by compaction cannot trigger this.
    pub fn outgoing_redaction_report(
        &self,
        payload: &Value,
        protected_history_messages: usize,
    ) -> RedactionReport {
        let mut report = RedactionReport {
            enabled: self.enabled,
            triggered: false,
            redacted_values: 0,
            sources: Vec::new(),
            sources_truncated: false,
        };
        if !self.enabled {
            return report;
        }
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.by_marker.is_empty() {
            return report;
        }
        let mut inspector = Inspector {
            state: &state,
            report: &mut report,
            sources: BTreeMap::new(),
            protected_history_messages,
        };
        inspector.visit(payload, "", "payload", false);
        inspector.report.sources = inspector.sources.into_values().collect();
        report
    }

    pub(super) fn record_text_rule(&self, text: &str, rule: &'static str) {
        if !text.contains("__CROSSLANE_SECRET_") {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        for matched in marker_pattern().find_iter(text) {
            if state.by_marker.contains_key(matched.as_str()) {
                state
                    .rules
                    .entry(matched.as_str().to_owned())
                    .or_default()
                    .insert(rule);
            }
        }
    }

    pub(super) fn record_value_rule(&self, value: &Value, rule: &'static str) {
        match value {
            Value::String(text) => self.record_text_rule(text, rule),
            Value::Object(fields) => {
                for value in fields.values() {
                    self.record_value_rule(value, rule);
                }
            }
            Value::Array(values) => {
                for value in values {
                    self.record_value_rule(value, rule);
                }
            }
            _ => {}
        }
    }
}

struct Inspector<'a> {
    state: &'a SessionState,
    report: &'a mut RedactionReport,
    sources: BTreeMap<String, RedactionSource>,
    protected_history_messages: usize,
}

impl Inspector<'_> {
    fn text(&mut self, text: &str, path: &str, source: &'static str) {
        if !text.contains("__CROSSLANE_SECRET_") {
            return;
        }
        for matched in marker_pattern().find_iter(text) {
            if !self.state.by_marker.contains_key(matched.as_str()) {
                continue;
            }
            self.report.triggered = true;
            self.report.redacted_values += 1;
            self.report.sources_truncated |= path.contains("/...");
            if !self.sources.contains_key(path) && self.sources.len() >= MAX_SOURCES {
                self.report.sources_truncated = true;
                continue;
            }
            let entry = self
                .sources
                .entry(path.to_owned())
                .or_insert_with(|| RedactionSource {
                    source,
                    path: path.to_owned(),
                    rules: BTreeSet::new(),
                    redactions: 0,
                });
            entry.redactions += 1;
            if let Some(rules) = self
                .state
                .rules
                .get(matched.as_str())
                .filter(|rules| !rules.is_empty())
            {
                entry.rules.extend(rules);
            } else {
                entry.rules.insert("credential_value");
            }
        }
    }

    fn path(&self, prefix: &str, segment: &str) -> String {
        let path = format!("{prefix}/{segment}");
        if path.len() > MAX_PATH_BYTES {
            format!("{}/...", &path[..MAX_PATH_BYTES - 4])
        } else {
            path
        }
    }

    fn visit(&mut self, value: &Value, path: &str, source: &'static str, protected: bool) {
        match value {
            Value::String(text) => self.text(text, path, source),
            Value::Array(values) => {
                for (index, value) in values.iter().enumerate() {
                    let child = self.path(path, &index.to_string());
                    let protected = protected
                        || (path == "/conversationState/history"
                            && index < self.protected_history_messages);
                    self.visit(value, &child, source, protected);
                }
            }
            Value::Object(fields) => {
                // Native images are opaque. Only supported text documents are
                // decoded, matching the redactor's existing attachment boundary.
                if source == "document" {
                    if let Some(name) = fields.get("name").and_then(Value::as_str) {
                        let child = self.path(path, "name");
                        self.text(name, &child, "document_name");
                    }
                    if fields
                        .get("format")
                        .and_then(Value::as_str)
                        .is_some_and(|format| {
                            matches!(
                                format.to_ascii_lowercase().as_str(),
                                "txt" | "md" | "csv" | "html"
                            )
                        })
                    {
                        if let Some(text) = fields
                            .get("source")
                            .and_then(|source| source.get("bytes"))
                            .and_then(Value::as_str)
                            .and_then(|text| {
                                base64::engine::general_purpose::STANDARD.decode(text).ok()
                            })
                            .and_then(|bytes| String::from_utf8(bytes).ok())
                        {
                            let child = self.path(path, "source/bytes");
                            self.text(&text, &child, "document_text");
                        }
                    }
                    return;
                }
                for (index, (name, value)) in fields.iter().enumerate() {
                    let key_path = self.path(path, &format!("field[{index}]/@key"));
                    self.text(name, &key_path, source);
                    let segment = if protocol_field(name) {
                        name.clone()
                    } else {
                        format!("field[{index}]")
                    };
                    let child = self.path(path, &segment);
                    let user = matches!(
                        source,
                        "current_user_text" | "history_user_text" | "system_prompt"
                    );
                    let history =
                        path.starts_with("/conversationState/history/") && source == "payload";
                    let category = match name.as_str() {
                        "userInputMessage" if history && protected => "system_prompt",
                        "userInputMessage" if history => "history_user_text",
                        "userInputMessage" if path == "/conversationState/currentMessage" => {
                            "current_user_text"
                        }
                        "assistantResponseMessage" if history => "history_assistant_text",
                        "toolResults" if user => "tool_result",
                        "toolUses" if source == "history_assistant_text" => "tool_input",
                        "toolSpecification" if user => "tool_definition",
                        "description" if source == "tool_definition" => "tool_description",
                        "inputSchema" if source == "tool_definition" => "tool_schema",
                        "documents" if user => "document",
                        "envState" if user => "environment",
                        "additionalModelRequestFields" if path.is_empty() => {
                            "additional_model_fields"
                        }
                        "query" if path == "/params/arguments" => "web_search",
                        _ => source,
                    };
                    self.visit(value, &child, category, protected);
                }
            }
            _ => {}
        }
    }
}

fn protocol_field(name: &str) -> bool {
    matches!(
        name,
        "conversationState"
            | "currentMessage"
            | "history"
            | "userInputMessage"
            | "assistantResponseMessage"
            | "userInputMessageContext"
            | "content"
            | "text"
            | "toolResults"
            | "toolUses"
            | "input"
            | "tools"
            | "toolSpecification"
            | "description"
            | "inputSchema"
            | "json"
            | "properties"
            | "items"
            | "prefixItems"
            | "default"
            | "example"
            | "examples"
            | "enum"
            | "const"
            | "documents"
            | "name"
            | "source"
            | "bytes"
            | "envState"
            | "currentWorkingDirectory"
            | "operatingSystem"
            | "additionalModelRequestFields"
            | "params"
            | "arguments"
            | "query"
    )
}
