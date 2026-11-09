//! Capture physical client boundaries before translation merges text. All of
//! this metadata is local-only; token counts still come from the wire collector.

use super::common::{
    content_text, extract_documents, extract_images, extract_openai_images, kiro_cache_point,
};
use crate::{KiroAssistantMessage, KiroUserInputMessage, LocalSourceBlock, LocalSources};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

pub(super) fn blocks(content: &Value, control: Option<&Value>) -> Vec<Value> {
    let mut blocks = match content {
        Value::Array(blocks) => blocks.clone(),
        Value::String(text) => vec![json!({"type":"text", "text":text})],
        _ => Vec::new(),
    };
    if let (Some(last), Some(control)) = (blocks.last_mut(), control) {
        if kiro_cache_point(Some(control)).is_some()
            && kiro_cache_point(last.get("cache_control")).is_none()
        {
            last["cache_control"] = control.clone();
        }
    }
    blocks
}

fn identity(role: &str, block: &Value) -> Value {
    let mut block = block.clone();
    if let Some(object) = block.as_object_mut() {
        object.remove("cache_control");
    }
    // Fingerprint identities contain media digests instead of base64 payloads.
    fn digest_media(value: &mut Value) {
        use sha2::{Digest, Sha256};
        if let Some(source) = value.get_mut("source").and_then(Value::as_object_mut) {
            for key in ["data", "bytes"] {
                if let Some(data) = source.remove(key) {
                    source.insert(
                        format!("{key}_sha256"),
                        json!(format!(
                            "{:x}",
                            Sha256::digest(data.as_str().unwrap_or_default().as_bytes())
                        )),
                    );
                }
            }
        }
        for pointer in ["/image_url/url", "/file/file_data"] {
            if let Some(data) = value.pointer_mut(pointer) {
                if let Some(text) = data.as_str() {
                    *data = json!({"sha256":format!("{:x}", Sha256::digest(text.as_bytes()))});
                }
            }
        }
        if let Some(parts) = value.get_mut("content").and_then(Value::as_array_mut) {
            for part in parts {
                digest_media(part);
            }
        }
    }
    digest_media(&mut block);
    json!({"role":role,"block":block})
}

pub(super) fn user(
    content: &Value,
    control: Option<&Value>,
    actual: &KiroUserInputMessage,
    openai: bool,
    system: bool,
) -> Arc<LocalSources> {
    let mut sources = LocalSources::default();
    // Index once; a message can contain many tool results as well as text/media.
    // Keep duplicate IDs in wire order, matching the former per-block filter.
    let mut results_by_id = HashMap::<_, Vec<_>>::new();
    for result in actual
        .user_input_message_context
        .iter()
        .flat_map(|context| &context.tool_results)
    {
        results_by_id
            .entry(result.tool_use_id.as_str())
            .or_default()
            .push(result);
    }
    for (index, mut block) in blocks(content, control).into_iter().enumerate() {
        if system && index == 0 {
            if let Some(text) = block.get_mut("text") {
                if let Some(raw) = text
                    .as_str()
                    .filter(|text| text.starts_with("x-anthropic-billing-header:"))
                {
                    *text = json!(raw.split_once('\n').map_or("", |(_, rest)| rest));
                }
            }
            if block.get("text").and_then(Value::as_str) == Some("") {
                continue;
            }
        }
        let single = Value::Array(vec![block]);
        let block = &single[0];
        // The collector only needs this block's content, media and results.
        // Cloning `actual` here copies the entire merged message for every block.
        let part = KiroUserInputMessage {
            local_sources: None,
            content: content_text(&single),
            model_id: String::new(),
            origin: String::new(),
            images: if openai {
                extract_openai_images(&single)
            } else {
                extract_images(&single)
            },
            documents: extract_documents(&single),
            cache_point: None,
            client_cache_config: None,
            user_input_message_context: block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .and_then(|id| results_by_id.get(id))
                .map(|results| crate::KiroMessageContext {
                    tool_results: results.iter().map(|result| (**result).clone()).collect(),
                    ..Default::default()
                }),
        };
        let mut fragments = Vec::new();
        let mut fixed = 0;
        crate::tokenizer::collect_user_message(&part, &mut fragments, &mut fixed);
        let result_count = part
            .user_input_message_context
            .as_ref()
            .map_or(0, |c| c.tool_results.len());
        sources.blocks.push(LocalSourceBlock {
            identity: identity(if system { "system" } else { "user" }, block),
            cache_point: kiro_cache_point(block.get("cache_control")).map(|p| p.local),
            fragments,
            media_tokens: fixed.saturating_sub(8 + 10 * result_count) as u64,
            tool_turn: result_count > 0,
        });
    }
    sources.record_user(actual);
    Arc::new(sources)
}

