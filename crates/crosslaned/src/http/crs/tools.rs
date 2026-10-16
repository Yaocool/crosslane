//! Project Claude Code's late tool declarations onto a legacy tools[] catalog.
//! The client owns tool execution; tool_use and tool_result blocks stay intact.

use std::collections::HashMap;

use crosslane_translate::ValidationError;

use super::*;

fn invalid(field: String, message: &str) -> ValidationError {
    ValidationError::InvalidField {
        field,
        message: message.into(),
    }
}

fn name(value: &Value, field: &str) -> Result<String, ValidationError> {
    value
        .as_str()
        .filter(|name| !name.is_empty() && name.len() <= 128 && !name.chars().any(char::is_control))
        .map(str::to_owned)
        .ok_or_else(|| invalid(field.into(), "expected a bounded tool name"))
}

pub(crate) fn normalize_tool_changes(body: &Bytes) -> Result<Bytes, ValidationError> {
    if ![
        b"\"tool_addition\"".as_slice(),
        b"\"tool_removal\"".as_slice(),
    ]
    .iter()
    .any(|needle| body.windows(needle.len()).any(|bytes| bytes == *needle))
    {
        return Ok(body.clone());
    }
    let Ok(mut value) = serde_json::from_slice::<Value>(body) else {
        return Ok(body.clone());
    };
    let Some(messages) = value.get("messages").and_then(Value::as_array) else {
        return Ok(body.clone());
    };
    if !messages
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .any(|block| {
            matches!(
                block["type"].as_str(),
                Some("tool_addition" | "tool_removal")
            )
        })
    {
        return Ok(body.clone());
    }
    let mut tools = match value.get("tools") {
        None => Vec::new(),
        Some(Value::Array(tools)) => tools.clone(),
        Some(_) => return Err(invalid("tools".into(), "expected a tool array")),
    };
    let mut catalog = HashMap::new();
    for (index, tool) in tools.iter().enumerate() {
        let name = name(&tool["name"], &format!("tools.{index}.name"))?;
        if catalog.insert(name, index).is_some() {
            return Err(invalid(
                format!("tools.{index}.name"),
                "tool names must be unique",
            ));
        }
    }
    let mut removed = HashMap::new();
    let tools_disabled = value.pointer("/tool_choice/type").and_then(Value::as_str) == Some("none");
    let messages = value["messages"].as_array_mut().unwrap();
    for (message_index, message) in messages.iter_mut().enumerate() {
        let role = message["role"].as_str().unwrap_or_default().to_owned();
        let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for (block_index, block) in blocks.iter_mut().enumerate() {
            let addition = match block["type"].as_str() {
                Some("tool_addition") => true,
                Some("tool_removal") => false,
                _ => {
                    reject_removed_references(
                        block,
                        &format!("messages.{message_index}.content.{block_index}"),
                        &removed,
                    )?;
                    continue;
                }
            };
            let field = format!("messages.{message_index}.content.{block_index}");
            if !matches!(role.as_str(), "user" | "system") {
                return Err(invalid(field, "tool changes require a user or system role"));
            }
            let tool = &block["tool"];
            let tool_name = match tool["type"].as_str() {
                Some("tool_reference") => name(&tool["name"], &format!("{field}.tool.name"))?,
                Some("tool_definition") if addition => {
                    let definition = tool["definition"].as_object().ok_or_else(|| {
                        invalid(
                            format!("{field}.tool.definition"),
                            "expected a custom tool definition",
                        )
                    })?;
                    if definition
                        .get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| kind != "custom")
                        || !definition.get("input_schema").is_some_and(Value::is_object)
                    {
                        return Err(invalid(
                            format!("{field}.tool.definition"),
                            "inline additions must define custom tools with an input_schema",
                        ));
                    }
                    let tool_name = name(
                        &tool["definition"]["name"],
                        &format!("{field}.tool.definition.name"),
                    )?;
                    let mut definition = tool["definition"].clone();
                    definition.as_object_mut().unwrap().remove("defer_loading");
                    if let Some(index) = catalog.get(&tool_name).copied() {
                        let mut existing = tools[index].clone();
                        existing.as_object_mut().unwrap().remove("defer_loading");
                        if existing != definition && !removed.contains_key(&tool_name) {
                            return Err(invalid(format!("{field}.tool.definition.name"), "conflicting tool definition; remove the previous definition before replacing it"));
                        }
                        tools[index] = definition;
                    } else {
                        if tools.len() >= 10_000 {
                            return Err(invalid("tools".into(), "too many late tool definitions"));
                        }
                        catalog.insert(tool_name.clone(), tools.len());
                        tools.push(definition);
                    }
                    tool_name
                }
                _ => {
                    return Err(invalid(
                        format!("{field}.tool.type"),
                        "expected a tool reference or an added custom definition",
                    ))
                }
            };
            let Some(index) = catalog.get(&tool_name).copied() else {
                return Err(invalid(
                    format!("{field}.tool.name"),
                    "referenced tool is not declared",
                ));
            };
            if addition {
                removed.remove(&tool_name);
                tools[index]
                    .as_object_mut()
                    .unwrap()
                    .remove("defer_loading");
            } else {
                removed.insert(tool_name.clone(), (message_index, block_index));
            }
            let mut replacement = serde_json::json!({"type":"text","text":format!("[Tool catalog update: {tool_name} {}.]", if addition {"available"} else {"removed"})});
            if let Some(cache) = block.get("cache_control") {
                replacement["cache_control"] = cache.clone();
            }
            *block = replacement;
        }
    }
    archive_removed_references(messages, &removed);
    if tools_disabled {
        preserve_removed_definition_sources(messages, &tools, &removed);
    }
    tools.retain(|tool| {
        !tool["name"]
            .as_str()
            .is_some_and(|name| removed.contains_key(name))
    });
    value["tools"] = Value::Array(tools);
    serde_json::to_vec(&value)
        .map(Bytes::from)
        .map_err(|_| invalid("messages".into(), "failed to encode tool catalog update"))
}

