//! A local notice in the existing final text block, never an error or tool event.

use crosslane_core::config::{FeaturesConfig, DEFAULT_REQUEST_REDACTION_NOTICE_TEXT};
use crosslane_translate::{ClaudeRequest, KiroPayload, RedactionSession};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// Recognize notices already present in conversations created by older builds.
const LEGACY_NOTICE_TEXT: &str = "---\n\n> **⚠️ Crosslane 安全提示：已触发凭证脱敏**\n>\n> 本次识别到的敏感凭证已在发送给上游模型前替换为占位符。工具执行所需参数会在本地还原。\n>\n> **请注意凭证的使用方式：**优先使用环境变量引用或安全凭证配置，避免直接粘贴明文凭证。";

const MAX_RECORDED_NOTICES: usize = 1024;
const MAX_RECORDED_NOTICE_CHAIN: usize = 16;
const RECORDED_NOTICE_TTL: Duration = Duration::from_secs(60 * 60);

/// Proof of locally appended custom notices. Retain only bounded whole-answer
/// fingerprints and the original byte length, never answer or credential text.
#[derive(Default)]
pub(crate) struct NoticeHistory {
    entries: Mutex<HashMap<[u8; 32], RecordedNotice>>,
}

struct RecordedNotice {
    original_len: Option<usize>,
    recorded_at: Instant,
}

impl NoticeHistory {
    pub(in crate::http) fn remember(&self, text: &str, notice: &str) {
        let Some(original) = text
            .strip_suffix(notice)
            .filter(|_| !notice.trim().is_empty())
        else {
            return;
        };
        let digest = Sha256::digest(text.as_bytes()).into();
        let now = Instant::now();
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        entries.retain(|_, record| {
            now.saturating_duration_since(record.recorded_at) < RECORDED_NOTICE_TTL
        });
        entries
            .entry(digest)
            .and_modify(|record| {
                // Identical bytes can be emitted under two different configured
                // suffixes. Do not guess which prefix belongs to later history.
                if record.original_len != Some(original.len()) {
                    record.original_len = None;
                }
                record.recorded_at = now;
            })
            .or_insert(RecordedNotice {
                original_len: Some(original.len()),
                recorded_at: now,
            });
        if entries.len() > MAX_RECORDED_NOTICES {
            if let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, record)| record.recorded_at)
                .map(|(digest, _)| *digest)
            {
                entries.remove(&oldest);
            }
        }
    }

    // None is an unknown/expired answer; Some(None) is an active but ambiguous
    // record. The latter must not fall back to guessing a built-in suffix.
    fn recorded_prefix(&self, text: &str) -> Option<Option<usize>> {
        let digest: [u8; 32] = Sha256::digest(text.as_bytes()).into();
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&digest)
            .filter(|record| record.recorded_at.elapsed() < RECORDED_NOTICE_TTL)
            .map(|record| record.original_len)
    }

    #[cfg(test)]
    fn original_len(&self, text: &str) -> Option<usize> {
        self.recorded_prefix(text).flatten()
    }
}

/// Remove verified custom notices or complete built-in notice suffixes. An
/// arbitrary configured phrase such as "Done" is not evidence of an insertion.
pub(in crate::http) fn strip_history(payload: &mut KiroPayload, history: &NoticeHistory) {
    let notices = [DEFAULT_REQUEST_REDACTION_NOTICE_TEXT, LEGACY_NOTICE_TEXT]
        .map(|text| format!("\n\n{text}"));
    for message in &mut payload.conversation_state.history {
        let Some(assistant) = &mut message.assistant_response_message else {
            continue;
        };
        let mut remaining = assistant.content.as_str();
        let mut allow_builtin = true;
        // Bound full-answer hashing for nested custom receipts. Repeated
        // built-in notices below need only literal suffix comparisons.
        for _ in 0..MAX_RECORDED_NOTICE_CHAIN {
            match history.recorded_prefix(remaining) {
                Some(Some(length)) => remaining = &remaining[..length],
                Some(None) => {
                    allow_builtin = false;
                    break;
                }
                None => break,
            }
        }
        if allow_builtin && notices.iter().any(|notice| remaining.ends_with(notice)) {
            let (_, _, inside_code) = normalized_markdown(remaining);
            // Neither built-in text contains code delimiters, so repeated
            // copies share this code state. Scan the large prefix only once.
            if !inside_code {
                while let Some(prefix) = notices
                    .iter()
                    .find_map(|notice| remaining.strip_suffix(notice.as_str()))
                {
                    remaining = prefix;
                }
            }
        }
        // Do not turn a notice-only assistant turn into an invalid empty turn.
        if !remaining.trim().is_empty() || !assistant.tool_uses.is_empty() {
            let length = remaining.len();
            assistant.content.truncate(length);
        }
    }
}

