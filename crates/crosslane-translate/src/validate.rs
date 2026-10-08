//! Request validation and bounded tool-schema traversal.

use base64::Engine as _;
use serde_json::Value;
use std::collections::HashSet;
use thiserror::Error;
use url::Url;

use crate::translate::common::claude_document_format;
use crate::{is_tool_search_type, matches_type_family, ClaudeRequest, OpenAiRequest};

mod cache;
use cache::{validate_cache_control, validate_claude_cache_controls};

pub const MAX_SCHEMA_DEPTH: usize = 64;
pub const MAX_SCHEMA_NODES: usize = 50_000;
pub const MAX_TOOL_DOC_CHARS: usize = 512_000;
pub const MAX_TOOLS: usize = crosslane_core::config::MAX_LOADED_TOOLS;
pub const MAX_DEFERRED_TOOLS: usize = 10_000;
pub const MAX_LOADED_TOOL_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_DEFERRED_TOOL_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_TOOL_BYTES: usize = 256 * 1024;
pub const MAX_TOOL_NAME_CHARS: usize = 128;
pub const MAX_STOP_SEQUENCES: usize = 64;
pub const MAX_STOP_SEQUENCE_BYTES: usize = 4 * 1024;
pub const MAX_STOP_SEQUENCE_TOTAL_BYTES: usize = 64 * 1024;
pub const MAX_DOCUMENTS_PER_MESSAGE: usize = 5;
pub const MAX_DOCUMENT_BYTES: usize = 4_500_000;
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_IMAGES_PER_REQUEST: usize = 100;
pub const MAX_DOCUMENTS_PER_REQUEST: usize = 5;
pub const MAX_CACHE_BREAKPOINTS: usize = 4;
/// Resource/logging boundary, not an enumeration of Kiro's supported models.
pub const MAX_MODEL_BYTES: usize = 256;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("model is required")]
    MissingModel,
    #[error("messages must not be empty")]
    MissingMessages,
    #[error("too many tools: maximum is {MAX_TOOLS}")]
    TooManyTools,
    #[error("too many deferred tools: maximum is {MAX_DEFERRED_TOOLS}")]
    TooManyDeferredTools,
    #[error("loaded tool definitions exceed the {MAX_LOADED_TOOL_BYTES} byte limit; enable Anthropic Tool Search and defer MCP tools")]
    LoadedToolDefinitionsTooLarge,
    #[error("deferred tool catalog exceeds the {MAX_DEFERRED_TOOL_BYTES} byte limit")]
    DeferredToolDefinitionsTooLarge,
    #[error("a single tool definition exceeds the {MAX_TOOL_BYTES} byte limit")]
    ToolDefinitionTooLarge,
    #[error("tool name exceeds {MAX_TOOL_NAME_CHARS} characters")]
    ToolNameTooLong,
    #[error("tool documentation exceeds {MAX_TOOL_DOC_CHARS} characters")]
    ToolDocumentationTooLarge,
    #[error("tool schema exceeds maximum depth {MAX_SCHEMA_DEPTH}")]
    SchemaTooDeep,
    #[error("tool schema exceeds maximum node count {MAX_SCHEMA_NODES}")]
    SchemaTooLarge,
    #[error("message role is not supported: {0}")]
    InvalidRole(String),
    #[error("invalid value for {field}: {message}")]
    InvalidField { field: String, message: String },
}

pub fn validate_claude(request: &ClaudeRequest) -> Result<(), ValidationError> {
    validate_claude_request(request, false)
}

/// Counting has no generation budget, even when extended thinking is enabled.
pub fn validate_claude_count(request: &ClaudeRequest) -> Result<(), ValidationError> {
    validate_claude_request(request, true)
}

