use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};

use crate::{
    KiroCachePoint, KiroCitationsConfig, KiroDocument, KiroDocumentSource, KiroImage,
    KiroImageSource, KiroInferenceConfig, KiroMessageContext, KiroText, KiroTool, KiroToolResult,
    KiroToolSpecification, KiroUserInputMessage,
};

const MAX_KIRO_TOOL_NAME: usize = 64;
const MAX_KIRO_DESCRIPTION: usize = 1024;

/// Log only known field names, never client schemas or arbitrary hint values.
pub(crate) fn log_ignored_controls(protocol: &str, controls: &[(&str, bool)]) {
    let fields: Vec<_> = controls
        .iter()
        .filter_map(|(field, present)| present.then_some(*field))
        .collect();
    if !fields.is_empty() {
        tracing::debug!(
            event = "proxy.compatibility.controls_ignored",
            protocol,
            fields = ?fields,
            "accepting client hints without forwarding them, matching the reference Kiro gateways"
        );
    }
}

/// Tool definitions belong only on Kiro's current message. Keeping them on
/// copied history turns multiplies a large catalog on every internal server
/// tool continuation and can exceed the upstream payload budget.
pub(crate) fn history_user_without_tools(
    mut message: KiroUserInputMessage,
) -> KiroUserInputMessage {
    if let Some(context) = message.user_input_message_context.as_mut() {
        context.tools.clear();
        context.env_state = None;
        if context.tool_results.is_empty() {
            message.user_input_message_context = None;
        }
    }
    message
}

/// Request-scoped, reversible mapping between client tool names and Kiro's
/// restricted 64-byte name space. Building it for the whole catalog avoids
/// silent aliasing when two legal client names normalize to the same value.
#[derive(Debug, Clone, Default)]
pub struct ToolNameRegistry {
    original_to_kiro: HashMap<String, String>,
    kiro_to_original: HashMap<String, String>,
}

impl ToolNameRegistry {
    pub fn new<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let originals = names
            .into_iter()
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        let mut base_counts = HashMap::<String, usize>::new();
        for original in &originals {
            *base_counts.entry(tool_name(original)).or_default() += 1;
        }

        let mut registry = Self::default();
        for original in originals {
            let base = tool_name(&original);
            let needs_hash = base_counts.get(&base).copied().unwrap_or_default() > 1
                || registry.kiro_to_original.contains_key(&base);
            let mut nonce = 0u32;
            let kiro = loop {
                let candidate = if needs_hash || nonce > 0 {
                    collision_tool_name(&original, nonce)
                } else {
                    base.clone()
                };
                if !registry.kiro_to_original.contains_key(&candidate) {
                    break candidate;
                }
                nonce = nonce.saturating_add(1);
            };
            registry
                .original_to_kiro
                .insert(original.clone(), kiro.clone());
            registry.kiro_to_original.insert(kiro, original);
        }
        registry
    }

    pub fn kiro_name(&self, original: &str) -> String {
        self.original_to_kiro
            .get(original)
            .cloned()
            .unwrap_or_else(|| tool_name(original))
    }

    pub fn contains_original(&self, original: &str) -> bool {
        self.original_to_kiro.contains_key(original)
    }

    pub fn restore_map(&self) -> HashMap<String, String> {
        self.kiro_to_original.clone()
    }
}