fn preserve_removed_definition_sources(
    messages: &mut [Value],
    tools: &[Value],
    removed: &HashMap<String, (usize, usize)>,
) {
    let used = messages
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .filter(|block| matches!(block["type"].as_str(), Some("tool_use" | "server_tool_use")))
        .filter_map(|block| block["name"].as_str())
        .collect::<std::collections::HashSet<_>>();
    let definitions = tools
        .iter()
        .filter(|tool| {
            tool["name"]
                .as_str()
                .is_some_and(|name| removed.contains_key(name) && used.contains(name))
        })
        .collect::<Vec<_>>();
    if definitions.is_empty() {
        return;
    }
    let source = serde_json::json!({"type":"text","text":format!(
        "Client-provided tool definitions for interpreting historical records (unavailable for new calls this turn; JSON source data):\n{}",
        serde_json::to_string(&definitions).expect("tool definitions serialize")
    )});
    if let Some(user) = messages
        .iter_mut()
        .find(|message| message["role"] == "user")
    {
        if let Some(blocks) = user["content"].as_array_mut() {
            blocks.push(source);
        } else if let Some(text) = user["content"].as_str().map(str::to_owned) {
            user["content"] = serde_json::json!([{"type":"text","text":text},source]);
        }
    }
}

fn reject_removed_references(
    block: &Value,
    field: &str,
    removed: &HashMap<String, (usize, usize)>,
) -> Result<(), ValidationError> {
    let references = match block["type"].as_str() {
        Some("tool_result") => block.get("content").and_then(Value::as_array),
        Some("tool_search_tool_result") => block
            .pointer("/content/tool_references")
            .and_then(Value::as_array),
        _ => None,
    };
    for (index, reference) in references.into_iter().flatten().enumerate() {
        if reference["type"] == "tool_reference"
            && reference["tool_name"]
                .as_str()
                .is_some_and(|name| removed.contains_key(name))
        {
            return Err(invalid(
                format!("{field}.tool_reference.{index}.tool_name"),
                "referenced tool was removed before this reference",
            ));
        }
    }
    Ok(())
}