fn validate_claude_request(request: &ClaudeRequest, counting: bool) -> Result<(), ValidationError> {
    common(&request.model, request.messages.is_empty())?;
    // Match the reference gateways' permissive handling of additive controls.
    // Validate data we consume, not official guarantees Kiro does not expose.
    if !counting && request.max_tokens == 0 {
        return invalid(
            "max_tokens",
            "zero-token cache warming is not supported by Kiro; setting maxTokens to zero can still generate output",
        );
    }
    if request
        .temperature
        .is_some_and(|temperature| !(0.0..=1.0).contains(&temperature))
    {
        return invalid("temperature", "must be in 0..=1");
    }
    if request
        .top_p
        .is_some_and(|top_p| !(0.0..=1.0).contains(&top_p))
    {
        return invalid("top_p", "must be in 0..=1");
    }
    if request.stop_sequences.iter().any(String::is_empty) {
        return invalid("stop_sequences", "sequences must not be empty");
    }
    if request.stop_sequences.len() > MAX_STOP_SEQUENCES {
        return invalid(
            "stop_sequences",
            format!("must contain at most {MAX_STOP_SEQUENCES} sequences"),
        );
    }
    if request
        .stop_sequences
        .iter()
        .any(|sequence| sequence.len() > MAX_STOP_SEQUENCE_BYTES)
    {
        return invalid(
            "stop_sequences",
            format!("each sequence must be at most {MAX_STOP_SEQUENCE_BYTES} bytes"),
        );
    }
    let stop_sequence_bytes = request
        .stop_sequences
        .iter()
        .map(String::len)
        .fold(0usize, usize::saturating_add);
    if stop_sequence_bytes > MAX_STOP_SEQUENCE_TOTAL_BYTES {
        return invalid(
            "stop_sequences",
            format!("sequences must total at most {MAX_STOP_SEQUENCE_TOTAL_BYTES} bytes"),
        );
    }
    validate_claude_system(request.system.as_ref())?;
    validate_claude_output_config(request.output_config.as_ref(), "output_config")?;
    if request
        .metadata
        .as_ref()
        .is_some_and(|metadata| !metadata.is_object())
    {
        return invalid("metadata", "expected an object");
    }
    if request
        .conversation_id
        .as_ref()
        .is_some_and(|id| id.trim().is_empty() || id.len() > 256)
    {
        return invalid(
            "conversation_id",
            "must be a non-empty string of at most 256 bytes",
        );
    }
    let mut image_count = 0usize;
    let mut document_count = 0usize;
    for (index, message) in request.messages.iter().enumerate() {
        if !matches!(message.role.as_str(), "user" | "assistant" | "system") {
            return Err(ValidationError::InvalidRole(message.role.clone()));
        }
        if message.output_config.is_some() && message.role != "system" {
            return invalid(
                format!("messages.{index}.output_config"),
                "per-message output_config is only supported on system messages",
            );
        }
        validate_claude_output_config(
            message.output_config.as_ref(),
            &format!("messages.{index}.output_config"),
        )?;
        validate_claude_content(
            &message.content,
            &message.role,
            format!("messages.{index}.content"),
        )?;
        image_count = image_count.saturating_add(count_claude_images(&message.content));
        document_count =
            document_count.saturating_add(count_claude_documents_in_content(&message.content));
    }
    if image_count > MAX_IMAGES_PER_REQUEST {
        return invalid(
            "messages",
            format!("must contain at most {MAX_IMAGES_PER_REQUEST} image blocks"),
        );
    }
    if document_count > MAX_DOCUMENTS_PER_REQUEST {
        return invalid(
            "messages",
            format!("must contain at most {MAX_DOCUMENTS_PER_REQUEST} document blocks"),
        );
    }
    validate_server_tool_history(request)?;
    let deferred_count = request
        .tools
        .iter()
        .filter(|tool| tool.defer_loading)
        .count();
    let loaded_count = request.tools.len().saturating_sub(deferred_count);
    if loaded_count > MAX_TOOLS {
        return Err(ValidationError::TooManyTools);
    }
    if deferred_count > MAX_DEFERRED_TOOLS {
        return Err(ValidationError::TooManyDeferredTools);
    }
    if request.tools.iter().any(|tool| {
        serde_json::to_vec(tool).map_or(true, |definition| definition.len() > MAX_TOOL_BYTES)
    }) {
        return Err(ValidationError::ToolDefinitionTooLarge);
    }
    let (loaded_bytes, deferred_bytes) =
        request
            .tools
            .iter()
            .fold((2usize, 2usize), |(loaded, deferred), tool| {
                let bytes = serde_json::to_vec(tool).map_or(usize::MAX, |value| value.len());
                if tool.defer_loading {
                    (loaded, deferred.saturating_add(bytes).saturating_add(1))
                } else {
                    (loaded.saturating_add(bytes).saturating_add(1), deferred)
                }
            });
    if loaded_bytes > MAX_LOADED_TOOL_BYTES {
        return Err(ValidationError::LoadedToolDefinitionsTooLarge);
    }
    if deferred_bytes > MAX_DEFERRED_TOOL_BYTES {
        return Err(ValidationError::DeferredToolDefinitionsTooLarge);
    }
    if deferred_count > 0 && loaded_count == 0 {
        return invalid("tools", "at least one tool must have defer_loading=false");
    }
    let docs = request
        .tools
        .iter()
        .filter(|tool| !tool.defer_loading)
        .map(|tool| {
            tool.description.chars().count()
                + tool
                    .input_examples
                    .as_ref()
                    .and_then(|examples| serde_json::to_string(examples).ok())
                    .map_or(0, |examples| examples.chars().count())
        })
        .sum::<usize>();
    if docs > MAX_TOOL_DOC_CHARS {
        return Err(ValidationError::ToolDocumentationTooLarge);
    }
    let mut seen_tool_names = HashSet::new();
    for (index, tool) in request.tools.iter().enumerate() {
        let kind = tool.r#type.as_deref();
        if kind.is_some_and(|kind| matches_type_family(kind, "web_fetch")) {
            return invalid(
                format!("tools.{index}.type"),
                "Claude Web Fetch is not implemented by this proxy; rejecting it avoids exposing a client tool with incompatible server-tool semantics",
            );
        }
        if kind == Some("mcp_toolset") {
            return invalid(
                format!("tools.{index}.type"),
                "mcp_toolset is not supported; expand it into individual custom tools or use Tool Search with defer_loading",
            );
        }
        if tool.name.trim().is_empty() {
            return invalid(format!("tools.{index}.name"), "must not be empty");
        }
        if !seen_tool_names.insert(tool.name.as_str()) {
            return invalid(format!("tools.{index}.name"), "tool names must be unique");
        }
        let web_tool = kind.is_some_and(|kind| matches_type_family(kind, "web_search"));
        let search_tool = kind.is_some_and(is_tool_search_type);
        if kind.is_some_and(|kind| kind.starts_with("tool_search_tool_")) && !search_tool {
            return invalid(
                format!("tools.{index}.type"),
                format!(
                    "unsupported Claude Tool Search version '{}'",
                    kind.unwrap_or_default()
                ),
            );
        }
        if web_tool && tool.name != "web_search" {
            return invalid(
                format!("tools.{index}.name"),
                "web_search server tools must use name=\"web_search\"",
            );
        }
        if kind.is_some_and(|kind| kind != "custom") && !web_tool && !search_tool {
            return invalid(
                format!("tools.{index}.type"),
                format!(
                    "unsupported Claude server tool '{}'",
                    kind.unwrap_or_default()
                ),
            );
        }
        if !web_tool && !search_tool {
            validate_tool(&tool.name, &tool.input_schema).map_err(|error| {
                if error == ValidationError::ToolNameTooLong {
                    error
                } else {
                    ValidationError::InvalidField {
                        field: format!("tools.{index}.input_schema"),
                        message: error.to_string(),
                    }
                }
            })?;
        }
        let tool_cache_breakpoints = validate_cache_control(
            tool.cache_control.as_ref(),
            &format!("tools.{index}.cache_control"),
        )?;
        if let Some(callers) = &tool.allowed_callers {
            if callers.as_slice() != ["direct"] {
                return invalid(
                    format!("tools.{index}.allowed_callers"),
                    "only allowed_callers=[\"direct\"] is supported; code-execution callers cannot be emulated by Kiro",
                );
            }
        }
        if kind.is_some_and(|kind| {
            matches_type_family(kind, "web_search")
                && !matches!(kind, "web_search" | "web_search_20250305")
        }) && tool.allowed_callers.is_none()
        {
            return invalid(
                format!("tools.{index}.allowed_callers"),
                "this web search version defaults to code execution; set allowed_callers=[\"direct\"] for Kiro compatibility",
            );
        }
        if web_tool {
            if tool.input_examples.is_some() {
                return invalid(
                    format!("tools.{index}.input_examples"),
                    "input_examples is not supported on Claude server tools",
                );
            }
            if tool
                .allowed_domains
                .as_ref()
                .is_some_and(|domains| !domains.is_empty())
                || tool
                    .blocked_domains
                    .as_ref()
                    .is_some_and(|domains| !domains.is_empty())
            {
                return invalid(
                    format!("tools.{index}"),
                    "web search domain filters are not supported by the Kiro MCP search endpoint",
                );
            }
            if tool.user_location.is_some() {
                return invalid(
                    format!("tools.{index}.user_location"),
                    "web search localization is not supported by the Kiro MCP search endpoint",
                );
            }
            if tool
                .response_inclusion
                .as_deref()
                .is_some_and(|value| !matches!(value, "full" | "excluded"))
            {
                return invalid(
                    format!("tools.{index}.response_inclusion"),
                    "expected \"full\" or \"excluded\"",
                );
            }
            if tool.response_inclusion.as_deref() == Some("excluded") {
                return invalid(
                    format!("tools.{index}.response_inclusion"),
                    "response_inclusion=\"excluded\" cannot be emulated because Kiro needs the search results to synthesize the answer",
                );
            }
            if tool.max_uses == Some(0) {
                return invalid(format!("tools.{index}.max_uses"), "must be at least 1");
            }
        } else if tool.max_uses.is_some()
            || tool.allowed_domains.is_some()
            || tool.blocked_domains.is_some()
            || tool.user_location.is_some()
            || tool.response_inclusion.is_some()
        {
            return invalid(
                format!("tools.{index}"),
                "web search configuration fields are only valid on web_search server tools",
            );
        }
        if tool.defer_loading && tool_cache_breakpoints > 0 {
            return invalid(
                format!("tools.{index}.cache_control"),
                "deferred tools cannot define cache_control",
            );
        }
        if search_tool && tool.defer_loading {
            return invalid(
                format!("tools.{index}.defer_loading"),
                "the Tool Search tool must be loaded immediately",
            );
        }
        if search_tool && tool.input_examples.is_some() {
            return invalid(
                format!("tools.{index}.input_examples"),
                "input_examples is not supported on Claude server tools",
            );
        }
        if tool
            .input_examples
            .as_ref()
            .is_some_and(|examples| examples.iter().any(|example| !example.is_object()))
        {
            return invalid(
                format!("tools.{index}.input_examples"),
                "every input example must be an object",
            );
        }
    }
    validate_claude_cache_controls(request)?;
    validate_tool_references(request, &seen_tool_names)?;
    if let Some(choice) = &request.tool_choice {
        if !matches!(choice.r#type.as_str(), "auto" | "any" | "tool" | "none") {
            return invalid("tool_choice.type", "expected auto, any, tool, or none");
        }
        if choice.r#type == "tool" {
            let Some(name) = choice
                .name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
            else {
                return invalid("tool_choice.name", "a tool name is required");
            };
            let found = request.tools.iter().any(|tool| {
                tool.name == name
                    || tool.r#type.as_deref() == Some(name)
                    || (name == "web_search"
                        && tool
                            .r#type
                            .as_deref()
                            .is_some_and(|kind| matches_type_family(kind, "web_search")))
                    || (name == "web_fetch"
                        && tool
                            .r#type
                            .as_deref()
                            .is_some_and(|kind| matches_type_family(kind, "web_fetch")))
            });
            if !found {
                return invalid("tool_choice.name", format!("tool '{name}' was not found"));
            }
        } else if choice.name.is_some() {
            return invalid(
                "tool_choice.name",
                "name is only valid when tool_choice.type is tool",
            );
        }
        if choice.r#type == "any" && request.tools.is_empty() {
            return invalid("tool_choice.type", "any requires at least one tool");
        }
        if matches!(choice.r#type.as_str(), "any" | "tool")
            && request
                .thinking
                .as_ref()
                .is_some_and(|thinking| thinking.r#type == "enabled")
        {
            return invalid(
                "tool_choice.type",
                "forced tool choice is incompatible with enabled thinking",
            );
        }
    }
    validate_thinking(
        request.thinking.as_ref(),
        (!counting).then_some(request.max_tokens),
    )?;
    validate_context_management(request.context_management.as_ref())?;
    Ok(())
}

fn validate_claude_output_config(
    config: Option<&crate::ClaudeOutputConfig>,
    path: &str,
) -> Result<(), ValidationError> {
    let Some(config) = config else {
        return Ok(());
    };
    if let Some(effort) = &config.effort {
        if !matches!(effort.as_str(), "low" | "medium" | "high" | "xhigh" | "max") {
            return invalid(
                format!("{path}.effort"),
                "expected low, medium, high, xhigh, or max",
            );
        }
    }
    // Only effort is consumed by the Kiro adapter. Preserve the gateway's
    // existing acceptance of format/task_budget and other additive hints;
    // accepting them does not promise native structured-output enforcement.
    Ok(())
}