pub fn system_text(system: Option<&Value>) -> String {
    match system {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub fn content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") | Some("thinking") | Some("compaction") => block
                    .get("text")
                    .or_else(|| block.get("thinking"))
                    .or_else(|| block.get("content"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                Some("refusal") => block.get("refusal").and_then(Value::as_str).map(str::to_owned),
                Some("search_result") => Some(format!("Client-provided search result (source data):\n{}", serde_json::json!({
                    "source":block.get("source"), "title":block.get("title"),
                    "content":block.get("content").and_then(Value::as_array).into_iter().flatten().filter_map(|part| part.get("text").and_then(Value::as_str)).collect::<Vec<_>>()
                }))),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub fn extract_images(content: &Value) -> Vec<KiroImage> {
    let mut images = Vec::new();
    collect_claude_images(content, &mut images);
    images
}

pub fn kiro_cache_point(control: Option<&Value>) -> Option<KiroCachePoint> {
    let control = control?;
    // Kiro's request contract only defines `type: default`. Claude's TTL is
    // still available to proxy-local accounting, but is never sent upstream.
    // Null and unrecognized client cache hints do not request a native marker.
    (control.get("type").and_then(Value::as_str) == Some("ephemeral")).then(KiroCachePoint::new)
}

/// Preserve client document context as message text from the first request;
/// it is not a native Kiro document field and must not require a 400 retry.
pub fn document_context_text(content: &Value) -> String {
    fn collect(content: &Value, contexts: &mut Vec<Value>) {
        for block in content.as_array().into_iter().flatten() {
            match block.get("type").and_then(Value::as_str) {
                Some("document") => {
                    if let Some(context) = block
                        .get("context")
                        .and_then(Value::as_str)
                        .filter(|context| !context.trim().is_empty())
                    {
                        contexts.push(serde_json::json!({
                            "document_name":neutral_document_name(
                                claude_document_label(block),
                                claude_document_format(block).unwrap_or("txt")
                            ),
                            "context":context
                        }));
                    }
                }
                Some("tool_result") => {
                    if let Some(content) = block.get("content") {
                        collect(content, contexts);
                    }
                }
                _ => {}
            }
        }
    }
    let mut contexts = Vec::new();
    collect(content, &mut contexts);
    if contexts.is_empty() {
        return String::new();
    }
    format!(
        "Client-provided document context (JSON; separate from document contents):\n{}",
        serde_json::to_string(&contexts).expect("document context is JSON-serializable")
    )
}

/// Kiro exposes one cache marker per conversation message. Claude can place
/// it on an individual block, so retain the last marker in that message—the
/// one that covers the longest prefix.
pub fn content_cache_point(content: &Value) -> Option<KiroCachePoint> {
    match content {
        Value::Array(blocks) => blocks.iter().filter_map(block_cache_point).next_back(),
        Value::Object(_) => kiro_cache_point(content.get("cache_control")),
        _ => None,
    }
}

fn block_cache_point(block: &Value) -> Option<KiroCachePoint> {
    let nested = match block.get("type").and_then(Value::as_str) {
        Some("tool_result" | "search_result") => block.get("content").and_then(content_cache_point),
        Some("document")
            if block.pointer("/source/type").and_then(Value::as_str) == Some("content") =>
        {
            block
                .pointer("/source/content")
                .and_then(content_cache_point)
        }
        _ => None,
    };
    merged_cache_point(nested, kiro_cache_point(block.get("cache_control")))
}

pub fn merged_cache_point(
    first: Option<KiroCachePoint>,
    second: Option<KiroCachePoint>,
) -> Option<KiroCachePoint> {
    second.or(first)
}

pub fn extract_documents(content: &Value) -> Vec<KiroDocument> {
    let mut documents = Vec::new();
    let mut image_index = 0;
    collect_claude_documents(content, &mut documents, &mut image_index);
    documents
}

fn collect_claude_documents(
    content: &Value,
    documents: &mut Vec<KiroDocument>,
    image_index: &mut usize,
) {
    let Some(blocks) = content.as_array() else {
        return;
    };
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("image") => *image_index = image_index.saturating_add(1),
            Some("document") => {
                if let Some(document) = claude_document(block, image_index) {
                    documents.push(document);
                }
            }
            Some("tool_result") => {
                if let Some(content) = block.get("content") {
                    collect_claude_documents(content, documents, image_index);
                }
            }
            _ => {}
        }
    }
}

fn claude_document(block: &Value, image_index: &mut usize) -> Option<KiroDocument> {
    let source = block.get("source")?;
    let source_type = source.get("type")?.as_str()?;
    let bytes = match source_type {
        "base64" => {
            let data = source.get("data")?.as_str()?;
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .ok()?;
            data.to_owned()
        }
        "text" => base64::engine::general_purpose::STANDARD
            .encode(source.get("data")?.as_str()?.as_bytes()),
        "content" => base64::engine::general_purpose::STANDARD
            .encode(custom_document_text(source.get("content")?, image_index).as_bytes()),
        _ => return None,
    };
    let format = claude_document_format(block)?;
    let name = neutral_document_name(claude_document_label(block), format);
    Some(KiroDocument {
        format: format.into(),
        name,
        source: KiroDocumentSource { bytes },
        citations: block
            .pointer("/citations/enabled")
            .and_then(Value::as_bool)
            .map(|enabled| KiroCitationsConfig { enabled }),
    })
}

fn custom_document_text(content: &Value, image_index: &mut usize) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") => block.get("text").and_then(Value::as_str).map(str::to_owned),
                Some("image") => {
                    *image_index = image_index.saturating_add(1);
                    Some(format!("[Message image {image_index}]"))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

/// Bedrock treats document names as prompt-bearing input and accepts only a
/// small neutral character set. Keep a recognizable stem while removing file
/// extensions, control characters, repeated whitespace, and instruction-like
/// punctuation before the name reaches Kiro.
fn neutral_document_name(label: Option<&str>, format: &str) -> String {
    let label = label
        .and_then(|label| {
            let trimmed = label.trim();
            let suffix = format!(".{format}");
            trimmed
                .to_ascii_lowercase()
                .ends_with(&suffix)
                .then(|| &trimmed[..trimmed.len().saturating_sub(suffix.len())])
                .or(Some(trimmed))
        })
        .unwrap_or("document");
    let mut output = String::new();
    let mut previous_space = false;
    for character in label.chars() {
        if output.chars().count() >= 200 {
            break;
        }
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '(' | ')' | '[' | ']') {
            output.push(character);
            previous_space = false;
        } else if character.is_whitespace() {
            if !previous_space && !output.is_empty() {
                output.push(' ');
                previous_space = true;
            }
        } else if !output.ends_with('-') && !output.is_empty() {
            output.push('-');
            previous_space = false;
        }
    }
    let output = output.trim_matches([' ', '-']).to_owned();
    if output.is_empty() {
        "document".into()
    } else {
        output
    }
}

pub(crate) fn claude_document_format(block: &Value) -> Option<&'static str> {
    let source = block.get("source")?;
    if source.get("type").and_then(Value::as_str) == Some("content") {
        return Some("txt");
    }
    source
        .get("media_type")
        .and_then(Value::as_str)
        .and_then(document_format_from_media_type)
        .or_else(|| {
            ["name", "title"]
                .into_iter()
                .filter_map(|name| block.get(name).and_then(Value::as_str))
                .find_map(document_format_from_name)
        })
}

fn claude_document_label(block: &Value) -> Option<&str> {
    ["name", "title"]
        .into_iter()
        .filter_map(|name| block.get(name).and_then(Value::as_str))
        .map(str::trim)
        .find(|name| !name.is_empty())
}

fn document_format_from_media_type(media_type: &str) -> Option<&'static str> {
    let media_type = media_type
        .split(';')
        .next()
        .unwrap_or(media_type)
        .trim()
        .to_ascii_lowercase();
    match media_type.as_str() {
        "application/pdf" => Some("pdf"),
        "text/csv" | "application/csv" => Some("csv"),
        "text/markdown" | "text/x-markdown" => Some("md"),
        "text/html" => Some("html"),
        "application/msword" => Some("doc"),
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => Some("docx"),
        "application/vnd.ms-excel" => Some("xls"),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => Some("xlsx"),
        media_type if media_type.starts_with("text/") => Some("txt"),
        _ => None,
    }
}

