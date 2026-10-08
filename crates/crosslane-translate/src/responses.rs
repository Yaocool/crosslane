//! OpenAI Responses input, normalized into the shared OpenAI/Kiro path.
//!
//! Protocol reference: [OpenAI Responses](https://developers.openai.com/api/reference/resources/responses).
//! Hosted tools are rejected before upstream execution. Ephemeral state is handled
//! at the HTTP boundary, after request authentication.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    validate_openai, OpenAiMessage, OpenAiRequest, OpenAiTool, ThinkingConfig, ValidationError,
};

#[derive(Debug, Deserialize, Serialize)]
pub struct ResponsesRequest {
    pub model: String,
    pub input: Value,
    pub instructions: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub stream: bool,
    pub stream_options: Option<Value>,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    /// Keep omission distinct from an explicit empty list so a stateful
    /// continuation can inherit prior tools without preventing a caller from
    /// intentionally clearing them.
    #[serde(default)]
    pub tools: Option<Vec<Value>>,
    pub tool_choice: Option<Value>,
    pub parallel_tool_calls: Option<bool>,
    pub reasoning: Option<ResponsesReasoning>,
    pub text: Option<ResponsesText>,
    pub metadata: Option<Value>,
    #[serde(default, deserialize_with = "null_default")]
    pub include: Vec<String>,
    pub store: Option<bool>,
    pub background: Option<bool>,
    pub previous_response_id: Option<String>,
    pub conversation: Option<Value>,
    pub truncation: Option<String>,
    pub service_tier: Option<String>,
    pub max_tool_calls: Option<u32>,
    pub context_management: Option<Value>,
    pub prompt_cache_key: Option<String>,
    pub prompt_cache_retention: Option<String>,
    pub safety_identifier: Option<String>,
    pub user: Option<String>,
    /// Codex telemetry; never used for authorization or added to model input.
    pub client_metadata: Option<HashMap<String, String>>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ResponsesReasoning {
    pub effort: Option<String>,
    pub summary: Option<String>,
    pub context: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ResponsesText {
    pub format: Option<Value>,
    pub verbosity: Option<String>,
}

// The Responses schema permits explicit null for these optional controls.
// Normalize it at this boundary without relaxing Chat Completions validation.
fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}

#[derive(Debug, Clone)]
pub struct ResponsesToolName {
    pub name: String,
    pub namespace: Option<String>,
}

pub struct ResponsesTranslation {
    pub request: OpenAiRequest,
    /// Flattened OpenAI name -> original Responses namespace and leaf name.
    pub tool_names: HashMap<String, ResponsesToolName>,
    /// Effective Responses declarations from the top-level `tools` field and
    /// any `input[].additional_tools` control items. The HTTP adapter uses
    /// this canonical list for response echoing and stateful inheritance.
    pub tools: Vec<Value>,
}

pub fn responses_to_openai(
    request: &ResponsesRequest,
) -> Result<ResponsesTranslation, ValidationError> {
    validate_controls(request)?;
    let declared_tools = declared_tools(request)?;
    crate::translate::common::log_ignored_controls(
        "responses",
        &[
            ("service_tier", request.service_tier.is_some()),
            (
                "prompt_cache_retention",
                request.prompt_cache_retention.is_some(),
            ),
            ("include", !request.include.is_empty()),
            ("stream_options", request.stream_options.is_some()),
            (
                "reasoning.summary",
                request
                    .reasoning
                    .as_ref()
                    .and_then(|value| value.summary.as_deref())
                    .is_some_and(|summary| summary != "none"),
            ),
            (
                "reasoning.context",
                request
                    .reasoning
                    .as_ref()
                    .is_some_and(|value| value.context.is_some()),
            ),
        ],
    );
    let mut messages = Vec::new();
    if let Some(instructions) = &request.instructions {
        messages.push(message("system", Some(json!(instructions))));
    }
    if let Some(verbosity) = request
        .text
        .as_ref()
        .and_then(|text| text.verbosity.as_deref())
    {
        let instruction = match verbosity {
            "low" => "Keep the final response concise.",
            "medium" => "Use a moderate level of detail in the final response.",
            "high" => "Provide a detailed final response.",
            _ => "",
        };
        if !instruction.is_empty() {
            messages.push(message("system", Some(json!(instruction))));
        }
    }
    match &request.input {
        Value::String(text) => messages.push(message("user", Some(json!(text)))),
        Value::Array(items) if !items.is_empty() => {
            let mut calls = HashSet::new();
            let mut pending = HashMap::new();
            let mut opaque_reasoning = false;
            let mut ignored_items: Vec<&str> = Vec::new();
            for (index, item) in items.iter().enumerate() {
                let field = format!("input.{index}");
                let kind = match item.get("type") {
                    None => "message",
                    Some(value) => value
                        .as_str()
                        .ok_or_else(|| error(format!("{field}.type"), "expected a string"))?,
                };
                match kind {
                    // Responses Lite transports the callable catalog as an
                    // input control instead of the top-level `tools` field.
                    // It was validated and collected before message parsing;
                    // it is not conversational history.
                    "additional_tools" => {}
                    "message" => {
                        let role = required_string(item, "role", &field)?;
                        if !matches!(role, "system" | "developer" | "user" | "assistant") {
                            return invalid(
                                format!("{field}.role"),
                                "expected system, developer, user, or assistant",
                            );
                        }
                        let content = content(
                            item.get("content"),
                            &format!("{field}.content"),
                            role == "user",
                        )?;
                        messages.push(message(role, Some(content)));
                    }
                    "function_call" | "custom_tool_call" => {
                        let id = required_string(item, "call_id", &field)?;
                        if !calls.insert(id.to_owned()) {
                            return invalid(format!("{field}.call_id"), "duplicate tool call id");
                        }
                        pending.insert(id.to_owned(), kind);
                        let name = qualified_name(item, &field)?;
                        let (tool_type, key) = if kind == "function_call" {
                            ("function", "arguments")
                        } else {
                            ("custom", "input")
                        };
                        let input = string(item, key, &field)?;
                        let mut assistant = message("assistant", None);
                        assistant.tool_calls.push(json!({
                            "id":id,"type":tool_type,tool_type:{"name":name,key:input}
                        }));
                        messages.push(assistant);
                    }
                    "function_call_output" if is_codex_automation_bootstrap(item) => {
                        // Codex starts scheduled turns with named automation
                        // context, without an actual preceding tool call:
                        // https://github.com/openai/codex/issues/41799
                        // Preserve that context using the developer-message
                        // workaround instead of inventing an unpaired Kiro ID.
                        let output =
                            content(item.get("output"), &format!("{field}.output"), false)?;
                        messages.push(message("developer", Some(output)));
                        tracing::debug!(
                            event = "proxy.compatibility.input_normalized",
                            protocol = "responses",
                            input_index = index,
                            normalization = "codex_automation_bootstrap",
                            "preserved standalone Codex automation output as developer context"
                        );
                    }
                    "function_call_output" | "custom_tool_call_output" => {
                        let id = required_string(item, "call_id", &field)?;
                        let expected = if kind == "function_call_output" {
                            "function_call"
                        } else {
                            "custom_tool_call"
                        };
                        if pending.remove(id) != Some(expected) {
                            return invalid(format!("{field}.call_id"), "tool output must match an earlier, unanswered call in input; send the complete history");
                        }
                        let output = content(item.get("output"), &format!("{field}.output"), true)?;
                        let mut result = message("tool", Some(output));
                        result.tool_call_id = Some(id.to_owned());
                        messages.push(result);
                    }
                    "reasoning" => {
                        // Codex replays the reasoning item it was handed on the
                        // previous turn. An opaque `encrypted_content` cannot be
                        // decrypted by Kiro, but it is also not the client's
                        // fault: rejecting it strands the whole conversation on
                        // turn two. Drop the opaque blob and keep any plaintext
                        // summary, mirroring how the reference gateways ignore
                        // reasoning history rather than failing the request.
                        opaque_reasoning |= item
                            .get("encrypted_content")
                            .is_some_and(|v| !v.is_null() && v.as_str() != Some(""));
                        let mut reasoning = Vec::new();
                        for key in ["summary", "content"] {
                            if let Some(parts) = item.get(key).filter(|v| !v.is_null()) {
                                let Some(parts) = parts.as_array() else {
                                    return invalid(format!("{field}.{key}"), "expected an array");
                                };
                                for (part_index, part) in parts.iter().enumerate() {
                                    let part_field = format!("{field}.{key}.{part_index}");
                                    if !matches!(
                                        part.get("type").and_then(Value::as_str),
                                        Some("summary_text" | "reasoning_text" | "text")
                                    ) {
                                        return invalid(
                                            part_field,
                                            "expected a plaintext reasoning part",
                                        );
                                    }
                                    reasoning.push(string(part, "text", &part_field)?.to_owned());
                                }
                            }
                        }
                        if !reasoning.is_empty() {
                            // Kiro has no Responses reasoning-history item.
                            // Preserve the supplied plaintext summary explicitly
                            // without changing Chat Completions history policy.
                            let assistant = message(
                                "assistant",
                                Some(json!(format!(
                                    "Reasoning summary:\n{}",
                                    reasoning.join("\n")
                                ))),
                            );
                            messages.push(assistant);
                        }
                    }
                    // Responses grows new output item types (hosted tool calls,
                    // item_reference, ...). Codex echoes back whatever it was
                    // given, so a hard failure here breaks the next turn of an
                    // otherwise valid conversation. These items carry no content
                    // Kiro needs, so skip them and record the type once.
                    other => {
                        if !ignored_items.contains(&kind) {
                            ignored_items.push(kind);
                        }
                        let _ = other;
                    }
                }
            }
            if !pending.is_empty() {
                return invalid(
                    "input",
                    "every tool call in input must have a matching tool output",
                );
            }
            // Only fixed field/type names are recorded, never user content.
            crate::translate::common::log_ignored_controls(
                "responses",
                &[
                    ("input.reasoning.encrypted_content", opaque_reasoning),
                    ("input.unsupported_item_types", !ignored_items.is_empty()),
                ],
            );
        }
        _ => {
            return invalid(
                "input",
                "expected a string or a non-empty array of input items",
            )
        }
    }

    // Responses emits reasoning, messages and each parallel tool call as
    // separate items in one assistant turn. Kiro requires one assistant
    // message followed by the complete batch of matching tool results.
    let messages = merge_assistant_items(messages);
    let mut tools = Vec::new();
    let mut tool_names = HashMap::new();
    for declaration in &declared_tools {
        let tool = declaration.value;
        let field = declaration.field.as_str();
        if tool.get("type").and_then(Value::as_str) == Some("namespace") {
            let namespace = required_string(tool, "name", field)?;
            let description = tool
                .get("description")
                .filter(|value| !value.is_null())
                .map(|_| string(tool, "description", field))
                .transpose()?;
            let Some(children) = tool.get("tools").and_then(Value::as_array) else {
                return invalid(
                    format!("{field}.tools"),
                    "expected an array of function or custom tools",
                );
            };
            for (child_index, child) in children.iter().enumerate() {
                add_tool(
                    child,
                    Some(namespace),
                    description,
                    &format!("{field}.tools.{child_index}"),
                    &mut tools,
                    &mut tool_names,
                )?;
            }
        } else {
            add_tool(tool, None, None, field, &mut tools, &mut tool_names)?;
        }
    }
    let tool_choice = request
        .tool_choice
        .as_ref()
        .filter(|v| !v.is_null())
        .map(|choice| {
            if choice.is_string() {
                return Ok(choice.clone());
            }
            let kind = required_string(choice, "type", "tool_choice")?;
            if matches!(kind, "auto" | "none" | "required") {
                return Ok(json!(kind));
            }
            if matches!(kind, "function" | "custom") {
                let name = qualified_name(choice, "tool_choice")?;
                return Ok(json!({"type":kind,kind:{"name":name}}));
            }
            if kind != "allowed_tools" {
                return invalid(
                    "tool_choice.type",
                    "expected auto, none, required, function, custom, or allowed_tools",
                );
            }
            let allowed = choice.get("allowed_tools").unwrap_or(choice);
            let mode = required_string(allowed, "mode", "tool_choice.allowed_tools")?;
            let tools = allowed
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    error(
                        "tool_choice.allowed_tools.tools",
                        "expected an array of function or custom tool references",
                    )
                })?;
            let mut normalized = Vec::with_capacity(tools.len());
            for (index, tool) in tools.iter().enumerate() {
                let field = format!("tool_choice.allowed_tools.tools.{index}");
                let tool_kind = required_string(tool, "type", &field)?;
                if !matches!(tool_kind, "function" | "custom") {
                    return invalid(format!("{field}.type"), "expected function or custom");
                }
                // Responses uses a flat `{type, name}` reference. Accept the
                // nested form too because this normalizer also accepts the
                // shared OpenAI representation internally. Flat references
                // retain the optional namespace extension used by this proxy.
                let name = if tool.get("name").is_some() {
                    qualified_name(tool, &field)?
                } else {
                    let definition = tool.get(tool_kind).ok_or_else(|| {
                        error(format!("{field}.{tool_kind}.name"), "tool name is required")
                    })?;
                    required_string(definition, "name", &format!("{field}.{tool_kind}"))?.to_owned()
                };
                normalized.push(json!({"type":tool_kind,tool_kind:{"name":name}}));
            }
            Ok(json!({
                "type":"allowed_tools",
                "allowed_tools":{"mode":mode,"tools":normalized}
            }))
        })
        .transpose()?;
    let effort = request.reasoning.as_ref().and_then(|r| r.effort.as_deref());
    let thinking = (effort == Some("none")).then(|| ThinkingConfig {
        r#type: "disabled".into(),
        budget_tokens: None,
        display: None,
    });
    let normalized = OpenAiRequest {
        model: request.model.clone(),
        messages,
        temperature: request.temperature,
        top_p: request.top_p,
        max_tokens: request.max_output_tokens,
        max_completion_tokens: None,
        stop: None,
        n: None,
        legacy_functions: false,
        stream: request.stream,
        stream_options: request.stream.then(|| json!({"include_usage":true})),
        tools,
        tool_choice,
        parallel_tool_calls: request.parallel_tool_calls.unwrap_or(true),
        thinking,
        reasoning_effort: effort.filter(|effort| *effort != "none").map(str::to_owned),
        // Kiro's conversation ID is also its prompt-cache affinity key. The
        // HTTP layer hashes this client hint into an API-key-scoped UUID.
        conversation_id: request.prompt_cache_key.clone(),
        metadata: request.metadata.clone(),
        response_format: request.text.as_ref().and_then(|text| text.format.clone()),
    };
    validate_openai(&normalized)?;
    Ok(ResponsesTranslation {
        request: normalized,
        tool_names,
        tools: declared_tools
            .into_iter()
            .map(|declaration| declaration.value.clone())
            .collect(),
    })
}