/// Match the complete configured/default notice, allowing presentation changes
/// such as blockquotes, emphasis, line wrapping and CRLF. Whole-line boundaries
/// keep short custom notices from matching ordinary phrases within the answer.
pub(in crate::http) fn already_present(text: &str, configured_text: &str) -> bool {
    let (normalized, boundaries, _) = normalized_markdown(text);
    [
        configured_text,
        DEFAULT_REQUEST_REDACTION_NOTICE_TEXT,
        LEGACY_NOTICE_TEXT,
    ]
    .into_iter()
    .any(|notice| {
        let (comparable, _, _) = normalized_markdown(notice);
        if comparable.is_empty() || comparable.contains('\0') {
            // Pure formatting or explicitly code-fenced custom notices need
            // an exact suffix match; ordinary fenced examples never match a
            // prose notice through Markdown normalization.
            let notice = notice.trim();
            return !notice.is_empty()
                && text.trim_end().strip_suffix(notice).is_some_and(|prefix| {
                    prefix.is_empty()
                        || prefix
                            .rsplit_once('\n')
                            .is_some_and(|(_, indent)| indent.trim().is_empty())
                });
        }
        normalized
            .match_indices(comparable.as_str())
            .any(|(start, _)| {
                boundaries.binary_search(&start).is_ok()
                    && boundaries
                        .binary_search(&(start + comparable.len()))
                        .is_ok()
            })
    })
}

fn normalized_markdown(text: &str) -> (String, Vec<usize>, bool) {
    let mut normalized = String::new();
    let mut boundaries = vec![0];
    let mut fence = None;
    let mut inline_ticks = 0;
    for line in text.lines() {
        let mut line = line.trim();
        while let Some(quoted) = line.strip_prefix('>') {
            line = quoted.trim_start();
        }
        if let Some((marker, width)) = fence {
            if line.starts_with(marker) {
                let run = line.chars().take_while(|ch| *ch == marker).count();
                if run >= width && line[run..].trim().is_empty() {
                    fence = None;
                }
            }
            continue;
        }
        if inline_ticks == 0 && (line.starts_with('`') || line.starts_with('~')) {
            let marker = line.chars().next().unwrap();
            let width = line.chars().take_while(|ch| *ch == marker).count();
            if width >= 3 {
                fence = Some((marker, width));
                // A barrier prevents matches from joining prose across a code
                // example. Code examples are answer data, not actual notices.
                normalized.push('\0');
                boundaries.push(normalized.len());
                continue;
            }
        }
        if ['-', '*', '_'].iter().any(|separator| {
            line.chars().filter(|ch| !ch.is_whitespace()).count() >= 3
                && line
                    .chars()
                    .all(|ch| ch == *separator || ch.is_whitespace())
        }) {
            continue;
        }
        let line = line.trim_start_matches('#').trim_start();
        let first = line.chars().find(|ch| !matches!(*ch, '*' | '`'));
        if inline_ticks == 0
            && normalized
                .chars()
                .last()
                .is_some_and(|ch| ch.is_ascii_alphanumeric())
            && first.is_some_and(|ch| ch.is_ascii_alphanumeric())
        {
            normalized.push(' ');
            boundaries.push(normalized.len());
        }
        let mut chars = line.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '`' {
                let mut width = 1;
                while chars.peek() == Some(&'`') {
                    chars.next();
                    width += 1;
                }
                if inline_ticks == 0 {
                    inline_ticks = width;
                } else if inline_ticks == width {
                    inline_ticks = 0;
                } else {
                    normalized.extend(std::iter::repeat_n('`', width));
                }
            } else if inline_ticks > 0 {
                normalized.push(ch);
            } else if ch.is_whitespace() {
                if !normalized.ends_with(' ') {
                    normalized.push(' ');
                }
            } else if !matches!(ch, '*' | '\u{fe0f}') {
                // Underscores and whitespace inside code are meaningful:
                // $API_KEY is not $APIKEY, and `export TOKEN` is not `exportTOKEN`.
                normalized.push(ch);
            }
        }
        if boundaries.last() != Some(&normalized.len()) {
            boundaries.push(normalized.len());
        }
    }
    (normalized, boundaries, fence.is_some() || inline_ticks != 0)
}