/// Validates Messages semantics that are specific to generation.
///
/// The token-count endpoint can count a prompt ending in an assistant turn,
/// but Kiro exposes only a current user input and has no assistant-prefill
/// field. Rejecting the generation request is safer than silently converting
/// the prefill into a new user turn.
pub fn validate_claude_generation(request: &ClaudeRequest) -> Result<(), ValidationError> {
    if let Some((index, _)) = request
        .messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, message)| message.role != "system")
        .filter(|(_, message)| message.role == "assistant")
    {
        return invalid(
            format!("messages.{index}.role"),
            "assistant prefill is not supported by the Kiro upstream",
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerToolKind {
    ToolSearch,
    WebSearch,
}

fn server_tool_kind(name: &str) -> Option<ServerToolKind> {
    if name == "web_search" {
        Some(ServerToolKind::WebSearch)
    } else if is_tool_search_type(name) {
        Some(ServerToolKind::ToolSearch)
    } else {
        None
    }
}

/// Validate the cross-block protocol that cannot be checked one content block
/// at a time. Server-tool IDs are globally unique in a request, result blocks
/// must match the kind of their pending call, and clients must never answer a
/// `srvtoolu_*` call with an ordinary client `tool_result`.
fn validate_server_tool_history(request: &ClaudeRequest) -> Result<(), ValidationError> {
    let mut all_tool_uses = std::collections::HashMap::<String, String>::new();
    let mut server_tool_uses = std::collections::HashMap::<String, (ServerToolKind, String)>::new();

    for (message_index, message) in request.messages.iter().enumerate() {
        let Some(blocks) = message.content.as_array() else {
            continue;
        };
        for (block_index, block) in blocks.iter().enumerate() {
            let field = format!("messages.{message_index}.content.{block_index}");
            let kind = block.get("type").and_then(Value::as_str);
            if !matches!(kind, Some("tool_use" | "server_tool_use")) {
                continue;
            }
            let Some(id) = block.get("id").and_then(Value::as_str) else {
                continue;
            };
            if let Some(previous) = all_tool_uses.insert(id.to_owned(), field.clone()) {
                return invalid(
                    format!("{field}.id"),
                    format!("tool-use id '{id}' is already defined at {previous}"),
                );
            }
            if kind == Some("server_tool_use") {
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let Some(server_kind) = server_tool_kind(name) else {
                    return invalid(
                        format!("{field}.name"),
                        format!("unsupported server tool '{name}'"),
                    );
                };
                server_tool_uses.insert(id.to_owned(), (server_kind, field));
            }
        }
    }

    let mut pending = std::collections::HashMap::<String, (ServerToolKind, String)>::new();
    let mut completed = HashSet::<String>::new();
    for (message_index, message) in request.messages.iter().enumerate() {
        let Some(blocks) = message.content.as_array() else {
            continue;
        };
        for (block_index, block) in blocks.iter().enumerate() {
            let field = format!("messages.{message_index}.content.{block_index}");
            let kind = block.get("type").and_then(Value::as_str);
            if kind == Some("server_tool_use") {
                let Some(id) = block.get("id").and_then(Value::as_str) else {
                    continue;
                };
                if let Some((server_kind, use_field)) = server_tool_uses.get(id) {
                    pending.insert(id.to_owned(), (*server_kind, use_field.clone()));
                }
                continue;
            }
            let Some(result_kind) = (match kind {
                Some("tool_search_tool_result") => Some(ServerToolKind::ToolSearch),
                Some("web_search_tool_result") => Some(ServerToolKind::WebSearch),
                _ => None,
            }) else {
                if kind == Some("tool_result") {
                    if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                        if server_tool_uses.contains_key(id) {
                            return invalid(
                                format!("{field}.tool_use_id"),
                                format!(
                                    "server-tool call '{id}' must be completed by its server result block, not tool_result"
                                ),
                            );
                        }
                    }
                }
                continue;
            };
            let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                continue;
            };
            if completed.contains(id) {
                return invalid(
                    format!("{field}.tool_use_id"),
                    format!("server-tool call '{id}' has more than one result"),
                );
            }
            let Some((expected_kind, use_field)) = pending.get(id) else {
                let message = if server_tool_uses.contains_key(id) {
                    format!("server-tool result '{id}' appears before its server_tool_use")
                } else {
                    format!("server-tool result '{id}' has no matching server_tool_use")
                };
                return invalid(format!("{field}.tool_use_id"), message);
            };
            if *expected_kind != result_kind {
                return invalid(
                    format!("{field}.tool_use_id"),
                    format!("server-tool result '{id}' does not match the call at {use_field}"),
                );
            }
            completed.insert(id.to_owned());
            pending.remove(id);
        }
    }
    Ok(())
}

fn validate_tool_references(
    request: &ClaudeRequest,
    tool_names: &HashSet<&str>,
) -> Result<(), ValidationError> {
    for (message_index, message) in request.messages.iter().enumerate() {
        let Some(blocks) = message.content.as_array() else {
            continue;
        };
        for (block_index, block) in blocks.iter().enumerate() {
            let references = match block.get("type").and_then(Value::as_str) {
                Some("tool_search_tool_result") => block
                    .pointer("/content/tool_references")
                    .and_then(Value::as_array),
                Some("tool_result") => block.get("content").and_then(Value::as_array),
                _ => None,
            };
            let Some(references) = references else {
                continue;
            };
            for (reference_index, reference) in references.iter().enumerate() {
                if reference.get("type").and_then(Value::as_str) != Some("tool_reference") {
                    continue;
                }
                let Some(name) = reference.get("tool_name").and_then(Value::as_str) else {
                    continue;
                };
                if !tool_names.contains(name) {
                    return invalid(
                        format!(
                            "messages.{message_index}.content.{block_index}.tool_reference.{reference_index}.tool_name"
                        ),
                        format!("referenced tool '{name}' is not defined in the top-level tools array"),
                    );
                }
            }
        }
    }
    Ok(())
}

fn validate_claude_system(value: Option<&Value>) -> Result<(), ValidationError> {
    let Some(value) = value else {
        return Ok(());
    };
    match value {
        Value::String(_) => Ok(()),
        Value::Array(blocks) => {
            for (index, block) in blocks.iter().enumerate() {
                if block.get("type").and_then(Value::as_str) != Some("text")
                    || !block.get("text").is_some_and(Value::is_string)
                {
                    return invalid(format!("system.{index}"), "expected a text content block");
                }
            }
            Ok(())
        }
        _ => invalid("system", "expected a string or content array"),
    }
}

fn validate_content_cache_controls(content: &Value, field: &str) -> Result<usize, ValidationError> {
    let Some(blocks) = content.as_array() else {
        return Ok(0);
    };
    let mut count = 0usize;
    for (index, block) in blocks.iter().enumerate() {
        count = count.saturating_add(validate_cache_control(
            block.get("cache_control"),
            &format!("{field}.{index}.cache_control"),
        )?);
        if block.get("type").and_then(Value::as_str) == Some("tool_result") {
            count = count.saturating_add(validate_content_cache_controls(
                block.get("content").unwrap_or(&Value::Null),
                &format!("{field}.{index}.content"),
            )?);
        } else if block.get("type").and_then(Value::as_str) == Some("document")
            && block.pointer("/source/type").and_then(Value::as_str) == Some("content")
        {
            count = count.saturating_add(validate_content_cache_controls(
                block.pointer("/source/content").unwrap_or(&Value::Null),
                &format!("{field}.{index}.source.content"),
            )?);
        }
    }
    Ok(count)
}

fn validate_claude_content(
    content: &Value,
    role: &str,
    field: String,
) -> Result<(), ValidationError> {
    match content {
        Value::String(_) => return Ok(()),
        Value::Array(blocks) => {
            for (index, block) in blocks.iter().enumerate() {
                validate_claude_block(block, role, format!("{field}.{index}"), false)?;
            }
            let document_count = blocks.iter().map(count_claude_documents).sum::<usize>();
            if document_count > MAX_DOCUMENTS_PER_MESSAGE {
                return invalid(
                    field,
                    format!("must contain at most {MAX_DOCUMENTS_PER_MESSAGE} document blocks"),
                );
            }
            return Ok(());
        }
        _ => {}
    }
    invalid(field, "expected a string or content array")
}