fn document_format_from_name(name: &str) -> Option<&'static str> {
    match name
        .rsplit_once('.')?
        .1
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "pdf" => Some("pdf"),
        "csv" => Some("csv"),
        "md" | "markdown" => Some("md"),
        "html" | "htm" => Some("html"),
        "txt" => Some("txt"),
        "doc" => Some("doc"),
        "docx" => Some("docx"),
        "xls" => Some("xls"),
        "xlsx" => Some("xlsx"),
        _ => None,
    }
}

fn collect_claude_images(content: &Value, images: &mut Vec<KiroImage>) {
    let Some(blocks) = content.as_array() else {
        return;
    };
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("image") => {
                if let Some(image) = claude_image(block) {
                    images.push(image);
                }
            }
            Some("tool_result") => {
                if let Some(content) = block.get("content") {
                    collect_claude_images(content, images);
                }
            }
            Some("document")
                if block.pointer("/source/type").and_then(Value::as_str) == Some("content") =>
            {
                if let Some(content) = block.pointer("/source/content") {
                    collect_claude_images(content, images);
                }
            }
            _ => {}
        }
    }
}

fn claude_image(block: &Value) -> Option<KiroImage> {
    let source = block.get("source")?;
    if source.get("type")?.as_str()? != "base64" {
        return None;
    }
    let media = source.get("media_type")?.as_str()?;
    let format = media.strip_prefix("image/").unwrap_or(media);
    let bytes = source.get("data")?.as_str()?.to_string();
    base64::engine::general_purpose::STANDARD
        .decode(&bytes)
        .ok()?;
    Some(KiroImage {
        format: format.into(),
        source: KiroImageSource { bytes },
    })
}