/// Freeze the policy with the request. Unknown non-text formats are treated as
/// structured; effort-only controls remain eligible.
pub(in crate::http) fn requested(
    request: &ClaudeRequest,
    features: &FeaturesConfig,
    client_compaction: bool,
) -> Option<String> {
    let structured = |format: &Value| !format.is_null() && format["type"] != "text";
    let eligible = features.enable_request_redaction_notice
        && !features.request_redaction_notice_text.trim().is_empty()
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
            .any(|key| request.extra.get(*key).is_some_and(structured));
    eligible.then(|| format!("\n\n{}", features.request_redaction_notice_text))
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
    notice: Option<&str>,
    session: &RedactionSession,
    history: &NoticeHistory,
) -> Value {
    let Some(notice) = notice else {
        return response;
    };
    let stop = response["stop_reason"].as_str().unwrap_or_default();
    let Some(content) = response["content"].as_array() else {
        return response;
    };
    if content.iter().any(|block| block["type"] == "tool_use") {
        return response;
    }
    let visible_text = content
        .iter()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if already_present(&visible_text, notice) {
        return response;
    }
    let Some(block) = content.last().filter(|block| block["type"] == "text") else {
        return response;
    };
    let Some(text) = block["text"].as_str() else {
        return response;
    };
    if should_append(true, session, stop, text) {
        let text = format!("{text}{notice}");
        history.remember(&text, notice);
        history.remember(&format!("{visible_text}{notice}"), notice);
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

    fn append_to_response(
        response: Value,
        notice: Option<&str>,
        session: &RedactionSession,
    ) -> Value {
        super::append_to_response(response, notice, session, &NoticeHistory::default())
    }

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
        session.record_transmitted_payload(&json!({"conversationState":{"currentMessage":{"userInputMessage":{"content":masked}}}}));
        session
    }

    fn features() -> FeaturesConfig {
        FeaturesConfig {
            enable_request_redaction_notice: true,
            ..Default::default()
        }
    }

    #[test]
    fn structured_requests_and_client_summaries_skip_notices() {
        for extra in [
            json!({"output_config":{"format":{"type":"json_schema","schema":{"type":"object"}}}}),
            json!({"output_format":{"type":"json_schema"}}),
            json!({"response_format":{"type":"json_object"}}),
            json!({"messages":[{"role":"user","content":"hello","output_config":{"format":{"type":"json_schema"}}}]}),
        ] {
            assert!(requested(&request(extra), &features(), false).is_none());
        }
        let normal =
            request(json!({"output_config":{"effort":"high"},"response_format":{"type":"text"}}));
        assert!(requested(&normal, &features(), false).is_some());
        assert!(requested(&normal, &FeaturesConfig::default(), false).is_none());
        assert!(requested(&normal, &features(), true).is_none());
    }

    #[test]
    fn notice_text_defaults_to_suspected_credentials_and_freezes_multiline_markdown() {
        let request = request(json!({}));
        let mut features = features();
        let default = requested(&request, &features, false).unwrap();
        assert!(default.contains("疑似敏感凭证"));
        assert!(default.contains("可能存在误报"));
        for blank in ["", " \n\t"] {
            features.request_redaction_notice_text = blank.into();
            assert!(requested(&request, &features, false).is_none());
        }
        features.request_redaction_notice_text =
            "## 自定义提示\n\n- 疑似凭证  \n  请检查 `$TOKEN`。\n".into();
        let notice = requested(&request, &features, false).unwrap();
        let expected = format!("\n\n{}", features.request_redaction_notice_text);
        features.request_redaction_notice_text = "updated for later requests".into();
        assert_eq!(notice, expected);
        let response = append_to_response(
            json!({"content":[{"type":"text","text":"answer"}],"stop_reason":"end_turn"}),
            Some(&notice),
            &sent_session(),
        );
        assert_eq!(response["content"][0]["text"], format!("answer{expected}"));
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
        let notice = requested(&request(json!({})), &features(), false).unwrap();
        let original = json!({"content":[{"type":"thinking","thinking":"reason"},{"type":"text","text":"answer","citations":[]}],"stop_reason":"end_turn","usage":{"output_tokens":3},"id":"message"});
        let mut expected = original.clone();
        expected["content"][1]["text"] = json!(format!("answer{notice}"));
        assert_eq!(
            append_to_response(original.clone(), Some(&notice), &session),
            expected
        );
        assert!(notice.starts_with("\n\n---\n\n> **⚠️ Crosslane 安全提示："));
        assert!(notice.contains("\n>\n> **请注意凭证的使用方式：**"));
        assert!(!notice.contains('\u{1b}'));
        assert!(!notice.contains('<'));
        assert_eq!(
            append_to_response(original.clone(), None, &session),
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
                append_to_response(response.clone(), Some(&notice), &session),
                response
            );
        }
    }

    #[test]
    fn nonstream_notice_is_not_added_twice() {
        let session = sent_session();
        let notice = requested(&request(json!({})), &features(), false).unwrap();
        let response =
            json!({"content":[{"type":"text","text":"answer"}],"stop_reason":"end_turn"});
        let once = append_to_response(response, Some(&notice), &session);
        assert_eq!(
            append_to_response(once.clone(), Some(&notice), &session),
            once
        );
    }

    #[test]
    fn existing_notices_are_recognized_without_matching_partial_mentions() {
        let custom = "## 管理员提示\n\n> **疑似凭证**，请检查 `$TOKEN`。";
        for notice in [
            DEFAULT_REQUEST_REDACTION_NOTICE_TEXT,
            LEGACY_NOTICE_TEXT,
            custom,
        ] {
            let reformatted = notice
                .replace("**", "")
                .replace("> ", "")
                .replace('\n', "\r\n");
            assert!(already_present(
                &format!("answer\n\n{reformatted}\n\nMore details."),
                custom
            ));
        }
        assert!(already_present("answer\n\n---", "---"));
        assert!(already_present("answer\n\n  ---", "  ---"));
        assert!(!already_present(
            "answer\n\n⚠️ Crosslane 安全提示：疑似敏感凭证已脱敏",
            custom
        ));
        assert!(!already_present(
            "The configured notice is ready.",
            "notice is ready."
        ));
        assert!(!already_present(
            "answer\n\n## 管理员提示\n\n> 疑似凭证",
            custom
        ));
    }

    #[test]
    fn nonstream_does_not_append_after_an_echo_in_an_earlier_text_block() {
        let notice = requested(&request(json!({})), &features(), false).unwrap();
        let response = json!({"content":[
            {"type":"text","text":format!("answer\n\n{LEGACY_NOTICE_TEXT}")},
            {"type":"thinking","thinking":"reason"},
            {"type":"text","text":"Further details."}
        ],"stop_reason":"end_turn","usage":{"output_tokens":3}});
        // The echoed notice occupies complete lines even when a later block
        // adds more prose. Thinking content itself never counts as a notice.
        assert_eq!(
            append_to_response(response.clone(), Some(&notice), &sent_session()),
            response
        );
        let response = json!({"content":[
            {"type":"thinking","thinking":LEGACY_NOTICE_TEXT},
            {"type":"text","text":"answer"}
        ],"stop_reason":"end_turn"});
        assert_eq!(
            append_to_response(response, Some(&notice), &sent_session())["content"][1]["text"],
            format!("answer{notice}")
        );
    }

    #[test]
    fn only_exact_notice_suffixes_are_removed_from_upstream_assistant_history() {
        let custom = "> 管理员自定义提示\n> 疑似凭证。";
        let notices = format!(
            "\n\n{LEGACY_NOTICE_TEXT}\n\n{DEFAULT_REQUEST_REDACTION_NOTICE_TEXT}\n\n{custom}"
        );
        let mut payload: KiroPayload = serde_json::from_value(json!({"conversationState":{
            "conversationId":"fixture","chatTriggerType":"MANUAL",
            "currentMessage":{"userInputMessage":{"content":format!("current input{notices}"),"modelId":"fixture","origin":"CLI"}},
            "history":[
                {"userInputMessage":{"content":format!("original user input{notices}"),"modelId":"fixture","origin":"CLI"}},
                {"assistantResponseMessage":{"content":format!("original answer  {notices}"),"toolUses":[{"toolUseId":"fixture","name":"tool","input":{"text":custom}}]}},
                {"assistantResponseMessage":{"content":format!("quoted example\n\n{custom}\n\nMore explanation.")}},
                {"assistantResponseMessage":{"content":format!("\n\n{custom}")}}
            ]
        }})).unwrap();
        let mut expected = serde_json::to_value(&payload).unwrap();
        expected["conversationState"]["history"][1]["assistantResponseMessage"]["content"] =
            json!("original answer  ");
        let history = NoticeHistory::default();
        history.remember(
            &format!("original answer  {notices}"),
            &format!("\n\n{custom}"),
        );
        strip_history(&mut payload, &history);
        assert_eq!(serde_json::to_value(&payload).unwrap(), expected);
    }

    #[test]
    fn review_unverified_custom_text_is_not_deleted_from_history() {
        for content in [
            "Actual task result\n\nDone".to_owned(),
            format!("Example:\n```markdown\n\n{DEFAULT_REQUEST_REDACTION_NOTICE_TEXT}"),
            format!("Example: `\n\n{DEFAULT_REQUEST_REDACTION_NOTICE_TEXT}"),
        ] {
            let mut payload: KiroPayload = serde_json::from_value(json!({"conversationState":{
            "conversationId":"fixture","chatTriggerType":"MANUAL",
            "currentMessage":{"userInputMessage":{"content":"continue","modelId":"fixture","origin":"CLI"}},
            "history":[{"assistantResponseMessage":{"content":content}}]
        }})).unwrap();
            let before = serde_json::to_value(&payload).unwrap();
            strip_history(&mut payload, &NoticeHistory::default());
            assert_eq!(serde_json::to_value(&payload).unwrap(), before);
        }
    }

    #[test]
    fn review_code_examples_and_different_identifiers_are_not_existing_notices() {
        assert!(!already_present(
            "请使用 `$APIKEY`。",
            "请使用 `$API_KEY`。"
        ));
        assert!(!already_present(
            "请执行 `exportTOKEN=value`。",
            "请执行 `export TOKEN=value`。"
        ));
        assert!(!already_present(
            "do not reuse credentials",
            "donot reuse credentials"
        ));
        assert!(!already_present(
            &format!("Example:\n```markdown\n{DEFAULT_REQUEST_REDACTION_NOTICE_TEXT}\n```"),
            DEFAULT_REQUEST_REDACTION_NOTICE_TEXT
        ));
    }

    #[test]
    fn recorded_custom_notices_require_an_exact_answer_and_survive_text_rotation() {
        let history = NoticeHistory::default();
        let notice = "\n\nDone";
        let response = super::append_to_response(
            json!({"content":[{"type":"text","text":"Actual task result"}],"stop_reason":"end_turn"}),
            Some(notice),
            &sent_session(),
            &history,
        );
        let recorded = response["content"][0]["text"].as_str().unwrap();
        assert_eq!(
            history.original_len(recorded),
            Some("Actual task result".len())
        );
        assert!(history
            .original_len("A different task result\n\nDone")
            .is_none());
        let mut payload: KiroPayload = serde_json::from_value(json!({"conversationState":{
            "conversationId":"fixture","chatTriggerType":"MANUAL",
            "currentMessage":{"userInputMessage":{"content":"continue","modelId":"fixture","origin":"CLI"}},
            "history":[{"assistantResponseMessage":{"content":recorded}}]
        }})).unwrap();
        strip_history(&mut payload, &history);
        assert_eq!(
            payload.conversation_state.history[0]
                .assistant_response_message
                .as_ref()
                .unwrap()
                .content,
            "Actual task result"
        );
    }

    #[test]
    fn recorded_notice_fingerprints_are_bounded_and_expire() {
        let history = NoticeHistory::default();
        for index in 0..=MAX_RECORDED_NOTICES {
            history.remember(&format!("answer {index}\n\nDone"), "\n\nDone");
        }
        assert_eq!(history.entries.lock().unwrap().len(), MAX_RECORDED_NOTICES);
        assert!(history.original_len("answer 0\n\nDone").is_none());
        let answer = format!("answer {MAX_RECORDED_NOTICES}\n\nDone");
        assert!(history.original_len(&answer).is_some());
        let digest: [u8; 32] = Sha256::digest(answer.as_bytes()).into();
        history
            .entries
            .lock()
            .unwrap()
            .get_mut(&digest)
            .unwrap()
            .recorded_at = Instant::now() - RECORDED_NOTICE_TTL;
        assert!(history.original_len(&answer).is_none());
    }

    #[test]
    fn conflicting_recorded_suffix_lengths_are_never_guessed() {
        let history = NoticeHistory::default();
        let answer = "Actual result\n\nfirst paragraph\n\nDone";
        history.remember(answer, "\n\nDone");
        assert!(history.original_len(answer).is_some());
        history.remember(answer, "\n\nfirst paragraph\n\nDone");
        assert!(history.original_len(answer).is_none());
        history.remember(answer, "\n\nDone");
        assert!(history.original_len(answer).is_none());
        assert_eq!(history.recorded_prefix(answer), Some(None));
    }

    #[test]
    fn repeated_builtin_notice_suffixes_are_all_removed_without_touching_the_answer() {
        assert!(!DEFAULT_REQUEST_REDACTION_NOTICE_TEXT.contains(['`', '~']));
        assert!(!LEGACY_NOTICE_TEXT.contains(['`', '~']));
        let repeated = format!("\n\n{DEFAULT_REQUEST_REDACTION_NOTICE_TEXT}").repeat(512);
        let mut payload: KiroPayload = serde_json::from_value(json!({"conversationState":{
            "conversationId":"fixture","chatTriggerType":"MANUAL",
            "currentMessage":{"userInputMessage":{"content":"continue","modelId":"fixture","origin":"CLI"}},
            "history":[{"assistantResponseMessage":{"content":format!("Original answer.{repeated}")}}]
        }})).unwrap();
        strip_history(&mut payload, &NoticeHistory::default());
        assert_eq!(
            payload.conversation_state.history[0]
                .assistant_response_message
                .as_ref()
                .unwrap()
                .content,
            "Original answer."
        );
    }
}