fn validate_claude_block(
    block: &Value,
    role: &str,
    field: String,
    tool_result_content: bool,
) -> Result<(), ValidationError> {
    let Some(kind) = block.get("type").and_then(Value::as_str) else {
        return invalid(format!("{field}.type"), "is required");
    };
    match kind {
        "text" => {
            if !block.get("text").is_some_and(Value::is_string) {
                return invalid(format!("{field}.text"), "expected a string");
            }
        }
        "search_result" if role == "user" => {
            for name in ["source", "title"] {
                if !block.get(name).is_some_and(Value::is_string) {
                    return invalid(format!("{field}.{name}"), "expected a string");
                }
            }
            let Some(parts) = block.get("content").and_then(Value::as_array) else {
                return invalid(
                    format!("{field}.content"),
                    "expected an array of text blocks",
                );
            };
            for (index, part) in parts.iter().enumerate() {
                if part["type"] != "text" || !part.get("text").is_some_and(Value::is_string) {
                    return invalid(format!("{field}.content.{index}"), "expected a text block");
                }
            }
            if let Some(citations) = block.get("citations").filter(|value| !value.is_null()) {
                if !citations.is_object()
                    || citations
                        .get("enabled")
                        .is_some_and(|value| !value.is_boolean())
                {
                    return invalid(
                        format!("{field}.citations"),
                        "expected citation options with a boolean enabled field",
                    );
                }
            }
        }
        "image" => {
            if role != "user" {
                return invalid(format!("{field}.type"), "image blocks require a user role");
            }
            validate_image_transformations(block.get("transformations"), &field)?;
            validate_claude_image(block, &field)?;
        }
        "tool_result" if !tool_result_content => {
            if role != "user" {
                return invalid(
                    format!("{field}.type"),
                    "tool_result blocks require a user role",
                );
            }
            if block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return invalid(format!("{field}.tool_use_id"), "is required");
            }
            if block
                .get("is_error")
                .is_some_and(|value| !value.is_boolean())
            {
                return invalid(format!("{field}.is_error"), "expected a boolean");
            }
            match block.get("content") {
                None => {}
                Some(Value::String(_)) => {}
                Some(Value::Array(blocks)) => {
                    for (index, nested) in blocks.iter().enumerate() {
                        validate_claude_block(
                            nested,
                            "user",
                            format!("{field}.content.{index}"),
                            true,
                        )?;
                    }
                }
                _ => {
                    return invalid(
                        format!("{field}.content"),
                        "expected a string or an array of text/image/document blocks",
                    )
                }
            }
        }
        "tool_use" if !tool_result_content => {
            if role != "assistant" {
                return invalid(
                    format!("{field}.type"),
                    "tool_use blocks require an assistant role",
                );
            }
            for name in ["id", "name"] {
                if block
                    .get(name)
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                {
                    return invalid(format!("{field}.{name}"), "is required");
                }
            }
            if !block.get("input").is_some_and(Value::is_object) {
                return invalid(format!("{field}.input"), "expected an object");
            }
        }
        "server_tool_use" if !tool_result_content => {
            if role != "assistant" {
                return invalid(
                    format!("{field}.type"),
                    "server_tool_use blocks require an assistant role",
                );
            }
            for name in ["id", "name"] {
                if block
                    .get(name)
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                {
                    return invalid(format!("{field}.{name}"), "is required");
                }
            }
            if !block.get("input").is_some_and(Value::is_object) {
                return invalid(format!("{field}.input"), "expected an object");
            }
        }
        "tool_search_tool_result" if !tool_result_content => {
            if role != "assistant" {
                return invalid(
                    format!("{field}.type"),
                    "tool_search_tool_result blocks require an assistant role",
                );
            }
            if block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return invalid(format!("{field}.tool_use_id"), "is required");
            }
            validate_tool_search_result(block.get("content"), &format!("{field}.content"))?;
        }
        "web_search_tool_result" if !tool_result_content => {
            if role != "assistant" {
                return invalid(
                    format!("{field}.type"),
                    "web_search_tool_result blocks require an assistant role",
                );
            }
            if block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return invalid(format!("{field}.tool_use_id"), "is required");
            }
            if block
                .get("caller")
                .is_some_and(|caller| caller.get("type").and_then(Value::as_str) != Some("direct"))
            {
                return invalid(
                    format!("{field}.caller"),
                    "only direct Web Search callers are supported",
                );
            }
            validate_web_search_result(block.get("content"), &format!("{field}.content"))?;
        }
        "tool_reference" if tool_result_content => {
            if block
                .get("tool_name")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return invalid(format!("{field}.tool_name"), "is required");
            }
        }
        "thinking" if !tool_result_content => {
            if role != "assistant" || !block.get("thinking").is_some_and(Value::is_string) {
                return invalid(
                    format!("{field}.thinking"),
                    "thinking blocks require an assistant role and string content",
                );
            }
            if block
                .get("signature")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return invalid(
                    format!("{field}.signature"),
                    "signed thinking history requires the opaque signature returned by Claude",
                );
            }
        }
        "redacted_thinking" if !tool_result_content => {
            if role != "assistant"
                || block
                    .get("data")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
            {
                return invalid(
                    format!("{field}.data"),
                    "redacted_thinking blocks require an assistant role and non-empty opaque data",
                );
            }
        }
        "compaction" if !tool_result_content => {
            if role != "assistant" {
                return invalid(
                    format!("{field}.type"),
                    "compaction blocks require an assistant role",
                );
            }
            match block.get("content") {
                None | Some(Value::Null) => {}
                Some(Value::String(content)) if content.is_empty() => {
                    return invalid(format!("{field}.content"), "must not be empty");
                }
                Some(Value::String(_)) => {}
                _ => {
                    return invalid(format!("{field}.content"), "expected a string or null");
                }
            }
        }
        "document" => {
            if role != "user" {
                return invalid(
                    format!("{field}.type"),
                    "document blocks require a user role",
                );
            }
            validate_claude_document(block, &field)?;
        }
        _ => {
            return invalid(
                format!("{field}.type"),
                format!("unsupported Claude content block '{kind}'"),
            )
        }
    }
    Ok(())
}

fn count_claude_documents(block: &Value) -> usize {
    let own = usize::from(block.get("type").and_then(Value::as_str) == Some("document"));
    let nested = if block.get("type").and_then(Value::as_str) == Some("tool_result") {
        block
            .get("content")
            .and_then(Value::as_array)
            .map(|blocks| blocks.iter().map(count_claude_documents).sum())
            .unwrap_or_default()
    } else {
        0
    };
    own.saturating_add(nested)
}

fn count_claude_images(content: &Value) -> usize {
    content
        .as_array()
        .into_iter()
        .flatten()
        .map(|block| {
            usize::from(block.get("type").and_then(Value::as_str) == Some("image"))
                + if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                    count_claude_images(block.get("content").unwrap_or(&Value::Null))
                } else if block.get("type").and_then(Value::as_str) == Some("document")
                    && block.pointer("/source/type").and_then(Value::as_str) == Some("content")
                {
                    count_claude_images(block.pointer("/source/content").unwrap_or(&Value::Null))
                } else {
                    0
                }
        })
        .sum()
}

fn count_claude_documents_in_content(content: &Value) -> usize {
    content
        .as_array()
        .into_iter()
        .flatten()
        .map(count_claude_documents)
        .sum()
}

fn validate_claude_document(block: &Value, field: &str) -> Result<(), ValidationError> {
    for name in ["name", "title"] {
        if block
            .get(name)
            .is_some_and(|value| !value.is_null() && !value.is_string())
        {
            return invalid(format!("{field}.{name}"), "expected a string or null");
        }
    }
    if block
        .get("context")
        .is_some_and(|value| !value.is_null() && !value.is_string())
    {
        return invalid(format!("{field}.context"), "expected a string or null");
    }
    if let Some(citations) = block.get("citations").filter(|value| !value.is_null()) {
        let Some(citations) = citations.as_object() else {
            return invalid(format!("{field}.citations"), "expected an object");
        };
        if citations
            .get("enabled")
            .is_some_and(|enabled| !enabled.is_boolean())
        {
            return invalid(format!("{field}.citations.enabled"), "expected a boolean");
        }
        if let Some(unknown) = citations.keys().find(|key| key.as_str() != "enabled") {
            return invalid(
                format!("{field}.citations"),
                format!("unsupported citation configuration field '{unknown}'"),
            );
        }
    }
    let Some(source) = block.get("source").and_then(Value::as_object) else {
        return invalid(format!("{field}.source"), "expected an object");
    };
    let source_type = source.get("type").and_then(Value::as_str);
    if !matches!(source_type, Some("base64" | "text" | "url" | "content")) {
        return invalid(
            format!("{field}.source.type"),
            "expected base64, text, url, or content",
        );
    }
    if source_type == Some("content") {
        return validate_claude_content_document_source(source, field);
    }
    let allowed_source_fields: &[&str] = match source_type {
        Some("base64" | "text") => &["type", "media_type", "data"],
        Some("url") => &["type", "url"],
        _ => &[],
    };
    if let Some(unknown) = source
        .keys()
        .find(|key| !allowed_source_fields.contains(&key.as_str()))
    {
        return invalid(
            format!("{field}.source.{unknown}"),
            format!(
                "is not valid for a {} document source",
                source_type.unwrap_or("unknown")
            ),
        );
    }
    if source
        .get("media_type")
        .is_some_and(|value| !value.is_string())
    {
        return invalid(format!("{field}.source.media_type"), "expected a string");
    }
    let format = claude_document_format(block);
    if source_type != Some("url") && format.is_none() {
        return invalid(
            format!("{field}.source.media_type"),
            "expected PDF, CSV, DOC, DOCX, XLS, XLSX, HTML, TXT, or Markdown",
        );
    }
    if source_type == Some("text") && !matches!(format, Some("csv" | "md" | "html" | "txt")) {
        return invalid(
            format!("{field}.source.media_type"),
            "text sources require CSV, HTML, TXT, or Markdown content",
        );
    }
    if source_type == Some("url") {
        let Some(url) = source.get("url").and_then(Value::as_str) else {
            return invalid(format!("{field}.source.url"), "expected a string");
        };
        validate_http_attachment_url(url, &format!("{field}.source.url"))?;
        return Ok(());
    }
    let Some(data) = source.get("data").and_then(Value::as_str) else {
        return invalid(format!("{field}.source.data"), "expected a string");
    };
    if data.is_empty() {
        return invalid(format!("{field}.source.data"), "must not be empty");
    }
    let decoded = if source_type == Some("base64") {
        match base64::engine::general_purpose::STANDARD.decode(data) {
            Ok(decoded) => std::borrow::Cow::Owned(decoded),
            Err(_) => {
                return invalid(
                    format!("{field}.source.data"),
                    "invalid base64 document data",
                )
            }
        }
    } else {
        std::borrow::Cow::Borrowed(data.as_bytes())
    };
    if decoded.len() > MAX_DOCUMENT_BYTES {
        return invalid(
            format!("{field}.source.data"),
            format!("decoded document must be at most {MAX_DOCUMENT_BYTES} bytes"),
        );
    }
    if !document_bytes_match_format(format.expect("validated document format"), &decoded) {
        return invalid(
            format!("{field}.source.data"),
            "document bytes do not match the declared media_type",
        );
    }
    Ok(())
}

