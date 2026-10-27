//! A local notice in the existing final text block, never an error or tool event.

use crosslane_translate::{ClaudeRequest, RedactionSession};
use serde_json::Value;

pub(in crate::http) const NOTICE: &str = "\n\n---\n\n> **⚠️ Crosslane 安全提示：已触发凭证脱敏**\n>\n> 本次识别到的敏感凭证已在发送给上游模型前替换为占位符。工具执行所需参数会在本地还原。\n>\n> **请注意凭证的使用方式：**优先使用环境变量引用或安全凭证配置，避免直接粘贴明文凭证。";

/// Freeze the policy with the request. Unknown non-text formats are treated as
/// structured; effort-only controls remain eligible.
pub(in crate::http) fn requested(
    request: &ClaudeRequest,
    enabled: bool,
    client_compaction: bool,
) -> bool {
    let structured = |format: &Value| !format.is_null() && format["type"] != "text";
    enabled
        && !client_compaction
        && !request
            .output_config
            .iter()
            .chain(
                request
                    .messages
                    .iter()
                    .filter_map(|message| message.output_config.as_ref()),
            )
            .any(|config| config.extra.get("format").is_some_and(structured))
        && !["output_format", "response_format"]
            .iter()
            .any(|key| request.extra.get(*key).is_some_and(structured))
}

pub(in crate::http) fn should_append(
    requested: bool,
    session: &RedactionSession,
    stop_reason: &str,
    text: &str,
) -> bool {
    requested
        && stop_reason == "end_turn"
        && !text.trim().is_empty()
        && session.has_transmitted_redactions()
        // Preserve JSON answers even without a protocol-level output format.
        && serde_json::from_str::<Value>(text).is_err()
}

pub(in crate::http) fn append_to_response(
    mut response: Value,
    requested: bool,
    session: &RedactionSession,
) -> Value {
    let stop = response["stop_reason"].as_str().unwrap_or_default();
    let Some(content) = response["content"].as_array() else {
        return response;
    };
    if content.iter().any(|block| block["type"] == "tool_use") {
        return response;
    }
    let Some(block) = content.last().filter(|block| block["type"] == "text") else {
        return response;
    };
    let Some(text) = block["text"].as_str() else {
        return response;
    };
    if should_append(requested, session, stop, text) {
        let text = format!("{text}{NOTICE}");
        let last = response["content"]
            .as_array_mut()
            .unwrap()
            .last_mut()
            .unwrap();
        last["text"] = Value::String(text);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(extra: Value) -> ClaudeRequest {
        let mut body = json!({"model":"fixture","max_tokens":256,"messages":[{"role":"user","content":"hello"}]});
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(body).unwrap()
    }

    fn sent_session() -> RedactionSession {
        let session = RedactionSession::new();
        let masked = session.redact_text("token=notice-fixture-secret", &[]);
        session.record_transmitted_payload(&json!({"content":masked}));
        session
    }

    #[test]
    fn structured_requests_and_client_summaries_skip_notices() {
        for extra in [
            json!({"output_config":{"format":{"type":"json_schema","schema":{"type":"object"}}}}),
            json!({"output_format":{"type":"json_schema"}}),
            json!({"response_format":{"type":"json_object"}}),
            json!({"messages":[{"role":"user","content":"hello","output_config":{"format":{"type":"json_schema"}}}]}),
        ] {
            assert!(!requested(&request(extra), true, false));
        }
        let normal =
            request(json!({"output_config":{"effort":"high"},"response_format":{"type":"text"}}));
        assert!(requested(&normal, true, false));
        assert!(!requested(&normal, false, false));
        assert!(!requested(&normal, true, true));
    }

    #[test]
    fn only_completed_non_json_answers_from_transmitted_redactions_get_notices() {
        let sent = sent_session();
        assert!(should_append(true, &sent, "end_turn", "answer"));
        for stop in [
            "tool_use",
            "max_tokens",
            "stop_sequence",
            "pause_turn",
            "refusal",
            "",
        ] {
            assert!(!should_append(true, &sent, stop, "answer"));
        }
        for text in [
            "",
            "  ",
            "{\"ok\":true}",
            "[1,2]",
            "42",
            "true",
            "null",
            "\"ok\"",
        ] {
            assert!(!should_append(true, &sent, "end_turn", text));
        }
        assert!(!should_append(false, &sent, "end_turn", "answer"));
        assert!(!should_append(
            true,
            &RedactionSession::new(),
            "end_turn",
            "answer"
        ));
        let estimated = RedactionSession::new();
        estimated.redact_text("token=budget-only-secret", &[]);
        assert!(!should_append(true, &estimated, "end_turn", "answer"));
    }

    #[test]
    fn nonstream_notice_extends_existing_text_and_preserves_metadata_and_tools() {
        let session = sent_session();
        let original = json!({"content":[{"type":"thinking","thinking":"reason"},{"type":"text","text":"answer","citations":[]}],"stop_reason":"end_turn","usage":{"output_tokens":3},"id":"message"});
        let mut expected = original.clone();
        expected["content"][1]["text"] = json!(format!("answer{NOTICE}"));
        assert_eq!(
            append_to_response(original.clone(), true, &session),
            expected
        );
        assert!(NOTICE.starts_with("\n\n---\n\n> **⚠️ Crosslane 安全提示："));
        assert!(NOTICE.contains("\n>\n> **请注意凭证的使用方式：**"));
        assert!(!NOTICE.contains('\u{1b}'));
        assert!(!NOTICE.contains('<'));
        assert_eq!(
            append_to_response(original.clone(), false, &session),
            original
        );
        for content in [
            json!([]),
            json!([{"type":"thinking","thinking":"reason"}]),
            json!([{"type":"text","text":"{}"}]),
            json!([{"type":"tool_use","id":"call","name":"request","input":{"token":"real"}},{"type":"text","text":"answer"}]),
        ] {
            let response = json!({"content":content,"stop_reason":"end_turn"});
            assert_eq!(
                append_to_response(response.clone(), true, &session),
                response
            );
        }
    }
}
