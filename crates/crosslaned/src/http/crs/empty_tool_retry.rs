//! Clarify the execution boundary on the existing empty-tool-response retry.

use crosslane_translate::{KiroPayload, KiroTool};

const MAX_DESCRIPTION_BYTES: usize = 1024;
const EXECUTION_BOUNDARY_HINT: &str = " This is a client-executed custom function. Return a structured tool call with the requested arguments; the client will perform the operation.";

/// Kiro emits calls; the downstream client or proxy executes the current tools.
/// Keep their original identities and source data intact, and only append this
/// clarification when it fits the conservative UTF-8 byte budget. Callers own
/// the existing retry limit and the Claude/Kiro protocol boundary.
pub(crate) fn clarify_tool_execution_boundary(payload: &mut KiroPayload) -> usize {
    if payload
        .model_request_intent
        .as_ref()
        .is_some_and(|intent| intent.crs_tool_choice_none)
    {
        return 0;
    }
    let Some(context) = payload
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context
        .as_mut()
    else {
        return 0;
    };
    let mut clarified = 0;
    for tool in context
        .tools
        .iter_mut()
        .filter_map(KiroTool::specification_mut)
    {
        if tool.description.ends_with(EXECUTION_BOUNDARY_HINT)
            || tool
                .description
                .len()
                .saturating_add(EXECUTION_BOUNDARY_HINT.len())
                > MAX_DESCRIPTION_BYTES
        {
            continue;
        }
        tool.description.push_str(EXECUTION_BOUNDARY_HINT);
        clarified += 1;
    }
    clarified
}

#[cfg(test)]
mod tests {
    use crosslane_translate::{KiroCachePoint, ModelRequestIntent};
    use serde_json::{json, Value};

    use super::*;

    fn payload() -> KiroPayload {
        let mut payload: KiroPayload = serde_json::from_value(json!({
            "conversationState": {
                "agentContinuationId": "continuation-original",
                "agentTaskType": "vibe",
                "chatTriggerType": "MANUAL",
                "conversationId": "conversation-original",
                "history": [
                    {"userInputMessage": {
                        "content": "Historical source checkpoint 中文😀",
                        "modelId": "historical-model", "origin": "AI_EDITOR",
                        "cachePoint": {"type": "default"},
                        "userInputMessageContext": {"tools": [{
                            "toolSpecification": {
                                "name": "historical_reader", "description": "Historical definition",
                                "inputSchema": {"json": {"type": "object"}}
                            }
                        }]}
                    }},
                    {"assistantResponseMessage": {
                        "content": "", "cachePoint": {"type": "default"},
                        "toolUses": [{"toolUseId": "past-call", "name": "checkpoint_reader",
                            "input": {"path": "/owned/历史😀.txt"}}]
                    }}
                ],
                "currentMessage": {"userInputMessage": {
                    "content": "Use checkpoint_reader to read /owned/中文😀.txt.",
                    "modelId": "target-model", "origin": "AI_EDITOR",
                    "cachePoint": {"type": "default"},
                    "clientCacheConfig": {"opaque": "retained"},
                    "userInputMessageContext": {
                        "tools": [
                            {"toolSpecification": {
                                "name": "checkpoint_reader", "description": "读取原始文件😀",
                                "inputSchema": {"json": {
                                    "type": "object", "additionalProperties": false,
                                    "properties": {"path": {"type": "string"}}, "required": ["path"]
                                }}
                            }},
                            {"cachePoint": {"type": "default"}},
                            {"toolSpecification": {
                                "name": "web_search", "description": "Search the web",
                                "inputSchema": {"json": {"type": "object"}}
                            }}
                        ],
                        "toolResults": [{"toolUseId": "past-call", "status": "success",
                            "content": [{"text": "Original result 中文😀"}]}],
                        "envState": {"currentWorkingDirectory": "/owned", "operatingSystem": "macOS"}
                    }
                }}
            },
            "profileArn": "original-profile", "inferenceConfig": {"maxTokens": 1024},
            "additionalModelRequestFields": {"thinkingConfig": {"type": "enabled"}}
        }))
        .unwrap();
        payload.model_request_intent = Some(ModelRequestIntent {
            requested_model: "client-model".into(),
            effort: Some("high".into()),
            automatic_history_truncation: true,
            ..Default::default()
        });
        payload.protected_history_messages = 1;
        payload
    }

