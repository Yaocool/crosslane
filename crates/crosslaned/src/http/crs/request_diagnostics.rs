//! Content-free correlation for an independently reconstructed Kiro request.

use crosslane_translate::{error::log_model, KiroPayload};
use sha2::{Digest, Sha256};

/// This is a logical-payload fingerprint, not a transport-wire fingerprint.
/// Transport routing may add endpoint-specific fields. Exclude credentials'
/// profile identifier and random conversation identifiers; retain all model,
/// generation, history and tool fields so a changed contract cannot match.
fn logical_fingerprint(payload: &KiroPayload) -> Option<String> {
    let mut value = serde_json::to_value(payload).ok()?;
    value.as_object_mut()?.remove("profileArn");
    let conversation = value.get_mut("conversationState")?.as_object_mut()?;
    conversation.remove("conversationId");
    conversation.remove("agentContinuationId");
    let bytes = serde_json::to_vec(&value).ok()?;
    Some(format!("{:x}", Sha256::digest(bytes)))
}

pub(in crate::http) fn log_generation_request(
    payload: &KiroPayload,
    trace_id: &str,
    attempt: u32,
    phase: &'static str,
) {
    let Some(fingerprint) = logical_fingerprint(payload) else {
        return;
    };
    let state = &payload.conversation_state;
    let current = &state.current_message.user_input_message;
    let mut history_text_bytes = 0usize;
    let mut largest_history_text_bytes = 0usize;
    let mut history_tool_calls = 0usize;
    let mut history_tool_results = 0usize;
    for message in &state.history {
        let bytes = if let Some(user) = &message.user_input_message {
            history_tool_results += user
                .user_input_message_context
                .as_ref()
                .map_or(0, |context| context.tool_results.len());
            user.content.len()
        } else if let Some(assistant) = &message.assistant_response_message {
            history_tool_calls += assistant.tool_uses.len();
            assistant.content.len()
        } else {
            0
        };
        history_text_bytes += bytes;
        largest_history_text_bytes = largest_history_text_bytes.max(bytes);
    }
    tracing::info!(
        event = "proxy.upstream.logical_request",
        trace_id,
        attempt,
        phase,
        fingerprint_version = 1u32,
        logical_payload_sha256 = %fingerprint,
        model = %log_model(&current.model_id),
        max_tokens = payload.max_output_tokens(),
        history_messages = state.history.len(),
        history_text_bytes,
        largest_history_text_bytes,
        history_tool_calls,
        history_tool_results,
        current_text_bytes = current.content.len(),
        current_tool_results = current.user_input_message_context.as_ref().map_or(0, |context| context.tool_results.len()),
        current_tools = current.user_input_message_context.as_ref().map_or(0, |context| context.tools.len()),
        has_additional_model_fields = payload.additional_model_request_fields.is_some(),
        "prepared logical Kiro request; fingerprint excludes profile and random conversation identifiers"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload() -> KiroPayload {
        serde_json::from_value(json!({
            "conversationState": {
                "agentTaskType":"vibe", "chatTriggerType":"MANUAL",
                "conversationId":"random-a", "agentContinuationId":"random-b",
                "history":[{"assistantResponseMessage":{"content":"Original fact"}}],
                "currentMessage":{"userInputMessage":{
                    "content":"Read 中文😀", "modelId":"model-a", "origin":"AI_EDITOR",
                    "userInputMessageContext":{"tools":[{"toolSpecification":{
                        "name":"Read", "description":"Read an owned file",
                        "inputSchema":{"json":{"type":"object","additionalProperties":false}}
                    }}]}
                }}
            },
            "profileArn":"private-profile",
            "inferenceConfig":{"maxTokens":128000}
        }))
        .unwrap()
    }

    #[test]
    fn ignores_only_profile_and_random_identifiers_without_mutation() {
        let original = payload();
        let before = serde_json::to_value(&original).unwrap();
        let mut changed = original.clone();
        changed.profile_arn = Some("other-private-profile".into());
        changed.conversation_state.conversation_id = "another-conversation".into();
        changed.conversation_state.agent_continuation_id = Some("another-turn".into());
        assert_eq!(
            logical_fingerprint(&original),
            logical_fingerprint(&changed)
        );
        assert_eq!(serde_json::to_value(&original).unwrap(), before);
        assert_eq!(logical_fingerprint(&original).unwrap().len(), 64);
    }

    #[test]
    fn contract_changes_never_share_the_original_fingerprint() {
        let original = payload();
        let value = serde_json::to_value(&original).unwrap();
        for (pointer, replacement) in [
            ("/inferenceConfig/maxTokens", json!(8192)),
            ("/conversationState/currentMessage/userInputMessage/modelId", json!("model-b")),
            ("/conversationState/currentMessage/userInputMessage/content", json!("Different request")),
            ("/conversationState/currentMessage/userInputMessage/userInputMessageContext/tools/0/toolSpecification/inputSchema/json/additionalProperties", json!(true)),
            ("/conversationState/history", json!([{"assistantResponseMessage":{"content":"Preserve this fact"}}])),
        ] {
            let mut changed = value.clone();
            *changed.pointer_mut(pointer).unwrap() = replacement;
            let changed: KiroPayload = serde_json::from_value(changed).unwrap();
            assert_ne!(logical_fingerprint(&original), logical_fingerprint(&changed), "{pointer}");
        }
    }
}