fn validate_claude_content_document_source(
    source: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<(), ValidationError> {
    if let Some(unknown) = source
        .keys()
        .find(|key| !matches!(key.as_str(), "type" | "content"))
    {
        return invalid(
            format!("{field}.source.{unknown}"),
            "is not valid for a content document source",
        );
    }
    let Some(content) = source.get("content") else {
        return invalid(format!("{field}.source.content"), "is required");
    };
    match content {
        Value::String(text) => {
            if text.is_empty() {
                return invalid(format!("{field}.source.content"), "must not be empty");
            }
            if text.len() > MAX_DOCUMENT_BYTES {
                return invalid(
                    format!("{field}.source.content"),
                    format!("document content must be at most {MAX_DOCUMENT_BYTES} bytes"),
                );
            }
        }
        Value::Array(blocks) => {
            if blocks.is_empty() {
                return invalid(format!("{field}.source.content"), "must not be empty");
            }
            let mut flattened_bytes = 0usize;
            let mut has_content = false;
            for (index, block) in blocks.iter().enumerate() {
                let block_field = format!("{field}.source.content.{index}");
                if index > 0 {
                    flattened_bytes = flattened_bytes.saturating_add(2);
                }
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(unknown) = block.as_object().and_then(|block| {
                            block.keys().find(|key| {
                                !matches!(
                                    key.as_str(),
                                    "type" | "text" | "cache_control" | "citations"
                                )
                            })
                        }) {
                            return invalid(
                                format!("{block_field}.{unknown}"),
                                "is not valid for a text block in a content document",
                            );
                        }
                        let Some(text) = block.get("text").and_then(Value::as_str) else {
                            return invalid(format!("{block_field}.text"), "expected a string");
                        };
                        flattened_bytes = flattened_bytes.saturating_add(text.len());
                        has_content |= !text.is_empty();
                        if block.get("citations").is_some_and(|value| !value.is_null()) {
                            return invalid(
                                format!("{block_field}.citations"),
                                "nested citation history cannot be represented by the Kiro upstream",
                            );
                        }
                    }
                    Some("image") => {
                        if let Some(unknown) = block.as_object().and_then(|block| {
                            block.keys().find(|key| {
                                !matches!(
                                    key.as_str(),
                                    "type" | "source" | "cache_control" | "transformations"
                                )
                            })
                        }) {
                            return invalid(
                                format!("{block_field}.{unknown}"),
                                "is not valid for an image block in a content document",
                            );
                        }
                        validate_image_transformations(block.get("transformations"), &block_field)?;
                        validate_claude_image(block, &block_field)?;
                        // The image is hoisted to Kiro's message-level image list and a
                        // short marker retains its relative place in the text document.
                        flattened_bytes = flattened_bytes.saturating_add(32);
                        has_content = true;
                    }
                    Some(kind) => {
                        return invalid(
                            format!("{block_field}.type"),
                            format!("unsupported content document block '{kind}'"),
                        )
                    }
                    None => return invalid(format!("{block_field}.type"), "is required"),
                }
                if flattened_bytes > MAX_DOCUMENT_BYTES {
                    return invalid(
                        format!("{field}.source.content"),
                        format!("flattened document must be at most {MAX_DOCUMENT_BYTES} bytes"),
                    );
                }
            }
            if !has_content {
                return invalid(format!("{field}.source.content"), "must not be empty");
            }
        }
        _ => {
            return invalid(
                format!("{field}.source.content"),
                "expected a string or an array of text/image blocks",
            )
        }
    }
    Ok(())
}

/// Validates the minimum file signature needed before forwarding document
/// bytes to Kiro. Legacy Word and Excel share the OLE container signature;
/// modern Office documents share the ZIP container signature.
pub fn document_bytes_match_format(format: &str, bytes: &[u8]) -> bool {
    match format {
        "pdf" => bytes
            .get(..bytes.len().min(1024))
            .is_some_and(|prefix| prefix.windows(5).any(|window| window == b"%PDF-")),
        "doc" | "xls" => bytes.starts_with(&[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]),
        "docx" | "xlsx" => {
            bytes.starts_with(b"PK\x03\x04")
                || bytes.starts_with(b"PK\x05\x06")
                || bytes.starts_with(b"PK\x07\x08")
        }
        "csv" | "md" | "html" | "txt" => {
            std::str::from_utf8(bytes).is_ok_and(|text| !text.contains('\0'))
        }
        _ => false,
    }
}

fn validate_web_search_result(value: Option<&Value>, field: &str) -> Result<(), ValidationError> {
    match value {
        Some(Value::Array(results)) => {
            for (index, result) in results.iter().enumerate() {
                if result.get("type").and_then(Value::as_str) != Some("web_search_result") {
                    return invalid(
                        format!("{field}.{index}.type"),
                        "expected web_search_result",
                    );
                }
                for name in ["url", "title"] {
                    if result
                        .get(name)
                        .and_then(Value::as_str)
                        .is_none_or(str::is_empty)
                    {
                        return invalid(format!("{field}.{index}.{name}"), "is required");
                    }
                }
                let Some(encrypted) = result
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                else {
                    return invalid(
                        format!("{field}.{index}.encrypted_content"),
                        "is required so web search results can be replayed on later turns",
                    );
                };
                // Anthropic-originated opaque values and crosslane-originated
                // replay payloads are both valid here. Only crosslane values can
                // be decoded locally; foreign opaque content is preserved.
                let _ = encrypted;
                if result
                    .get("page_age")
                    .is_some_and(|value| !value.is_null() && !value.is_string())
                {
                    return invalid(
                        format!("{field}.{index}.page_age"),
                        "expected a string or null",
                    );
                }
            }
            Ok(())
        }
        Some(Value::Object(error))
            if error.get("type").and_then(Value::as_str)
                == Some("web_search_tool_result_error") =>
        {
            if error
                .get("error_code")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return invalid(format!("{field}.error_code"), "is required");
            }
            Ok(())
        }
        _ => invalid(
            field,
            "expected web search result blocks or a web_search_tool_result_error object",
        ),
    }
}

fn validate_tool_search_result(value: Option<&Value>, field: &str) -> Result<(), ValidationError> {
    let Some(value) = value else {
        return invalid(field, "is required");
    };
    match value.get("type").and_then(Value::as_str) {
        Some("tool_search_tool_search_result") => {
            let Some(references) = value.get("tool_references").and_then(Value::as_array) else {
                return invalid(format!("{field}.tool_references"), "expected an array");
            };
            for (index, reference) in references.iter().enumerate() {
                if reference.get("type").and_then(Value::as_str) != Some("tool_reference")
                    || reference
                        .get("tool_name")
                        .and_then(Value::as_str)
                        .is_none_or(str::is_empty)
                {
                    return invalid(
                        format!("{field}.tool_references.{index}"),
                        "expected a tool_reference with tool_name",
                    );
                }
            }
            Ok(())
        }
        Some("tool_search_tool_result_error") => {
            if !value.get("error_code").is_some_and(Value::is_string) {
                return invalid(format!("{field}.error_code"), "is required");
            }
            if value
                .get("error_message")
                .is_some_and(|message| !message.is_null() && !message.is_string())
            {
                return invalid(
                    format!("{field}.error_message"),
                    "expected a string or null",
                );
            }
            Ok(())
        }
        _ => invalid(field, "expected a Tool Search result or error"),
    }
}

fn validate_claude_image(block: &Value, field: &str) -> Result<(), ValidationError> {
    let source = block.get("source").unwrap_or(&Value::Null);
    if source.get("type").and_then(Value::as_str) == Some("url") {
        if let Some(unknown) = source.as_object().and_then(|source| {
            source
                .keys()
                .find(|key| !matches!(key.as_str(), "type" | "url"))
        }) {
            return invalid(
                format!("{field}.source.{unknown}"),
                "is not valid for a URL image source",
            );
        }
        let Some(url) = source.get("url").and_then(Value::as_str) else {
            return invalid(format!("{field}.source.url"), "expected a string");
        };
        validate_http_attachment_url(url, &format!("{field}.source.url"))?;
        return Ok(());
    }
    if source.get("type").and_then(Value::as_str) != Some("base64") {
        return invalid(format!("{field}.source.type"), "expected base64 or url");
    }
    if let Some(unknown) = source.as_object().and_then(|source| {
        source
            .keys()
            .find(|key| !matches!(key.as_str(), "type" | "media_type" | "data"))
    }) {
        return invalid(
            format!("{field}.source.{unknown}"),
            "is not valid for a base64 image source",
        );
    }
    let media_type = source
        .get("media_type")
        .and_then(Value::as_str)
        .filter(|media| {
            matches!(
                *media,
                "image/jpeg" | "image/png" | "image/gif" | "image/webp"
            )
        });
    let Some(media_type) = media_type else {
        return invalid(
            format!("{field}.source.media_type"),
            "expected image/jpeg, image/png, image/gif, or image/webp",
        );
    };
    let Some(data) = source.get("data").and_then(Value::as_str) else {
        return invalid(format!("{field}.source.data"), "expected a string");
    };
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| ValidationError::InvalidField {
            field: format!("{field}.source.data"),
            message: "invalid base64 image data".into(),
        })?;
    if decoded.len() > MAX_IMAGE_BYTES {
        return invalid(
            format!("{field}.source.data"),
            format!("decoded image must be at most {MAX_IMAGE_BYTES} bytes"),
        );
    }
    if detected_image_media_type(&decoded) != Some(media_type) {
        return invalid(
            format!("{field}.source.data"),
            "image bytes do not match the declared media_type",
        );
    }
    Ok(())
}

