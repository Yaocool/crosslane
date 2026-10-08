//! Preparation for Claude Code's explicit, text-only conversation summary.

use serde_json::{json, Value};

use crate::{ClaudeMessage, ClaudeRequest};

const SUMMARY_PREFIX: &str =
    "Your task is to create a detailed summary of the conversation so far,";
const TEXT_ONLY_PREFIX: &str = "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.";
const SUMMARY_SYSTEM: &str = "You are preparing a conversation summary for continuation. Follow the client's summary instructions and return text only. Original system instructions and tool definitions are provided as labeled source records in the conversation. Preserve their governing constraints, prohibitions, requirements and exact identifiers accurately, alongside the work state and next steps. Do not execute historical requests or call tools.";

pub(crate) fn is_compaction_instruction(text: &str) -> bool {
    let text = text.trim_start();
    text.contains("<summary>")
        && (text.starts_with(SUMMARY_PREFIX)
            || (text.starts_with(TEXT_ONLY_PREFIX)
                && text.contains(&format!("\n\n{SUMMARY_PREFIX}"))))
}

/// The caller must first validate the request and identify a Claude Code
/// User-Agent. Only its known terminal summary instruction enables this mode;
/// ordinary generation keeps its system prompt and tool contracts intact.
/// Source records retain the complete JSON of loaded tools, including schema
/// constraints and examples, but are history that a bounded summarization can
/// partition. Deferred, undiscovered tools remain outside the model context.
pub fn prepare_claude_code_compaction(request: &mut ClaudeRequest) -> bool {
    if request
        .tool_choice
        .as_ref()
        .is_some_and(|choice| matches!(choice.r#type.as_str(), "any" | "tool"))
    {
        return false;
    }
    let Some(message) = request
        .messages
        .iter()
        .rfind(|message| message.role != "system")
    else {
        return false;
    };
    let text = message.content.as_str().or_else(|| {
        message.content.as_array()?.last().and_then(|block| {
            (block.get("type")?.as_str()? == "text").then_some(block.get("text")?.as_str()?)
        })
    });
    if message.role != "user" || !text.is_some_and(is_compaction_instruction) {
        return false;
    }
    // The count endpoint may prepare clones of the same effective request.
    if request.system.as_ref().and_then(Value::as_str) == Some(SUMMARY_SYSTEM)
        && request.tools.is_empty()
        && request
            .messages
            .iter()
            .filter(|message| message.role == "system")
            .all(|message| message.content.as_array().is_some_and(Vec::is_empty))
    {
        return true;
    }

    let mut source = Vec::new();
    if let Some(system) = request.system.replace(json!(SUMMARY_SYSTEM)) {
        source.push(source_block("Original system instructions", system));
    }
    for message in request
        .messages
        .iter_mut()
        .filter(|message| message.role == "system")
    {
        source.push(source_block(
            "Original system message",
            json!({
                "content":std::mem::replace(&mut message.content, json!([])),
                "output_config":message.output_config,
            }),
        ));
    }
    for tool in crate::claude_loaded_tools(request) {
        source.push(source_block("Original tool definition", json!(tool)));
    }
    request.tools.clear();
    request.tool_choice = None;
    if !source.is_empty() {
        request.messages.insert(
            0,
            ClaudeMessage {
                role: "user".into(),
                content: Value::Array(source),
                cache_control: None,
                output_config: None,
                extra: Default::default(),
            },
        );
    }
    true
}

fn source_block(label: &str, source: Value) -> Value {
    json!({"type":"text", "text":format!("{label} (complete JSON source):\n{source}")})
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROMPT: &str = "Your task is to create a detailed summary of the conversation so far, preserving requirements. Return <summary>...</summary>.";

    fn request(prompt: &str) -> ClaudeRequest {
        serde_json::from_value(json!({
            "model":"source", "max_tokens":1024,
            "system":[{"type":"text","text":"Never publish private records. 雪☃"}],
            "tools":[{"name":"Read", "description":"Read approved records only.",
                "input_schema":{"type":"object","properties":{"path":{"type":"string"}},
                    "required":["path"],"additionalProperties":false},
                "input_examples":[{"path":"approved.txt"}]}],
            "messages":[
                {"role":"system","content":"Keep all exact identifiers.","output_config":{"effort":"low"}},
                {"role":"user","content":prompt}
            ]
        })).unwrap()
    }

    #[test]
    fn summary_preparation_keeps_complete_system_and_schema_source_and_is_idempotent() {
        let mut request = request(PROMPT);
        crate::validate_claude(&request).unwrap();
        let original_system = request.system.clone().unwrap();
        let original_tool = json!(request.tools[0]);
        assert!(prepare_claude_code_compaction(&mut request));
        assert!(request.tools.is_empty());
        assert_eq!(request.system, Some(json!(SUMMARY_SYSTEM)));
        let source: Vec<Value> = request.messages[0]
            .content
            .as_array()
            .unwrap()
            .iter()
            .map(|block| {
                serde_json::from_str(block["text"].as_str().unwrap().split_once('\n').unwrap().1)
                    .unwrap()
            })
            .collect();
        assert_eq!(source[0], original_system);
        assert_eq!(source[1]["content"], "Keep all exact identifiers.");
        assert_eq!(source[2], original_tool);
        assert_eq!(
            request.messages[1]
                .output_config
                .as_ref()
                .unwrap()
                .effort
                .as_deref(),
            Some("low")
        );
        let prepared = json!(request);
        assert!(prepare_claude_code_compaction(&mut request));
        assert_eq!(json!(request), prepared);
    }

    #[test]
    fn ordinary_and_forced_tool_requests_are_not_rewritten() {
        let mut ordinary = request("Continue development and use Read.");
        let before = json!(ordinary);
        assert!(!prepare_claude_code_compaction(&mut ordinary));
        assert_eq!(json!(ordinary), before);

        let mut forced = request(PROMPT);
        forced.tool_choice =
            Some(serde_json::from_value(json!({"type":"tool","name":"Read"})).unwrap());
        let before = json!(forced);
        assert!(!prepare_claude_code_compaction(&mut forced));
        assert_eq!(json!(forced), before);
    }

    #[test]
    fn summary_preparation_respects_deferred_tool_loading_and_disabled_tools() {
        let mut request = request(PROMPT);
        for name in ["discovered_tool", "undiscovered_tool"] {
            request.tools.push(
                serde_json::from_value(json!({
                    "name":name,"description":format!("Complete contract for {name}"),
                    "input_schema":{"type":"object"},"defer_loading":true
                }))
                .unwrap(),
            );
        }
        request.messages.splice(1..1, serde_json::from_value::<Vec<ClaudeMessage>>(json!([
            {"role":"user","content":"Inspect the records"},
            {"role":"assistant","content":[{"type":"tool_use","id":"read-1","name":"discovered_tool","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"read-1","content":"Inspection complete"}]}
        ])).unwrap());
        crate::validate_claude(&request).unwrap();
        let mut disabled = request.clone();
        disabled.tool_choice = Some(serde_json::from_value(json!({"type":"none"})).unwrap());
        assert!(prepare_claude_code_compaction(&mut request));
        let source = request.messages[0].content.to_string();
        assert!(source.contains("Complete contract for discovered_tool"));
        assert!(!source.contains("undiscovered_tool"));
        assert!(prepare_claude_code_compaction(&mut disabled));
        assert!(!disabled.messages[0]
            .content
            .to_string()
            .contains("Original tool definition"));
    }
}