struct DeclaredTool<'a> {
    value: &'a Value,
    field: String,
}

/// Collect both Responses tool declaration channels. Recent Codex Responses
/// Lite requests omit top-level `tools` and place the complete catalog in an
/// `additional_tools` input item instead. Exact duplicates are coalesced so a
/// client can safely send both representations during a compatibility rollout;
/// conflicting declarations still fail later as ambiguous tool names.
fn declared_tools(request: &ResponsesRequest) -> Result<Vec<DeclaredTool<'_>>, ValidationError> {
    let mut declarations = Vec::new();
    let top_level = request.tools.as_deref().unwrap_or(&[]);
    for (index, tool) in top_level.iter().enumerate() {
        declarations.push(DeclaredTool {
            value: tool,
            field: format!("tools.{index}"),
        });
    }
    if let Value::Array(items) = &request.input {
        for (item_index, item) in items.iter().enumerate() {
            if item.get("type").and_then(Value::as_str) != Some("additional_tools") {
                continue;
            }
            let field = format!("input.{item_index}.tools");
            let tools = item
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| error(&field, "expected an array of tool declarations"))?;
            for (tool_index, tool) in tools.iter().enumerate() {
                if !top_level.contains(tool) {
                    declarations.push(DeclaredTool {
                        value: tool,
                        field: format!("{field}.{tool_index}"),
                    });
                }
            }
        }
    }
    Ok(declarations)
}