fn validate_image_transformations(
    transformations: Option<&Value>,
    field: &str,
) -> Result<(), ValidationError> {
    let Some(transformations) = transformations else {
        return Ok(());
    };
    if transformations.is_null() {
        return Ok(());
    }
    let Some(transformations) = transformations.as_object() else {
        return invalid(
            format!("{field}.transformations"),
            "expected an object or null",
        );
    };
    if !transformations.is_empty() {
        return invalid(
            format!("{field}.transformations"),
            "image transformations are not supported by the Kiro upstream",
        );
    }
    Ok(())
}

fn validate_context_management(value: Option<&Value>) -> Result<(), ValidationError> {
    let Some(value) = value else {
        return Ok(());
    };
    let Some(object) = value.as_object() else {
        return invalid("context_management", "expected an object");
    };
    if let Some(unknown) = object.keys().find(|key| key.as_str() != "edits") {
        return invalid(
            format!("context_management.{unknown}"),
            "unsupported context management field",
        );
    }
    let Some(edits) = object.get("edits") else {
        return Ok(());
    };
    let Some(edits) = edits.as_array() else {
        return invalid("context_management.edits", "expected an array");
    };
    let mut known_families = HashSet::new();
    for (index, edit) in edits.iter().enumerate() {
        let Some(edit) = edit.as_object() else {
            return invalid(
                format!("context_management.edits.{index}"),
                "expected an object",
            );
        };
        let Some(edit_type) = edit.get("type").and_then(Value::as_str) else {
            return invalid(
                format!("context_management.edits.{index}.type"),
                "expected a string",
            );
        };
        if edit_type.trim().is_empty() {
            return invalid(
                format!("context_management.edits.{index}.type"),
                "must not be empty",
            );
        }
        let family = if crate::is_compact_edit_type(edit_type) {
            "compact"
        } else if matches_type_family(edit_type, "clear_thinking") {
            "clear_thinking"
        } else if matches_type_family(edit_type, "clear_tool_uses") {
            "clear_tool_uses"
        } else {
            // Preserve forward compatibility for edit families the proxy has
            // never claimed to emulate. Recognized families are strict below
            // so malformed controls can never become destructive no-ops.
            continue;
        };
        if !known_families.insert(family) {
            return invalid(
                format!("context_management.edits.{index}.type"),
                format!("duplicate {family} edit"),
            );
        }
        if family == "clear_thinking" {
            if index != 0 {
                return invalid(
                    format!("context_management.edits.{index}.type"),
                    "clear_thinking must be the first context edit",
                );
            }
            validate_clear_thinking_edit(edit, index)?;
            continue;
        }
        if family == "clear_tool_uses" {
            validate_clear_tool_uses_edit(edit, index)?;
            continue;
        }
        validate_compact_edit(edit, index)?;
    }
    Ok(())
}

fn validate_clear_thinking_edit(
    edit: &serde_json::Map<String, Value>,
    index: usize,
) -> Result<(), ValidationError> {
    if edit
        .keys()
        .any(|key| !matches!(key.as_str(), "type" | "keep"))
    {
        return invalid(
            format!("context_management.edits.{index}"),
            "contains an unsupported field",
        );
    }
    match edit.get("keep") {
        None => Ok(()),
        Some(Value::String(value)) if value == "all" => Ok(()),
        Some(Value::Object(value)) if value.get("type").and_then(Value::as_str) == Some("all") => {
            if value.keys().any(|key| key.as_str() != "type") {
                return invalid(
                    format!("context_management.edits.{index}.keep"),
                    "contains an unsupported field",
                );
            }
            Ok(())
        }
        Some(value) => validate_typed_count(
            value,
            &format!("context_management.edits.{index}.keep"),
            &["thinking_turns"],
            1,
        ),
    }
}

fn validate_clear_tool_uses_edit(
    edit: &serde_json::Map<String, Value>,
    index: usize,
) -> Result<(), ValidationError> {
    if edit.keys().any(|key| {
        !matches!(
            key.as_str(),
            "type" | "trigger" | "keep" | "clear_at_least" | "exclude_tools" | "clear_tool_inputs"
        )
    }) {
        return invalid(
            format!("context_management.edits.{index}"),
            "contains an unsupported field",
        );
    }
    if let Some(trigger) = edit.get("trigger") {
        validate_typed_count(
            trigger,
            &format!("context_management.edits.{index}.trigger"),
            &["input_tokens", "tool_uses"],
            1,
        )?;
    }
    if let Some(keep) = edit.get("keep") {
        validate_typed_count(
            keep,
            &format!("context_management.edits.{index}.keep"),
            &["tool_uses"],
            0,
        )?;
    }
    if let Some(clear_at_least) = edit.get("clear_at_least").filter(|value| !value.is_null()) {
        validate_typed_count(
            clear_at_least,
            &format!("context_management.edits.{index}.clear_at_least"),
            &["input_tokens"],
            1,
        )?;
    }
    if let Some(clear_tool_inputs) = edit
        .get("clear_tool_inputs")
        .filter(|value| !value.is_null())
    {
        match clear_tool_inputs {
            Value::Bool(_) => {}
            Value::Array(names)
                if names
                    .iter()
                    .all(|name| name.as_str().is_some_and(|name| !name.trim().is_empty())) => {}
            _ => {
                return invalid(
                    format!("context_management.edits.{index}.clear_tool_inputs"),
                    "expected a boolean, null, or an array of non-empty tool names",
                )
            }
        }
    }
    if let Some(excluded) = edit.get("exclude_tools").filter(|value| !value.is_null()) {
        let Some(excluded) = excluded.as_array() else {
            return invalid(
                format!("context_management.edits.{index}.exclude_tools"),
                "expected an array of non-empty strings",
            );
        };
        if excluded
            .iter()
            .any(|value| value.as_str().is_none_or(|value| value.trim().is_empty()))
        {
            return invalid(
                format!("context_management.edits.{index}.exclude_tools"),
                "expected an array of non-empty strings",
            );
        }
    }
    Ok(())
}

fn validate_typed_count(
    value: &Value,
    field: &str,
    allowed_types: &[&str],
    minimum: u64,
) -> Result<(), ValidationError> {
    let Some(value) = value.as_object() else {
        return invalid(field, "expected an object");
    };
    if value
        .get("type")
        .and_then(Value::as_str)
        .is_none_or(|kind| !allowed_types.contains(&kind))
    {
        return invalid(
            format!("{field}.type"),
            format!("expected {}", allowed_types.join(" or ")),
        );
    }
    if value
        .get("value")
        .and_then(Value::as_u64)
        .is_none_or(|value| value < minimum)
    {
        return invalid(
            format!("{field}.value"),
            format!("must be an integer of at least {minimum}"),
        );
    }
    if value
        .keys()
        .any(|key| !matches!(key.as_str(), "type" | "value"))
    {
        return invalid(field, "contains an unsupported field");
    }
    Ok(())
}

fn validate_compact_edit(
    edit: &serde_json::Map<String, Value>,
    index: usize,
) -> Result<(), ValidationError> {
    if edit.keys().any(|key| {
        !matches!(
            key.as_str(),
            "type" | "trigger" | "pause_after_compaction" | "instructions"
        )
    }) {
        return invalid(
            format!("context_management.edits.{index}"),
            "contains an unsupported field",
        );
    }
    if edit
        .get("pause_after_compaction")
        .is_some_and(|value| !value.is_boolean())
    {
        return invalid(
            format!("context_management.edits.{index}.pause_after_compaction"),
            "expected a boolean",
        );
    }
    if edit.get("pause_after_compaction").and_then(Value::as_bool) == Some(true) {
        return invalid(
            format!("context_management.edits.{index}.pause_after_compaction"),
            "pausing after compaction is not supported by the Kiro upstream",
        );
    }
    if edit
        .get("instructions")
        .is_some_and(|value| !value.is_string())
    {
        return invalid(
            format!("context_management.edits.{index}.instructions"),
            "expected a string",
        );
    }
    if edit
        .get("instructions")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
    {
        return invalid(
            format!("context_management.edits.{index}.instructions"),
            "custom compaction instructions are not supported by the Kiro upstream",
        );
    }
    if let Some(trigger) = edit.get("trigger") {
        let Some(trigger) = trigger.as_object() else {
            return invalid(
                format!("context_management.edits.{index}.trigger"),
                "expected an object",
            );
        };
        if trigger.get("type").and_then(Value::as_str) != Some("input_tokens") {
            return invalid(
                format!("context_management.edits.{index}.trigger.type"),
                "expected input_tokens",
            );
        }
        let minimum = crate::MIN_COMPACT_TRIGGER_TOKENS;
        if trigger
            .get("value")
            .and_then(Value::as_u64)
            .is_none_or(|value| value < minimum)
        {
            return invalid(
                format!("context_management.edits.{index}.trigger.value"),
                format!("must be an integer of at least {minimum}"),
            );
        }
        if trigger
            .keys()
            .any(|key| !matches!(key.as_str(), "type" | "value"))
        {
            return invalid(
                format!("context_management.edits.{index}.trigger"),
                "contains an unsupported field",
            );
        }
    }
    Ok(())
}