pub fn extract_openai_images(content: &Value) -> Vec<KiroImage> {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| {
            if block.get("type")?.as_str()? != "image_url" {
                return None;
            }
            let url = block.get("image_url")?.get("url")?.as_str()?;
            let encoded = url.strip_prefix("data:image/")?;
            let (format, bytes) = encoded.split_once(";base64,")?;
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(bytes)
                .ok()?;
            if decoded.len() > 5 * 1024 * 1024 {
                return None;
            }
            Some(KiroImage {
                format: normalize_image_format(format).into(),
                source: KiroImageSource {
                    bytes: bytes.into(),
                },
            })
        })
        .collect()
}

fn normalize_image_format(format: &str) -> &str {
    match format.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => "jpeg",
        "png" => "png",
        "gif" => "gif",
        "webp" => "webp",
        _ => "png",
    }
}

pub fn extract_tool_results(content: &Value) -> Vec<KiroToolResult> {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|block| {
            let tool_use_id = block.get("tool_use_id")?.as_str()?.to_string();
            let result = block.get("content").map(content_text).unwrap_or_default();
            let has_images = block
                .get("content")
                .is_some_and(content_contains_claude_image);
            let has_documents =
                block
                    .get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|blocks| {
                        blocks.iter().any(|block| {
                            block.get("type").and_then(Value::as_str) == Some("document")
                        })
                    });
            Some(KiroToolResult {
                content: vec![KiroText {
                    text: if result.is_empty() {
                        match (has_images, has_documents) {
                            (true, true) => "(image and document results attached)",
                            (true, false) => "(image result attached)",
                            (false, true) => "(document result attached)",
                            (false, false) => "(empty result)",
                        }
                        .into()
                    } else {
                        result
                    },
                }],
                status: if block.get("is_error").and_then(Value::as_bool) == Some(true) {
                    "error"
                } else {
                    "success"
                }
                .into(),
                tool_use_id,
            })
        })
        .collect()
}

fn content_contains_claude_image(content: &Value) -> bool {
    content.as_array().is_some_and(|blocks| {
        blocks
            .iter()
            .any(|block| match block.get("type").and_then(Value::as_str) {
                Some("image") => true,
                Some("tool_result") => block
                    .get("content")
                    .is_some_and(content_contains_claude_image),
                Some("document")
                    if block.pointer("/source/type").and_then(Value::as_str) == Some("content") =>
                {
                    block
                        .pointer("/source/content")
                        .is_some_and(content_contains_claude_image)
                }
                _ => false,
            })
    })
}

pub fn tool_name(name: &str) -> String {
    use std::fmt::Write as _;

    let normalized = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if normalized.len() <= MAX_KIRO_TOOL_NAME && !normalized.is_empty() {
        return normalized;
    }
    let digest = Sha256::digest(name.as_bytes());
    let suffix = digest[..6].iter().fold(String::new(), |mut output, byte| {
        let _ = write!(output, "{byte:02x}");
        output
    });
    let prefix_length = MAX_KIRO_TOOL_NAME - suffix.len() - 1;
    let mut prefix = normalized.chars().take(prefix_length).collect::<String>();
    if prefix.is_empty() {
        prefix.push_str("tool");
    }
    format!("{prefix}_{suffix}")
}

fn collision_tool_name(name: &str, nonce: u32) -> String {
    use std::fmt::Write as _;

    let normalized = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let digest = Sha256::digest(format!("{name}\0{nonce}").as_bytes());
    let suffix = digest[..8].iter().fold(String::new(), |mut output, byte| {
        let _ = write!(output, "{byte:02x}");
        output
    });
    let prefix_length = MAX_KIRO_TOOL_NAME - suffix.len() - 1;
    let mut prefix = normalized.chars().take(prefix_length).collect::<String>();
    if prefix.is_empty() {
        prefix.push_str("tool");
    }
    format!("{prefix}_{suffix}")
}

pub fn kiro_tool(name: &str, description: &str, schema: &Value) -> (KiroTool, Option<String>) {
    kiro_tool_named(name, &tool_name(name), description, schema)
}

pub fn kiro_tool_named(
    original_name: &str,
    kiro_name: &str,
    description: &str,
    schema: &Value,
) -> (KiroTool, Option<String>) {
    let (description, documentation) = if description.chars().count() > MAX_KIRO_DESCRIPTION {
        (
            format!("[Full documentation in system prompt under '## Tool: {original_name}']"),
            Some(format!("## Tool: {original_name}\n\n{description}")),
        )
    } else if description.trim().is_empty() {
        // Description is optional in Claude/OpenAI. Kiro accepts the tool
        // declaration but rejects a later structured call with an empty one.
        (format!("Tool {original_name}."), None)
    } else {
        (description.to_string(), None)
    };
    (
        KiroTool::Specification {
            tool_specification: KiroToolSpecification {
                name: kiro_name.to_owned(),
                description,
                input_schema: crate::KiroInputSchema {
                    // Kiro carries a JSON Schema document. Preserve semantic
                    // keywords such as `additionalProperties: false` and
                    // `$schema`; deleting them silently weakens the client's
                    // tool contract.
                    json: schema.clone(),
                },
            },
        },
        documentation,
    )
}