pub(super) fn assistant(
    content: &Value,
    control: Option<&Value>,
    actual: &KiroAssistantMessage,
) -> Arc<LocalSources> {
    let mut sources = LocalSources::default();
    let mut uses_by_id = HashMap::<_, Vec<_>>::new();
    for tool in &actual.tool_uses {
        uses_by_id
            .entry(tool.tool_use_id.as_str())
            .or_default()
            .push(tool);
    }
    for block in blocks(content, control) {
        let single = Value::Array(vec![block]);
        let block = &single[0];
        let part = KiroAssistantMessage {
            local_sources: None,
            content: content_text(&single),
            cache_point: None,
            tool_uses: block
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| uses_by_id.get(id))
                .into_iter()
                .flatten()
                .map(|tool| (**tool).clone())
                .collect(),
        };
        let mut fragments = Vec::new();
        crate::tokenizer::collect_assistant_message(&part, &mut fragments, &mut 0);
        sources.blocks.push(LocalSourceBlock {
            identity: identity("assistant", block),
            cache_point: kiro_cache_point(block.get("cache_control")).map(|p| p.local),
            fragments,
            media_tokens: 0,
            tool_turn: !part.tool_uses.is_empty(),
        });
    }
    sources.translated_digest = LocalSources::assistant_digest(actual);
    Arc::new(sources)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_projection_keeps_only_its_media_and_matching_results() {
        let actual: KiroUserInputMessage = serde_json::from_value(json!({
            "content":"unrelated merged text".repeat(10_000), "modelId":"model", "origin":"CLI",
            "images":[{"format":"png","source":{"bytes":"unrelated".repeat(10_000)}}],
            "userInputMessageContext":{
                "envState":{"operatingSystem":"unrelated OS"},
                "tools":[{"toolSpecification":{"name":"unrelated","description":"large unrelated tool","inputSchema":{"json":{}}}}],
                "toolResults":[
                    {"toolUseId":"b","status":"success","content":[{"text":"other result"}]},
                    {"toolUseId":"a","status":"success","content":[{"text":"first"}]},
                    {"toolUseId":"a","status":"error","content":[{"text":"second"}]}
                ]
            }
        })).unwrap();
        let content = json!([
            {"type":"text","text":"hello"},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":"YWJj"}},
            {"type":"tool_result","tool_use_id":"a","content":"raw result","cache_control":{"type":"ephemeral","ttl":"1h"}}
        ]);
        let sources = user(&content, None, &actual, false, false);
        assert_eq!(sources.blocks.len(), 3);
        assert_eq!(sources.blocks[0].fragments, ["hello"]);
        assert_eq!(sources.blocks[0].media_tokens, 0);
        assert_eq!(sources.blocks[1].fragments, [""]);
        assert!(sources.blocks[1].media_tokens > 0);
        assert!(sources.blocks[1].identity["block"]["source"]
            .get("data")
            .is_none());
        assert!(sources.blocks[1].identity["block"]["source"]["data_sha256"].is_string());
        assert_eq!(
            sources.blocks[2].fragments,
            ["", "a", "success", "first", "a", "error", "second"]
        );
        assert_eq!(sources.blocks[2].media_tokens, 0);
        assert!(sources.blocks[2].tool_turn);
        assert!(sources.blocks[2].cache_point.unwrap().one_hour);
    }

    #[test]
    fn system_projection_strips_only_the_first_billing_line() {
        let actual: KiroUserInputMessage = serde_json::from_value(json!({
            "content":"rules", "modelId":"model", "origin":"CLI"
        }))
        .unwrap();
        let sources = user(
            &json!([
                {"type":"text","text":"x-anthropic-billing-header: volatile\nrules"},
                {"type":"text","text":"x-anthropic-billing-header: meaningful"}
            ]),
            Some(&json!({"type":"ephemeral"})),
            &actual,
            false,
            true,
        );
        assert_eq!(sources.blocks[0].fragments, ["rules"]);
        assert_eq!(
            sources.blocks[1].fragments,
            ["x-anthropic-billing-header: meaningful"]
        );
        assert!(sources.blocks[1].cache_point.is_some());
    }

    #[test]
    fn assistant_projection_preserves_duplicate_id_order() {
        let actual: KiroAssistantMessage = serde_json::from_value(json!({
            "content":"unrelated merged text", "toolUses":[
                {"toolUseId":"b","name":"Other","input":{}},
                {"toolUseId":"a","name":"First","input":{"x":1}},
                {"toolUseId":"a","name":"Second","input":{"x":2}}
            ]
        }))
        .unwrap();
        let sources = assistant(
            &json!([
                {"type":"text","text":"hello"},
                {"type":"tool_use","id":"a","name":"original","input":{}}
            ]),
            None,
            &actual,
        );
        assert_eq!(sources.blocks[0].fragments, ["hello"]);
        assert!(!sources.blocks[0].tool_turn);
        assert_eq!(
            sources.blocks[1].fragments,
            ["", "a", "First", "{\"x\":1}", "a", "Second", "{\"x\":2}"]
        );
        assert!(sources.blocks[1].tool_turn);
    }
}