fn validate_controls(request: &ResponsesRequest) -> Result<(), ValidationError> {
    for (field, unsupported) in [
        ("background", request.background == Some(true)),
        ("conversation", request.conversation.is_some()),
        ("context_management", request.context_management.is_some()),
    ] {
        if unsupported {
            return invalid(field, "not supported by this endpoint");
        }
    }
    if request
        .truncation
        .as_deref()
        .is_some_and(|v| !matches!(v, "disabled" | "auto"))
    {
        return invalid("truncation", "expected disabled or auto");
    }
    // Retention, include and reasoning presentation hints do not change the
    // Kiro payload. Accept them like the reference gateways, without promising
    // the originating API's service or output guarantees.
    if let Some(options) = &request.stream_options {
        if !options.is_object() {
            return invalid("stream_options", "expected an object");
        }
    }
    Ok(())
}

fn add_tool(
    tool: &Value,
    namespace: Option<&str>,
    namespace_description: Option<&str>,
    field: &str,
    tools: &mut Vec<OpenAiTool>,
    names: &mut HashMap<String, ResponsesToolName>,
) -> Result<(), ValidationError> {
    let kind = required_string(tool, "type", field)?;
    if !matches!(kind, "function" | "custom") {
        return invalid(
            format!("{field}.type"),
            "hosted tools are not supported; use function or custom tools executed by the client",
        );
    }
    let name = required_string(tool, "name", field)?;
    let qualified = namespace.map_or_else(
        || name.to_owned(),
        |namespace| format!("{namespace}.{name}"),
    );
    if names
        .insert(
            qualified.clone(),
            ResponsesToolName {
                name: name.to_owned(),
                namespace: namespace.map(str::to_owned),
            },
        )
        .is_some()
    {
        return invalid(format!("{field}.name"), "duplicate or ambiguous tool name");
    }
    let mut definition = tool.as_object().cloned().unwrap_or_default();
    definition.remove("type");
    // These fields are nullable in the Responses function schema. An absent
    // parameters schema uses the existing no-argument tool default.
    for key in ["description", "parameters", "strict"] {
        if definition.get(key).is_some_and(Value::is_null) {
            definition.remove(key);
        }
    }
    if let Some(deferred) = definition.remove("defer_loading") {
        if deferred != false {
            return invalid(
                format!("{field}.defer_loading"),
                "deferred Responses tools are not supported",
            );
        }
    }
    definition.insert("name".into(), json!(qualified));
    if let Some(scope) = namespace_description.filter(|text| !text.is_empty()) {
        let description = tool
            .get("description")
            .filter(|value| !value.is_null())
            .map(|_| string(tool, "description", field))
            .transpose()?
            .unwrap_or_default();
        definition.insert(
            "description".into(),
            json!(format!(
                "Namespace {}: {scope}\n\n{description}",
                namespace.unwrap_or_default()
            )),
        );
    }
    if kind == "custom" {
        if let Some(format) = definition.get_mut("format") {
            if format.get("type").and_then(Value::as_str) == Some("grammar") {
                *format = json!({"type":"grammar","grammar":{
                    "syntax":format.get("syntax"),"definition":format.get("definition")
                }});
            }
        }
    }
    tools.push(OpenAiTool {
        r#type: kind.into(),
        body: json!({kind:definition}),
    });
    Ok(())
}

