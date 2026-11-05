//! Expands credential references that the model repeats in its answer.
//!
//! Tool inputs are restored at the transport boundary, but plain assistant
//! text used to reach the client still masked. A reference may be split
//! across stream chunks, so a trailing partial reference is held back until
//! the next text chunk, a content boundary, or the end of the round. Usage
//! and other metadata pass through without breaking an incomplete reference.
//!
//! Reasoning stays masked: its signature covers the text the model produced.

use crosslane_kiro::KiroEvent;
use crosslane_translate::redaction::{partial_marker_start, RedactionSession};

pub(super) struct VisibleTextRestorer {
    session: RedactionSession,
    pending: String,
}

impl VisibleTextRestorer {
    pub(super) fn new(session: RedactionSession) -> Self {
        Self {
            session,
            pending: String::new(),
        }
    }

    pub(super) fn push(&mut self, event: KiroEvent) -> Vec<KiroEvent> {
        if !self.session.is_enabled() {
            return vec![event];
        }
        if matches!(
            &event,
            KiroEvent::MessageMetadata { .. } | KiroEvent::Usage { .. } | KiroEvent::Other { .. }
        ) {
            return vec![event];
        }
        let KiroEvent::AssistantResponse { content } = event else {
            let mut output = self.flush();
            output.push(event);
            return output;
        };
        self.pending.push_str(&content);
        let split = partial_marker_start(&self.pending);
        let held = self.pending.split_off(split);
        let ready = std::mem::replace(&mut self.pending, held);
        self.text(&ready)
    }

    pub(super) fn finish(mut self) -> Vec<KiroEvent> {
        self.flush()
    }

    fn flush(&mut self) -> Vec<KiroEvent> {
        let ready = std::mem::take(&mut self.pending);
        self.text(&ready)
    }

    fn text(&self, text: &str) -> Vec<KiroEvent> {
        if text.is_empty() {
            return Vec::new();
        }
        vec![KiroEvent::AssistantResponse {
            content: self.session.restore_visible_text(text),
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "sk-test-9f8e7d6c5b4a3928170615";

    fn masked(session: &RedactionSession) -> String {
        let marker = session.redact_text(SECRET, &[SECRET]);
        assert!(marker.starts_with("__CROSSLANE_SECRET_"), "{marker}");
        marker
    }

    fn text(events: &[KiroEvent]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                KiroEvent::AssistantResponse { content } => Some(content.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn references_split_across_chunks_are_restored() {
        let session = RedactionSession::new();
        let marker = masked(&session);
        let answer = format!("The key is {marker}, keep it.");
        for size in [1, 3, 7, 16, answer.len()] {
            let mut restorer = VisibleTextRestorer::new(session.clone());
            let mut output = Vec::new();
            let bytes = answer.as_bytes();
            for chunk in bytes.chunks(size) {
                let content = String::from_utf8(chunk.to_vec()).expect("ascii");
                output.extend(restorer.push(KiroEvent::AssistantResponse { content }));
                assert!(
                    !text(&output).contains("__CROSSLANE_SECRET_"),
                    "chunk {size}"
                );
            }
            output.extend(restorer.finish());
            assert_eq!(
                text(&output),
                format!("The key is {SECRET}, keep it."),
                "chunk {size}"
            );
        }
    }

    #[test]
    fn references_split_by_metadata_are_restored_without_changing_metadata() {
        let session = RedactionSession::new();
        let marker = masked(&session);
        let metadata = [
            KiroEvent::MessageMetadata {
                usage: serde_json::json!({"usage":{"inputTokens":100}}),
            },
            KiroEvent::Usage {
                usage: serde_json::json!({"outputTokens":40}),
            },
            KiroEvent::Other {
                event_type: "contextUsageEvent".into(),
                payload: serde_json::json!({"contextUsagePercentage":25.0}),
            },
        ];
        for split in 1..marker.len() {
            let mut restorer = VisibleTextRestorer::new(session.clone());
            let mut output = restorer.push(KiroEvent::AssistantResponse {
                content: format!("key: {}", &marker[..split]),
            });
            assert_eq!(text(&output), "key: ");
            for event in &metadata {
                let passed = restorer.push(event.clone());
                assert_eq!(passed, vec![event.clone()]);
                output.extend(passed);
            }
            output.extend(restorer.push(KiroEvent::AssistantResponse {
                content: format!("{} done", &marker[split..]),
            }));
            output.extend(restorer.finish());
            assert_eq!(text(&output), format!("key: {SECRET} done"), "{split}");
        }
    }

    #[test]
    fn unfinished_reference_after_metadata_is_flushed_at_eof() {
        let session = RedactionSession::new();
        let marker = masked(&session);
        let mut restorer = VisibleTextRestorer::new(session);
        assert!(restorer
            .push(KiroEvent::AssistantResponse {
                content: marker[..24].to_owned(),
            })
            .is_empty());
        let metadata = KiroEvent::Usage {
            usage: serde_json::json!({"outputTokens":40}),
        };
        assert_eq!(restorer.push(metadata.clone()), vec![metadata]);
        assert_eq!(text(&restorer.finish()), marker[..24]);
    }

    #[test]
    fn held_text_is_flushed_before_other_events_and_reasoning_is_untouched() {
        let session = RedactionSession::new();
        let marker = masked(&session);
        let mut restorer = VisibleTextRestorer::new(session);
        let mut output = restorer.push(KiroEvent::AssistantResponse {
            content: format!("a {}", &marker[..10]),
        });
        let reasoning = KiroEvent::Reasoning {
            content: marker.clone(),
            signature: None,
            redacted_content: None,
        };
        output.extend(restorer.push(reasoning.clone()));
        assert_eq!(
            output,
            vec![
                KiroEvent::AssistantResponse {
                    content: "a ".into()
                },
                KiroEvent::AssistantResponse {
                    content: marker[..10].to_owned()
                },
                reasoning,
            ]
        );
    }

    #[test]
    fn disabled_sessions_pass_events_through() {
        let session = RedactionSession::with_enabled(false);
        let mut restorer = VisibleTextRestorer::new(session);
        let event = KiroEvent::AssistantResponse {
            content: "__CROSSLANE_".into(),
        };
        assert_eq!(restorer.push(event.clone()), vec![event]);
    }
}