/// A historical discovery is evidence, not permission to invoke a tool now.
/// Archive only references preceding an explicit removal. Unknown references
/// and references after removal still reach the ordinary validation failure.
fn archive_removed_references(messages: &mut [Value], removed: &HashMap<String, (usize, usize)>) {
    if removed.is_empty() {
        return;
    }
    for (message_index, message) in messages.iter_mut().enumerate() {
        let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        let mut projected = Vec::with_capacity(blocks.len());
        for (block_index, mut block) in std::mem::take(blocks).into_iter().enumerate() {
            let historical = |reference: &Value| {
                reference["type"] == "tool_reference"
                    && reference["tool_name"]
                        .as_str()
                        .and_then(|name| removed.get(name))
                        .is_some_and(|removal| (message_index, block_index) < *removal)
            };
            let mut notices = Vec::new();
            match block["type"].as_str() {
                Some("tool_result") => {
                    if let Some(parts) = block.get_mut("content").and_then(Value::as_array_mut) {
                        for reference in parts {
                            if historical(reference) {
                                *reference = archived_reference(reference);
                            }
                        }
                    }
                }
                Some("tool_search_tool_result") => {
                    if let Some(references) = block
                        .pointer_mut("/content/tool_references")
                        .and_then(Value::as_array_mut)
                    {
                        references.retain(|reference| {
                            if historical(reference) {
                                notices.push(archived_reference(reference));
                                false
                            } else {
                                true
                            }
                        });
                    }
                }
                _ => {}
            }
            projected.push(block);
            projected.extend(notices);
        }
        *blocks = projected;
    }
}