fn qualified_name(item: &Value, field: &str) -> Result<String, ValidationError> {
    let name = required_string(item, "name", field)?;
    match item.get("namespace").filter(|v| !v.is_null()) {
        Some(value) => {
            let namespace = value
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    error(format!("{field}.namespace"), "expected a non-empty string")
                })?;
            Ok(format!("{namespace}.{name}"))
        }
        None => Ok(name.to_owned()),
    }
}

fn content(value: Option<&Value>, field: &str, images: bool) -> Result<Value, ValidationError> {
    match value {
        Some(Value::String(text)) => Ok(json!(text)),
        Some(Value::Array(parts)) => parts
            .iter()
            .enumerate()
            .map(|(index, part)| {
                let field = format!("{field}.{index}");
                match part.get("type").and_then(Value::as_str) {
                    Some("input_text" | "output_text") => {
                        Ok(json!({"type":"text","text":string(part, "text", &field)?}))
                    }
                    Some("refusal") => {
                        Ok(json!({"type":"text","text":string(part, "refusal", &field)?}))
                    }
                    Some("input_file") if images => {
                        crate::compatibility::openai_file_document(part, &field)
                    }
                    Some("input_image") if images => {
                        let url = required_string(part, "image_url", &field)?;
                        if part.get("file_id").is_some_and(|v| !v.is_null()) {
                            return invalid(
                                format!("{field}.file_id"),
                                "use an image URL or base64 data URL",
                            );
                        }
                        let mut image = json!({"url":url});
                        if let Some(detail) = part.get("detail").filter(|v| !v.is_null()) {
                            if !matches!(
                                detail.as_str(),
                                Some("auto" | "low" | "high" | "original")
                            ) {
                                return invalid(format!("{field}.detail"), "invalid image detail");
                            }
                            image["detail"] = detail.clone();
                        }
                        Ok(json!({"type":"image_url","image_url":image}))
                    }
                    _ => invalid(
                        format!("{field}.type"),
                        "unsupported content part; use text or input_image with an image URL",
                    ),
                }
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        _ => invalid(field, "expected a string or content array"),
    }
}

fn is_codex_automation_bootstrap(item: &Value) -> bool {
    item.get("name").and_then(Value::as_str) == Some("automation_update")
        && item.get("namespace").and_then(Value::as_str) == Some("codex_app")
        && item.get("call_id").is_none_or(Value::is_null)
}

fn message(role: &str, content: Option<Value>) -> OpenAiMessage {
    OpenAiMessage {
        role: role.into(),
        content,
        tool_calls: Vec::new(),
        tool_call_id: None,
        reasoning_content: None,
        refusal: None,
        name: None,
        cache_control: None,
    }
}

fn merge_assistant_items(messages: Vec<OpenAiMessage>) -> Vec<OpenAiMessage> {
    let mut merged: Vec<OpenAiMessage> = Vec::with_capacity(messages.len());
    for mut message in messages {
        let previous = merged
            .last_mut()
            .filter(|previous| previous.role == "assistant" && message.role == "assistant");
        if let Some(previous) = previous {
            let had_content = previous.content.is_some() || message.content.is_some();
            // Reuse the accumulated vector: copying all earlier parts for
            // every assistant item makes fragmented histories quadratic.
            let mut parts = match previous.content.take() {
                Some(Value::Array(parts)) => parts,
                Some(Value::String(text)) if !text.is_empty() => {
                    vec![json!({"type":"text","text":text})]
                }
                _ => Vec::new(),
            };
            if let Some(content) = message.content.take() {
                match content {
                    Value::String(text) if !text.is_empty() => {
                        parts.push(json!({"type":"text","text":text}))
                    }
                    Value::Array(items) => parts.extend(items),
                    _ => {}
                }
            }
            previous.content = if parts.is_empty() {
                had_content.then(|| json!(""))
            } else {
                Some(Value::Array(parts))
            };
            previous.tool_calls.append(&mut message.tool_calls);
        } else {
            merged.push(message);
        }
    }
    merged
}

fn string<'a>(value: &'a Value, key: &str, field: &str) -> Result<&'a str, ValidationError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| error(format!("{field}.{key}"), "expected a string"))
}

fn required_string<'a>(
    value: &'a Value,
    key: &str,
    field: &str,
) -> Result<&'a str, ValidationError> {
    let value = string(value, key, field)?;
    if value.trim().is_empty() {
        return invalid(format!("{field}.{key}"), "must not be empty");
    }
    Ok(value)
}

fn error(field: impl Into<String>, message: impl Into<String>) -> ValidationError {
    ValidationError::InvalidField {
        field: field.into(),
        message: message.into(),
    }
}

fn invalid<T>(field: impl Into<String>, message: impl Into<String>) -> Result<T, ValidationError> {
    Err(error(field, message))
}

#[cfg(test)]
mod tests;