pub fn validate_openai(request: &OpenAiRequest) -> Result<(), ValidationError> {
    common(&request.model, request.messages.is_empty())?;
    if request.n.is_some_and(|n| !(1..=128).contains(&n)) {
        return invalid("n", "must be in 1..=128");
    }
    if let Some(stop) = &request.stop {
        let sequences = match stop {
            Value::String(value) => vec![value.as_str()],
            Value::Array(values) if values.len() <= 4 && values.iter().all(Value::is_string) => {
                values.iter().filter_map(Value::as_str).collect()
            }
            _ => return invalid("stop", "expected a string or an array of at most 4 strings"),
        };
        if sequences
            .iter()
            .any(|value| value.is_empty() || value.len() > MAX_STOP_SEQUENCE_BYTES)
        {
            return invalid(
                "stop",
                format!("each sequence must contain 1..={MAX_STOP_SEQUENCE_BYTES} bytes"),
            );
        }
    }
    let mut cache_breakpoints = 0usize;
    if request
        .temperature
        .is_some_and(|value| !(0.0..=2.0).contains(&value))
    {
        return invalid("temperature", "must be in 0..=2");
    }
    if request
        .top_p
        .is_some_and(|value| !(0.0..=1.0).contains(&value))
    {
        return invalid("top_p", "must be in 0..=1");
    }
    if request.max_tokens == Some(0) || request.max_completion_tokens == Some(0) {
        return invalid("max_tokens", "token limits must be positive");
    }
    if let Some(options) = &request.stream_options {
        let Some(options) = options.as_object() else {
            return invalid("stream_options", "expected an object");
        };
        if options
            .get("include_usage")
            .is_some_and(|value| !value.is_null() && !value.is_boolean())
        {
            return invalid("stream_options", "include_usage must be a boolean");
        }
    }
    let mut image_count = 0usize;
    let mut document_count = 0usize;
    for (index, message) in request.messages.iter().enumerate() {
        if !matches!(
            message.role.as_str(),
            "system" | "developer" | "user" | "assistant" | "tool"
        ) {
            return Err(ValidationError::InvalidRole(message.role.clone()));
        }
        image_count = image_count.saturating_add(validate_openai_message(message, index)?);
        cache_breakpoints = cache_breakpoints.saturating_add(validate_cache_control(
            message.cache_control.as_ref(),
            &format!("messages.{index}.cache_control"),
        )?);
        if let Some(content) = message.content.as_ref() {
            document_count += count_claude_documents_in_content(content);
            cache_breakpoints = cache_breakpoints.saturating_add(validate_content_cache_controls(
                content,
                &format!("messages.{index}.content"),
            )?);
        }
    }
    if image_count > MAX_IMAGES_PER_REQUEST {
        return invalid(
            "messages",
            format!("must contain at most {MAX_IMAGES_PER_REQUEST} image parts"),
        );
    }
    if request.tools.len() > MAX_TOOLS {
        return Err(ValidationError::TooManyTools);
    }
    if document_count > MAX_DOCUMENTS_PER_REQUEST {
        return invalid(
            "messages",
            format!("must contain at most {MAX_DOCUMENTS_PER_REQUEST} file parts"),
        );
    }
    let mut docs = 0usize;
    for (index, tool) in request.tools.iter().enumerate() {
        if !matches!(tool.r#type.as_str(), "function" | "custom") {
            return invalid(format!("tools.{index}.type"), "expected function or custom");
        }
        let definition = tool.body.get(&tool.r#type).unwrap_or(&Value::Null);
        cache_breakpoints = cache_breakpoints.saturating_add(validate_cache_control(
            tool.body.get("cache_control"),
            &format!("tools.{index}.cache_control"),
        )?);
        if !definition.is_object() {
            return invalid(
                format!("tools.{index}.{}", tool.r#type),
                "expected an object",
            );
        }
        let name = definition
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if name.trim().is_empty() {
            return invalid(
                format!("tools.{index}.{}.name", tool.r#type),
                "must not be empty",
            );
        }
        let empty_schema = Value::Object(serde_json::Map::new());
        let schema = definition.get("parameters").unwrap_or(&empty_schema);
        docs += definition
            .get("description")
            .and_then(Value::as_str)
            .map_or(0, |description| description.chars().count());
        if definition
            .get("description")
            .is_some_and(|description| !description.is_string())
        {
            return invalid(
                format!("tools.{index}.{}.description", tool.r#type),
                "expected a string",
            );
        }
        validate_tool(name, schema).map_err(|error| {
            if error == ValidationError::ToolNameTooLong {
                error
            } else {
                ValidationError::InvalidField {
                    field: format!("tools.{index}.{}.parameters", tool.r#type),
                    message: error.to_string(),
                }
            }
        })?;
        if tool.r#type == "function" {
            if definition
                .get("parameters")
                .is_some_and(|value| !value.is_object())
            {
                return invalid(
                    format!("tools.{index}.function.parameters"),
                    "expected an object",
                );
            }
            if definition
                .get("strict")
                .is_some_and(|strict| !strict.is_null() && !strict.is_boolean())
            {
                return invalid(
                    format!("tools.{index}.function.strict"),
                    "expected a boolean",
                );
            }
        } else {
            validate_custom_format(definition.get("format"), index)?;
            docs += definition
                .pointer("/format/grammar/definition")
                .and_then(Value::as_str)
                .map_or(0, |grammar| grammar.chars().count());
        }
    }
    if docs > MAX_TOOL_DOC_CHARS {
        return Err(ValidationError::ToolDocumentationTooLarge);
    }
    if cache_breakpoints > MAX_CACHE_BREAKPOINTS {
        return invalid(
            "cache_control",
            format!("must define at most {MAX_CACHE_BREAKPOINTS} cache breakpoints"),
        );
    }
    validate_openai_tool_choice(request)?;
    if request.reasoning_effort.as_deref().is_some_and(|effort| {
        !matches!(
            effort,
            "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
        )
    }) {
        return invalid(
            "reasoning_effort",
            "expected none, minimal, low, medium, high, xhigh, or max",
        );
    }
    if request
        .conversation_id
        .as_deref()
        .is_some_and(|id| id.trim().is_empty() || id.len() > 256)
    {
        return invalid(
            "conversation_id",
            "must be a non-empty string of at most 256 bytes",
        );
    }
    if request
        .metadata
        .as_ref()
        .is_some_and(|metadata| !metadata.is_object())
    {
        return invalid("metadata", "expected an object");
    }
    validate_thinking(request.thinking.as_ref(), request.output_token_limit())?;
    Ok(())
}

fn validate_openai_message(
    message: &crate::OpenAiMessage,
    index: usize,
) -> Result<usize, ValidationError> {
    let mut image_count = 0usize;
    if message.role == "tool"
        && message
            .tool_call_id
            .as_deref()
            .is_none_or(|id| id.trim().is_empty())
    {
        return invalid(
            format!("messages.{index}.tool_call_id"),
            "is required for tool messages",
        );
    }
    if message.content.is_none()
        && (message.role != "assistant"
            || (message.tool_calls.is_empty() && message.refusal.is_none()))
    {
        return invalid(format!("messages.{index}.content"), "is required");
    }
    if let Some(content) = &message.content {
        match content {
            Value::String(_) => {}
            Value::Array(parts) => {
                for (part_index, part) in parts.iter().enumerate() {
                    let kind = part.get("type").and_then(Value::as_str);
                    let field = format!("messages.{index}.content.{part_index}");
                    match kind {
                        Some("text") if part.get("text").is_some_and(Value::is_string) => {}
                        Some("refusal")
                            if message.role == "assistant"
                                && part.get("refusal").is_some_and(Value::is_string) => {}
                        Some("document") if matches!(message.role.as_str(), "user" | "tool") => {
                            validate_claude_document(part, &field)?;
                        }
                        Some("image_url") => {
                            let Some(url) = part.pointer("/image_url/url").and_then(Value::as_str)
                            else {
                                return invalid(
                                    format!("{field}.image_url.url"),
                                    "expected a string",
                                );
                            };
                            validate_openai_image_url(url, &format!("{field}.image_url.url"))?;
                            image_count = image_count.saturating_add(1);
                        }
                        _ => return invalid(field, "expected a text or image_url content part"),
                    }
                }
            }
            _ => {
                return invalid(
                    format!("messages.{index}.content"),
                    "expected a string or content array",
                )
            }
        }
    }
    for (tool_index, call) in message.tool_calls.iter().enumerate() {
        let field = format!("messages.{index}.tool_calls.{tool_index}");
        let kind = call.get("type").and_then(Value::as_str);
        if !matches!(kind, Some("function" | "custom"))
            || call
                .get("id")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        {
            return invalid(
                field,
                "expected a named function or custom tool call with an id",
            );
        }
        let definition = call.get(kind.unwrap_or_default()).unwrap_or(&Value::Null);
        if definition
            .get("name")
            .and_then(Value::as_str)
            .is_none_or(|name| name.trim().is_empty())
        {
            return invalid(format!("{field}.name"), "tool name is required");
        }
        if kind == Some("function") {
            let Some(arguments) = definition.get("arguments").and_then(Value::as_str) else {
                return invalid(
                    format!("{field}.function.arguments"),
                    "expected a JSON string",
                );
            };
            if serde_json::from_str::<Value>(arguments).is_err() {
                return invalid(
                    format!("{field}.function.arguments"),
                    "contains invalid JSON",
                );
            }
        } else if !definition.get("input").is_some_and(Value::is_string) {
            return invalid(format!("{field}.custom.input"), "expected a string");
        }
    }
    Ok(image_count)
}

fn validate_http_attachment_url(value: &str, field: &str) -> Result<(), ValidationError> {
    let url = Url::parse(value).map_err(|_| ValidationError::InvalidField {
        field: field.into(),
        message: "expected a valid HTTP(S) URL".into(),
    })?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return invalid(
            field,
            "expected an HTTP(S) URL with a host and without credentials",
        );
    }
    Ok(())
}

fn validate_openai_image_url(value: &str, field: &str) -> Result<(), ValidationError> {
    if Url::parse(value).is_ok_and(|url| matches!(url.scheme(), "http" | "https")) {
        return validate_http_attachment_url(value, field);
    }
    let Some(encoded) = value.strip_prefix("data:image/") else {
        return invalid(field, "expected an HTTP(S) URL or base64 image data URL");
    };
    let Some((format, data)) = encoded.split_once(";base64,") else {
        return invalid(field, "expected a base64 image data URL");
    };
    if !matches!(
        format.to_ascii_lowercase().as_str(),
        "jpeg" | "jpg" | "png" | "gif" | "webp"
    ) {
        return invalid(field, "expected a JPEG, PNG, GIF, or WebP image");
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| ValidationError::InvalidField {
            field: field.into(),
            message: "invalid base64 image data".into(),
        })?;
    if decoded.is_empty() {
        return invalid(field, "image data must not be empty");
    }
    if decoded.len() > MAX_IMAGE_BYTES {
        return invalid(
            field,
            format!("decoded image must be at most {MAX_IMAGE_BYTES} bytes"),
        );
    }
    let declared = match format.to_ascii_lowercase().as_str() {
        "jpeg" | "jpg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => unreachable!("format checked above"),
    };
    if detected_image_media_type(&decoded) != Some(declared) {
        return invalid(field, "image bytes do not match the data URL media type");
    }
    Ok(())
}

fn detected_image_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn validate_custom_format(format: Option<&Value>, index: usize) -> Result<(), ValidationError> {
    let Some(format) = format else {
        return Ok(());
    };
    match format.get("type").and_then(Value::as_str) {
        Some("text") => Ok(()),
        Some("grammar") => {
            let grammar = format.get("grammar").unwrap_or(&Value::Null);
            if grammar.get("definition").is_some_and(Value::is_string)
                && grammar
                    .get("syntax")
                    .and_then(Value::as_str)
                    .is_some_and(|syntax| matches!(syntax, "lark" | "regex"))
            {
                Ok(())
            } else {
                invalid(
                    format!("tools.{index}.custom.format.grammar"),
                    "requires a definition and lark or regex syntax",
                )
            }
        }
        _ => invalid(
            format!("tools.{index}.custom.format.type"),
            "expected text or grammar",
        ),
    }
}

fn validate_thinking(
    thinking: Option<&crate::ThinkingConfig>,
    max_tokens: Option<u32>,
) -> Result<(), ValidationError> {
    let Some(thinking) = thinking else {
        return Ok(());
    };
    if !matches!(
        thinking.r#type.as_str(),
        "enabled" | "adaptive" | "disabled"
    ) {
        return invalid("thinking.type", "expected enabled, adaptive, or disabled");
    }
    if thinking.r#type == "enabled" && thinking.budget_tokens.unwrap_or_default() < 1_024 {
        return invalid(
            "thinking.budget_tokens",
            "enabled thinking requires at least 1024 tokens",
        );
    }
    if thinking.r#type == "enabled"
        && thinking
            .budget_tokens
            .is_some_and(|budget| max_tokens.is_some_and(|maximum| budget >= maximum))
    {
        return invalid("thinking.budget_tokens", "must be less than max_tokens");
    }
    if thinking.r#type != "enabled" && thinking.budget_tokens.is_some() {
        return invalid(
            "thinking.budget_tokens",
            "budget_tokens is only valid for enabled thinking",
        );
    }
    if thinking
        .display
        .as_deref()
        .is_some_and(|display| !matches!(display, "summarized" | "omitted"))
    {
        return invalid("thinking.display", "expected summarized or omitted");
    }
    if thinking.r#type == "disabled" && thinking.display.is_some() {
        return invalid(
            "thinking.display",
            "display is not valid for disabled thinking",
        );
    }
    Ok(())
}