    fn tools(payload: &mut KiroPayload) -> &mut Vec<KiroTool> {
        &mut payload
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .as_mut()
            .unwrap()
            .tools
    }

    #[test]
    fn changes_only_current_tool_descriptions_and_preserves_unicode_source() {
        let mut payload = payload();
        let mut expected = serde_json::to_value(&payload).unwrap();
        for index in [0, 2] {
            let description = &mut expected["conversationState"]["currentMessage"]
                ["userInputMessage"]["userInputMessageContext"]["tools"][index]
                ["toolSpecification"]["description"];
            *description = Value::String(format!(
                "{}{EXECUTION_BOUNDARY_HINT}",
                description.as_str().unwrap()
            ));
        }

        assert_eq!(clarify_tool_execution_boundary(&mut payload), 2);
        assert_eq!(serde_json::to_value(&payload).unwrap(), expected);
        assert_eq!(payload.protected_history_messages, 1);
        let intent = payload.model_request_intent.as_ref().unwrap();
        assert_eq!(intent.requested_model, "client-model");
        assert_eq!(intent.effort.as_deref(), Some("high"));
        assert!(intent.automatic_history_truncation);
        assert!(!intent.crs_tool_choice_none);
    }

    #[test]
    fn repeated_clarification_is_a_noop() {
        let mut payload = payload();
        assert_eq!(clarify_tool_execution_boundary(&mut payload), 2);
        let clarified = serde_json::to_value(&payload).unwrap();
        assert_eq!(clarify_tool_execution_boundary(&mut payload), 0);
        assert_eq!(serde_json::to_value(&payload).unwrap(), clarified);
    }

    #[test]
    fn description_budget_keeps_overlong_originals_without_truncation() {
        let mut payload = payload();
        let base = tools(&mut payload)[0].clone();
        let fitting = "x".repeat(MAX_DESCRIPTION_BYTES - EXECUTION_BOUNDARY_HINT.len());
        let too_long = format!("{fitting}x");
        let unicode_too_long = "读".repeat(MAX_DESCRIPTION_BYTES / 3);
        let originals = [fitting, too_long, unicode_too_long];
        *tools(&mut payload) = originals
            .iter()
            .map(|description| {
                let mut tool = base.clone();
                tool.specification_mut().unwrap().description = description.clone();
                tool
            })
            .collect();

        assert_eq!(clarify_tool_execution_boundary(&mut payload), 1);
        let descriptions = tools(&mut payload)
            .iter()
            .map(|tool| tool.specification().unwrap().description.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            descriptions[0],
            format!("{}{EXECUTION_BOUNDARY_HINT}", originals[0])
        );
        assert_eq!(descriptions[0].len(), MAX_DESCRIPTION_BYTES);
        assert_eq!(descriptions[1], originals[1]);
        assert_eq!(descriptions[2], originals[2]);
        assert_eq!(clarify_tool_execution_boundary(&mut payload), 0);
    }

    #[test]
    fn none_intent_keeps_the_payload_unchanged() {
        let mut payload = payload();
        payload
            .model_request_intent
            .as_mut()
            .unwrap()
            .crs_tool_choice_none = true;
        let original = serde_json::to_value(&payload).unwrap();
        assert_eq!(clarify_tool_execution_boundary(&mut payload), 0);
        assert_eq!(serde_json::to_value(&payload).unwrap(), original);
        assert!(payload.model_request_intent.unwrap().crs_tool_choice_none);
    }

    #[test]
    fn missing_empty_or_cache_only_tool_context_is_a_noop() {
        for tool_set in [
            Vec::new(),
            vec![KiroTool::CachePoint {
                cache_point: KiroCachePoint::new(),
            }],
        ] {
            let mut payload = payload();
            *tools(&mut payload) = tool_set;
            let original = serde_json::to_value(&payload).unwrap();
            assert_eq!(clarify_tool_execution_boundary(&mut payload), 0);
            assert_eq!(serde_json::to_value(&payload).unwrap(), original);
        }
        let mut payload = payload();
        payload
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context = None;
        let original = serde_json::to_value(&payload).unwrap();
        assert_eq!(clarify_tool_execution_boundary(&mut payload), 0);
        assert_eq!(serde_json::to_value(&payload).unwrap(), original);
    }
}
