//! Translate Kiro's terminal refusal metadata to the native Messages contract.

use serde_json::{json, Value};

pub(in crate::http) fn native_refusal_details(metadata: &Value) -> Option<Value> {
    let metadata = metadata
        .get("metadataEvent")
        .or_else(|| metadata.get("messageMetadataEvent"))
        .unwrap_or(metadata);
    if metadata["stopReason"] != "CONTENT_FILTERED" {
        return None;
    }
    let mut details = json!({"type":"refusal"});
    if let Some(refusal) = metadata.pointer("/stopDetails/refusal") {
        if let Some(category) = refusal["category"].as_str().filter(|category| {
            !category.is_empty()
                && category.len() <= 64
                && category
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        }) {
            details["category"] = json!(category.to_ascii_lowercase());
        }
        if let Some(explanation) = refusal["explanation"].as_str() {
            details["explanation"] = json!(explanation.chars().take(1024).collect::<String>());
        }
    }
    Some(details)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn terminal_refusal_cancels_buffered_and_late_calls_but_keeps_usage() {
        use crate::http::response::{DecodedResponse, ToolLeakFilter};
        use crosslane_kiro::KiroEvent;
        let mut filter = ToolLeakFilter::new(true, ["checkpoint_reader".into()]);
        filter.set_native_claude_refusals(true);
        let mut decoded = DecodedResponse::default();
        for event in filter.push(KiroEvent::ToolUse {
            id: "partial".into(),
            name: "checkpoint_reader".into(),
            input_delta: "{\"path\":\"/partial".into(),
            stop: false,
        }) {
            decoded.push(event).unwrap();
        }
        for event in filter.push(KiroEvent::MessageMetadata {
            usage: json!({"stopReason":"CONTENT_FILTERED"}),
        }) {
            decoded.push(event).unwrap();
        }
        assert!(decoded.tools.is_empty());
        assert!(filter
            .push(KiroEvent::ToolUse {
                id: "late".into(),
                name: "checkpoint_reader".into(),
                input_delta: "{}".into(),
                stop: true
            })
            .is_empty());
        for event in filter.push(KiroEvent::Usage {
            usage: json!({"inputTokens":31,"outputTokens":7,"creditsConsumed":0.125}),
        }) {
            decoded.push(event).unwrap();
        }
        assert_eq!(decoded.usage.input_tokens, 31);
        assert_eq!(decoded.usage.output_tokens, 7);
        assert_eq!(decoded.usage.credits, 0.125);
        assert!(filter.finish().is_empty());
        assert!(decoded.finalize_tool_inputs().is_ok());
    }

    #[test]
    fn native_refusal_preserves_the_client_recovery_contract() {
        let metadata = json!({"stopReason":"CONTENT_FILTERED", "stopDetails":{"refusal":{
            "category":"REASONING_EXTRACTION", "explanation":"Owned reader request was stopped."
        }}});
        for wrapped in [
            metadata.clone(),
            json!({"metadataEvent":metadata}),
            json!({"messageMetadataEvent":metadata}),
        ] {
            assert_eq!(
                native_refusal_details(&wrapped),
                Some(json!({
                    "type":"refusal", "category":"reasoning_extraction", "explanation":"Owned reader request was stopped."
                }))
            );
        }
        assert_eq!(
            native_refusal_details(&json!({"stopReason":"END_TURN"})),
            None
        );
        assert_eq!(
            native_refusal_details(
                &json!({"stopDetails":{"refusal":{"category":"REASONING_EXTRACTION"}}})
            ),
            None
        );
    }

    #[test]
    fn refusal_details_are_optional_bounded_and_do_not_copy_unknown_metadata() {
        let details = native_refusal_details(&json!({"stopReason":"CONTENT_FILTERED", "stopDetails":{"refusal":{
            "category":"FUTURE_CATEGORY", "explanation":"中😀".repeat(900), "private_debug":"must not appear"
        }}})).unwrap();
        assert_eq!(details["category"], "future_category");
        assert!(details["explanation"].as_str().unwrap().chars().count() <= 1024);
        assert!(details.get("private_debug").is_none());
        assert_eq!(
            native_refusal_details(&json!({"stopReason":"CONTENT_FILTERED"})),
            Some(json!({"type":"refusal"}))
        );
    }
}