fn validate_openai_tool_choice(request: &OpenAiRequest) -> Result<(), ValidationError> {
    let Some(choice) = request.tool_choice.as_ref() else {
        return Ok(());
    };
    if choice
        .as_str()
        .is_some_and(|value| matches!(value, "none" | "auto"))
    {
        return Ok(());
    }
    if choice.as_str() == Some("required") {
        return if request.tools.is_empty() {
            invalid("tool_choice", "required requires at least one tool")
        } else {
            Ok(())
        };
    }
    for kind in ["function", "custom"] {
        if let Some(name) = choice
            .get(kind)
            .and_then(|definition| definition.get("name"))
            .and_then(Value::as_str)
        {
            if choice.get("type").and_then(Value::as_str) != Some(kind) {
                return invalid(
                    "tool_choice.type",
                    format!("expected '{kind}' for a {kind} tool choice"),
                );
            }
            let found = request.tools.iter().any(|tool| {
                tool.r#type == kind
                    && tool
                        .body
                        .get(kind)
                        .and_then(|definition| definition.get("name"))
                        .and_then(Value::as_str)
                        == Some(name)
            });
            return if found {
                Ok(())
            } else {
                invalid(
                    format!("tool_choice.{kind}.name"),
                    format!("tool '{name}' was not found"),
                )
            };
        }
    }
    if choice.get("type").and_then(Value::as_str) == Some("allowed_tools") {
        let allowed = choice.get("allowed_tools").unwrap_or(&Value::Null);
        if !allowed
            .get("mode")
            .and_then(Value::as_str)
            .is_some_and(|mode| matches!(mode, "auto" | "required"))
        {
            return invalid(
                "tool_choice.allowed_tools.mode",
                "expected auto or required",
            );
        }
        let Some(tools) = allowed.get("tools").and_then(Value::as_array) else {
            return invalid(
                "tool_choice.allowed_tools.tools",
                "expected a non-empty array",
            );
        };
        if tools.is_empty() {
            return invalid(
                "tool_choice.allowed_tools.tools",
                "expected a non-empty array",
            );
        }
        let mut seen = std::collections::HashSet::new();
        for (index, reference) in tools.iter().enumerate() {
            let Some(kind) = reference
                .get("type")
                .and_then(Value::as_str)
                .filter(|kind| matches!(*kind, "function" | "custom"))
            else {
                return invalid(
                    format!("tool_choice.allowed_tools.tools.{index}.type"),
                    "expected function or custom",
                );
            };
            let Some(name) = reference
                .get(kind)
                .and_then(|definition| definition.get("name"))
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
            else {
                return invalid(
                    format!("tool_choice.allowed_tools.tools.{index}.{kind}.name"),
                    "tool name is required",
                );
            };
            let identity = format!("{kind}:{name}");
            if !seen.insert(identity) {
                return invalid(
                    format!("tool_choice.allowed_tools.tools.{index}.{kind}.name"),
                    "duplicate allowed tool",
                );
            }
            let found = request.tools.iter().any(|tool| {
                tool.r#type == kind
                    && tool
                        .body
                        .get(kind)
                        .and_then(|definition| definition.get("name"))
                        .and_then(Value::as_str)
                        == Some(name)
            });
            if !found {
                return invalid(
                    format!("tool_choice.allowed_tools.tools.{index}.{kind}.name"),
                    format!("tool '{name}' was not found"),
                );
            }
        }
        return Ok(());
    }
    invalid("tool_choice", "unsupported tool choice")
}

fn invalid<T>(field: impl Into<String>, message: impl Into<String>) -> Result<T, ValidationError> {
    Err(ValidationError::InvalidField {
        field: field.into(),
        message: message.into(),
    })
}

fn common(model: &str, messages_empty: bool) -> Result<(), ValidationError> {
    validate_model(model)?;
    if messages_empty {
        return Err(ValidationError::MissingMessages);
    }
    Ok(())
}

/// Check the identifier's resource/logging boundary without enumerating model
/// names or rejecting otherwise valid custom routing aliases.
pub fn validate_model(model: &str) -> Result<(), ValidationError> {
    if model.trim().is_empty() {
        return Err(ValidationError::MissingModel);
    }
    if model.len() > MAX_MODEL_BYTES {
        return invalid("model", format!("must be at most {MAX_MODEL_BYTES} bytes"));
    }
    if model
        .chars()
        .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return invalid("model", "must not contain whitespace or control characters");
    }
    Ok(())
}

fn validate_tool(name: &str, schema: &Value) -> Result<(), ValidationError> {
    if name.chars().count() > MAX_TOOL_NAME_CHARS {
        return Err(ValidationError::ToolNameTooLong);
    }
    let mut stack = vec![(schema, 1usize)];
    let mut nodes = 0usize;
    while let Some((value, depth)) = stack.pop() {
        nodes += 1;
        if nodes > MAX_SCHEMA_NODES {
            return Err(ValidationError::SchemaTooLarge);
        }
        if depth > MAX_SCHEMA_DEPTH {
            return Err(ValidationError::SchemaTooDeep);
        }
        match value {
            Value::Array(values) => {
                stack.extend(values.iter().map(|child| (child, depth + 1)));
            }
            Value::Object(values) => {
                stack.extend(values.values().map(|child| (child, depth + 1)));
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