fn archived_reference(reference: &Value) -> Value {
    let mut notice = serde_json::json!({
        "type":"text",
        "text":format!("[Historical tool reference; currently unavailable: {reference}]")
    });
    if let Some(cache) = reference.get("cache_control") {
        notice["cache_control"] = cache.clone();
    }
    notice
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn discovery_before_removal(official: bool) -> Value {
        let (search, history) = if official {
            (
                json!({"type":"tool_search_tool_regex_20251119","name":"tool_search_tool_regex"}),
                json!([
                    {"role":"user","content":"discover tools"},
                    {"role":"assistant","content":[
                        {"type":"server_tool_use","id":"search1","name":"tool_search_tool_regex","input":{"pattern":"tool"}},
                        {"type":"tool_search_tool_result","tool_use_id":"search1","content":{"type":"tool_search_tool_search_result","tool_references":[
                            {"type":"tool_reference","tool_name":"old_tool"},
                            {"type":"tool_reference","tool_name":"active_tool"}
                        ]}}
                    ]}
                ]),
            )
        } else {
            (
                json!({"name":"custom_search","input_schema":{"type":"object"}}),
                json!([
                    {"role":"user","content":"discover tools"},
                    {"role":"assistant","content":[{"type":"tool_use","id":"search1","name":"custom_search","input":{}}]},
                    {"role":"user","content":[{"type":"tool_result","tool_use_id":"search1","content":[
                        {"type":"tool_reference","tool_name":"old_tool","cache_control":{"type":"ephemeral"}},
                        {"type":"tool_reference","tool_name":"active_tool"},
                        {"type":"text","text":"original search details"}
                    ]}]}
                ]),
            )
        };
        let mut value = json!({"model":"claude-sonnet-4-6","max_tokens":100,"tools":[
            search,
            {"name":"old_tool","input_schema":{"type":"object"},"defer_loading":true},
            {"name":"active_tool","input_schema":{"type":"object"},"defer_loading":true}
        ],"messages":history});
        value["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role":"user","content":[
                {"type":"tool_removal","tool":{"type":"tool_reference","name":"old_tool"}}
            ]}));
        value
    }

    #[test]
    fn removed_tools_keep_historical_discovery_without_becoming_callable() {
        for official in [false, true] {
            let input = discovery_before_removal(official);
            let normalized = normalize_tool_changes(&Bytes::from(input.to_string())).unwrap();
            let output: Value = serde_json::from_slice(&normalized).unwrap();
            let typed = serde_json::from_slice(&normalized).unwrap();
            crosslane_translate::validate_claude(&typed).unwrap();
            assert!(!output["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "old_tool"));
            assert_eq!(
                output["messages"][1]["content"][0],
                input["messages"][1]["content"][0]
            );
            assert!(normalized
                .windows(b"Historical tool reference".len())
                .any(|s| s == b"Historical tool reference"));
            if official {
                let result = &output["messages"][1]["content"][1];
                assert_eq!(result["tool_use_id"], "search1");
                assert_eq!(
                    result["content"]["tool_references"],
                    json!([{"type":"tool_reference","tool_name":"active_tool"}])
                );
                assert!(output["messages"][1]["content"][2]["text"]
                    .as_str()
                    .unwrap()
                    .contains("old_tool"));
            } else {
                let result = &output["messages"][2]["content"][0];
                assert_eq!(result["tool_use_id"], "search1");
                assert_eq!(
                    result["content"][0]["cache_control"],
                    input["messages"][2]["content"][0]["content"][0]["cache_control"]
                );
                assert_eq!(
                    result["content"][1],
                    input["messages"][2]["content"][0]["content"][1]
                );
                assert_eq!(
                    result["content"][2],
                    input["messages"][2]["content"][0]["content"][2]
                );
            }
        }
    }

    #[test]
    fn none_keeps_removed_called_tool_definitions_as_source_data() {
        let input = json!({"model":"source","max_tokens":128,"tool_choice":{"type":"none"},
        "tools":[{"name":"old_tool","description":"Old lookup description","input_schema":{"type":"object","properties":{"query":{"type":"string"}}}}],
        "messages":[
            {"role":"user","content":"Look up the previous value."},
            {"role":"assistant","content":[{"type":"tool_use","id":"old1","name":"old_tool","input":{"query":"key"}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"old1","content":"旧结果 中文😀"}]},
            {"role":"user","content":[{"type":"tool_removal","tool":{"type":"tool_reference","name":"old_tool"}},{"type":"text","text":"Answer without tools."}]}
        ]});
        let normalized = normalize_tool_changes(&Bytes::from(input.to_string())).unwrap();
        let value: Value = serde_json::from_slice(&normalized).unwrap();
        assert!(value["tools"].as_array().unwrap().is_empty());
        assert!(value["messages"][0]["content"][1]["text"]
            .as_str()
            .unwrap()
            .contains("Old lookup description"));
        let request = serde_json::from_slice(&normalized).unwrap();
        crosslane_translate::validate_claude(&request).unwrap();
        let mut payload = crosslane_translate::claude_to_kiro(
            &request,
            &crosslane_translate::TranslationOptions::new("target", "AI_EDITOR"),
        );
        crosslane_translate::sanitize_kiro_tool_history(&mut payload);
        crosslane_translate::validate_kiro_tool_history(&payload).unwrap();
        let current = &payload
            .conversation_state
            .current_message
            .user_input_message;
        assert!(current
            .user_input_message_context
            .as_ref()
            .unwrap()
            .tools
            .is_empty());
        assert_eq!(
            current
                .user_input_message_context
                .as_ref()
                .unwrap()
                .tool_results[0]
                .tool_use_id,
            "old1"
        );
        assert!(serde_json::to_string(&payload)
            .unwrap()
            .contains("Old lookup description"));
    }

    #[test]
    fn removal_does_not_authorize_unknown_or_later_references() {
        for lifecycle in ["unknown", "removed", "readded", "removed_again"] {
            let mut input = discovery_before_removal(false);
            if lifecycle != "unknown" {
                input["messages"].as_array_mut().unwrap().extend([
                    json!({"role":"assistant","content":[{"type":"tool_use","id":"search2","name":"custom_search","input":{}}]}),
                    json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"search2","content":[{"type":"tool_reference","tool_name":"old_tool"}]}]})
                ]);
                if lifecycle != "removed" {
                    input["messages"].as_array_mut().unwrap().push(json!({"role":"user","content":[
                        {"type":"tool_addition","tool":{"type":"tool_reference","name":"old_tool"}}
                    ]}));
                }
                if lifecycle == "removed_again" {
                    input["messages"].as_array_mut().unwrap().push(json!({"role":"user","content":[
                        {"type":"tool_removal","tool":{"type":"tool_reference","name":"old_tool"}}
                    ]}));
                }
                let error = normalize_tool_changes(&Bytes::from(input.to_string())).unwrap_err();
                assert!(error.to_string().contains("removed before this reference"));
            } else {
                input["messages"][2]["content"][0]["content"][1]["tool_name"] =
                    json!("unknown_tool");
                let normalized = normalize_tool_changes(&Bytes::from(input.to_string())).unwrap();
                let typed = serde_json::from_slice(&normalized).unwrap();
                let error = crosslane_translate::validate_claude(&typed).unwrap_err();
                assert!(error
                    .to_string()
                    .contains("not defined in the top-level tools array"));
            }
        }
        let mut input = discovery_before_removal(false);
        input["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role":"user","content":[
                {"type":"tool_addition","tool":{"type":"tool_reference","name":"old_tool"}}
            ]}));
        let normalized = normalize_tool_changes(&Bytes::from(input.to_string())).unwrap();
        let output: Value = serde_json::from_slice(&normalized).unwrap();
        assert_eq!(output["messages"][2], input["messages"][2]);
        assert!(output["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "old_tool"));
        crosslane_translate::validate_claude(&serde_json::from_slice(&normalized).unwrap())
            .unwrap();
        input["messages"].as_array_mut().unwrap().extend([
            json!({"role":"assistant","content":[{"type":"tool_use","id":"search2","name":"custom_search","input":{}}]}),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"search2","content":[{"type":"tool_reference","tool_name":"old_tool"}]}]})
        ]);
        let normalized = normalize_tool_changes(&Bytes::from(input.to_string())).unwrap();
        crosslane_translate::validate_claude(&serde_json::from_slice(&normalized).unwrap())
            .unwrap();
    }

    #[test]
    fn late_tools_become_available_without_changing_tool_execution_history() {
        let value = json!({"model":"claude","max_tokens":32,"tools":[{"name":"Read","input_schema":{"type":"object"}},{"name":"late","input_schema":{"type":"object"},"defer_loading":true}],"messages":[
            {"role":"user","content":"read"},
            {"role":"assistant","content":[{"type":"tool_use","id":"once","name":"Read","input":{"file_path":"/tmp/中文.txt"}},{"type":"thinking","thinking":"kept","signature":"opaque"}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"once","content":"中文😀"}]},
            {"role":"system","content":[{"type":"text","text":"policy stays"},{"type":"tool_addition","tool":{"type":"tool_reference","name":"late"},"cache_control":{"type":"ephemeral"}},{"type":"tool_addition","tool":{"type":"tool_definition","definition":{"name":"extra","input_schema":{"type":"object"}}}}]}
        ]});
        let out = normalize_tool_changes(&Bytes::from(value.to_string())).unwrap();
        let out: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(out["messages"][1], value["messages"][1]);
        assert_eq!(out["messages"][2], value["messages"][2]);
        assert_eq!(
            out["messages"][3]["content"][0],
            value["messages"][3]["content"][0]
        );
        assert_eq!(
            out["messages"][3]["content"][1]["cache_control"],
            value["messages"][3]["content"][1]["cache_control"]
        );
        assert!(out["tools"][1].get("defer_loading").is_none());
        assert_eq!(out["tools"][2]["name"], "extra");
        let typed = serde_json::from_value(out).unwrap();
        crosslane_translate::validate_claude(&typed).unwrap();
    }

    #[test]
    fn removal_and_readdition_follow_message_order_and_conflicts_fail_closed() {
        let mut value = json!({"tools":[{"name":"late","input_schema":{"type":"object"}}],"messages":[{"role":"system","content":[{"type":"tool_removal","tool":{"type":"tool_reference","name":"late"}}]}]});
        let out: Value = serde_json::from_slice(
            &normalize_tool_changes(&Bytes::from(value.to_string())).unwrap(),
        )
        .unwrap();
        assert!(out["tools"].as_array().unwrap().is_empty());
        value["messages"][0]["content"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"tool_addition","tool":{"type":"tool_reference","name":"late"}}));
        let out: Value = serde_json::from_slice(
            &normalize_tool_changes(&Bytes::from(value.to_string())).unwrap(),
        )
        .unwrap();
        assert_eq!(out["tools"][0]["name"], "late");
        value["messages"][0]["content"].as_array_mut().unwrap().push(json!({"type":"tool_addition","tool":{"type":"tool_definition","definition":{"name":"late","input_schema":{"type":"object","required":["new"]}}}}));
        assert!(normalize_tool_changes(&Bytes::from(value.to_string())).is_err());
    }

    #[test]
    fn quoted_control_names_do_not_mutate_requests_and_unknown_references_are_rejected() {
        let body = Bytes::from_static(
            br#"{"messages":[{"role":"user","content":"describe \"tool_addition\""}]}"#,
        );
        assert_eq!(normalize_tool_changes(&body).unwrap(), body);
        let body = Bytes::from_static(br#"{"messages":[{"role":"system","content":[{"type":"tool_addition","tool":{"type":"tool_reference","name":"missing"}}]}]}"#);
        assert!(normalize_tool_changes(&body).is_err());
        let body = Bytes::from_static(br#"{"tools":[{"name":"x","input_schema":{"type":"object"}}],"messages":["invalid-message",{"role":"system","content":[{"type":"tool_addition","tool":{"type":"tool_reference","name":"x"}}]}]}"#);
        // Leave malformed ordinary messages to normal request validation.
        assert!(normalize_tool_changes(&body).is_ok());
    }
}