pub fn context(
    tools: Vec<KiroTool>,
    tool_results: Vec<KiroToolResult>,
) -> Option<KiroMessageContext> {
    (!tools.is_empty() || !tool_results.is_empty()).then_some(KiroMessageContext {
        tools,
        tool_results,
        env_state: None,
    })
}

pub fn inference(
    requested: Option<u32>,
    _has_tools: bool,
    temperature: Option<f64>,
    top_p: Option<f64>,
) -> Option<KiroInferenceConfig> {
    // Match buildKiroPayload in chaogei/Kiro-account-manager: omission leaves
    // the default to Kiro. Zero sampling values remain meaningful; maxTokens
    // is emitted only when positive (generation validation rejects zero).
    let max_tokens = requested.filter(|maximum| *maximum > 0);
    if max_tokens.is_none() && temperature.is_none() && top_p.is_none() {
        return None;
    }
    Some(KiroInferenceConfig {
        max_tokens,
        temperature,
        top_p,
    })
}

/// A legacy prompt hint for exact editor tool names, not a classification of
/// side effects. In particular, do not inspect or strip MCP namespaces here.
pub(super) fn needs_chunked_write_hint(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "write" | "edit" | "multiedit" | "notebookedit"
    )
}

pub fn enhance_system(mut system: String, chunked_write_hint: bool) -> String {
    let rich = system.to_ascii_lowercase().contains("claude code");
    if !rich {
        system.push_str(
            "\n\nExecute the user's request directly and use provided tools when needed.",
        );
    }
    if chunked_write_hint && !system.contains("Write/create/edit tool arguments must stay small") {
        system.push_str(
            "\n\nWrite/create/edit tool arguments must stay small; split large writes into chunks.",
        );
    }
    system.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunked_write_prompt_hint_does_not_classify_mcp_tools() {
        let options = crate::TranslationOptions::new("model", "AI_EDITOR");
        for (name, expected_hint) in [
            ("Write", true),
            ("EDIT", true),
            ("MultiEdit", true),
            ("NotebookEdit", true),
            ("write_file", false),
            ("mcp__files__Write", false),
            ("mcp__files__edit", false),
            ("mcp__relayer__memory_list_editable_atoms", false),
        ] {
            assert_eq!(needs_chunked_write_hint(name), expected_hint, "{name}");
            let claude: crate::ClaudeRequest = serde_json::from_value(serde_json::json!({
                "model":"model","max_tokens":256,
                "messages":[{"role":"user","content":"Use the provided tool."}],
                "tools":[{"name":name,"description":"Test tool","input_schema":{"type":"object"}}]
            }))
            .unwrap();
            let openai: crate::OpenAiRequest = serde_json::from_value(serde_json::json!({
                "model":"model",
                "messages":[{"role":"user","content":"Use the provided tool."}],
                "tools":[{"type":"function","function":{"name":name,"description":"Test tool","parameters":{"type":"object"}}}]
            }))
            .unwrap();
            for payload in [
                crate::claude_to_kiro(&claude, &options),
                crate::openai_to_kiro(&openai, &options),
            ] {
                assert_eq!(
                    serde_json::to_string(&payload)
                        .unwrap()
                        .contains("Write/create/edit tool arguments must stay small"),
                    expected_hint,
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn request_registry_resolves_normalization_collisions_reversibly() {
        let registry = ToolNameRegistry::new(["mcp.a/read", "mcp_a/read"]);
        let first = registry.kiro_name("mcp.a/read");
        let second = registry.kiro_name("mcp_a/read");
        assert_ne!(first, second);
        assert!(first.len() <= MAX_KIRO_TOOL_NAME);
        assert!(second.len() <= MAX_KIRO_TOOL_NAME);
        let reverse = registry.restore_map();
        assert_eq!(reverse.get(&first).map(String::as_str), Some("mcp.a/read"));
        assert_eq!(reverse.get(&second).map(String::as_str), Some("mcp_a/read"));
    }
}
