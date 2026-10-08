//! Bounded recovery for clients that do not retain a returned Claude
//! `compaction` block.

use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crosslane_translate::ClaudeMessage;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const REPLAY_TTL: Duration = Duration::from_secs(60 * 60);
const MAX_REPLAY_ENTRIES: usize = 256;
const MAX_CHECKPOINT_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone, Copy)]
pub(crate) struct CompactionReplaySource {
    message_count: usize,
    digest: [u8; 32],
}

impl CompactionReplaySource {
    pub(crate) fn capture(messages: &[ClaudeMessage]) -> Option<Self> {
        Some(Self {
            message_count: messages.len(),
            digest: digest_messages(messages)?,
        })
    }

    pub(crate) fn message_count(self) -> usize {
        self.message_count
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CompactionReplayApplied {
    pub(crate) source_message_count: usize,
    pub(crate) incoming_message_count: usize,
    pub(crate) checkpoint_bytes: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct CompactionReplay {
    source_message_count: usize,
    incoming_message_count: usize,
    checkpoint: String,
}

impl CompactionReplay {
    pub(crate) fn apply_to(
        &self,
        messages: &mut [ClaudeMessage],
    ) -> Option<CompactionReplayApplied> {
        let assistant = messages.get_mut(self.source_message_count)?;
        if assistant.role != "assistant" || has_completed_compaction(&assistant.content) {
            return None;
        }
        prepend_compaction(&mut assistant.content, &self.checkpoint)?;
        Some(CompactionReplayApplied {
            source_message_count: self.source_message_count,
            incoming_message_count: self.incoming_message_count,
            checkpoint_bytes: self.checkpoint.len(),
        })
    }
}

#[derive(Debug, Clone)]
struct ReplayEntry {
    source: CompactionReplaySource,
    checkpoint: String,
    expires_at: Instant,
    last_used_at: Instant,
}

/// Stores only a checkpoint and a digest of the exact history it replaces.
/// Request contents are never retained. The digest check makes the stable
/// affinity key an optimization rather than an authorization or correctness
/// boundary.
#[derive(Debug, Default)]
pub(crate) struct CompactionReplayTracker {
    entries: Mutex<HashMap<String, ReplayEntry>>,
}

impl CompactionReplayTracker {
    pub(crate) fn remember(
        &self,
        scope: String,
        source: CompactionReplaySource,
        checkpoint: String,
    ) -> bool {
        if checkpoint.is_empty() || checkpoint.len() > MAX_CHECKPOINT_BYTES {
            return false;
        }
        let now = Instant::now();
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        prune_expired(&mut entries, now);
        entries.insert(
            scope,
            ReplayEntry {
                source,
                checkpoint,
                expires_at: now + REPLAY_TTL,
                last_used_at: now,
            },
        );
        while entries.len() > MAX_REPLAY_ENTRIES {
            let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used_at)
                .map(|(scope, _)| scope.clone())
            else {
                break;
            };
            entries.remove(&oldest);
        }
        true
    }

    /// Inserts the cached checkpoint at the exact assistant boundary following
    /// the source request. The ordinary protocol normalizer then discards only
    /// the history covered by that checkpoint and preserves every newer turn.
    pub(crate) fn apply(
        &self,
        scope: &str,
        messages: &mut [ClaudeMessage],
    ) -> Option<CompactionReplayApplied> {
        self.find(scope, messages)?.apply_to(messages)
    }

    /// Verifies a replay against the raw request once. Callers such as the
    /// token-count path may then apply the same verified boundary again after
    /// attachment hydration has rewritten values without re-fetching them.
    pub(crate) fn find(&self, scope: &str, messages: &[ClaudeMessage]) -> Option<CompactionReplay> {
        let now = Instant::now();
        let entry = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            prune_expired(&mut entries, now);
            entries.get(scope).cloned()
        }?;
        if messages.len() <= entry.source.message_count {
            return None;
        }
        let assistant = messages.get(entry.source.message_count)?;
        if assistant.role != "assistant" || has_completed_compaction(&assistant.content) {
            return None;
        }
        if digest_messages(&messages[..entry.source.message_count])? != entry.source.digest {
            return None;
        }

        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(current) = entries.get_mut(scope) {
            if current.source.digest == entry.source.digest {
                current.last_used_at = now;
                current.expires_at = now + REPLAY_TTL;
            }
        }
        Some(CompactionReplay {
            source_message_count: entry.source.message_count,
            incoming_message_count: messages.len(),
            checkpoint: entry.checkpoint,
        })
    }
}

fn prune_expired(entries: &mut HashMap<String, ReplayEntry>, now: Instant) {
    entries.retain(|_, entry| entry.expires_at > now);
}

fn has_completed_compaction(content: &Value) -> bool {
    content.as_array().is_some_and(|blocks| {
        blocks.iter().any(|block| {
            block.get("type").and_then(Value::as_str) == Some("compaction")
                && block
                    .get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|content| !content.is_empty())
        })
    })
}

fn prepend_compaction(content: &mut Value, checkpoint: &str) -> Option<()> {
    let boundary = json!({"type":"compaction", "content":checkpoint});
    match content {
        Value::Array(blocks) => blocks.insert(0, boundary),
        Value::String(text) => {
            let text = std::mem::take(text);
            *content = Value::Array(if text.is_empty() {
                vec![boundary]
            } else {
                vec![boundary, json!({"type":"text", "text":text})]
            });
        }
        _ => return None,
    }
    Some(())
}

fn digest_messages(messages: &[ClaudeMessage]) -> Option<[u8; 32]> {
    let mut writer = DigestWriter(Sha256::new());
    writer.0.update(b"crosslane-compaction-replay-v1\0");
    serde_json::to_writer(&mut writer, messages).ok()?;
    Some(writer.0.finalize().into())
}

struct DigestWriter(Sha256);

impl Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosslane_translate::ClaudeRequest;

    fn messages(value: Value) -> Vec<ClaudeMessage> {
        serde_json::from_value::<ClaudeRequest>(json!({
            "model":"test",
            "max_tokens":1,
            "messages":value
        }))
        .expect("request")
        .messages
    }

    #[test]
    fn replays_only_at_an_exact_extended_history_boundary() {
        let tracker = CompactionReplayTracker::default();
        let source = messages(json!([
            {"role":"user","content":"old"},
            {"role":"assistant","content":"answer"},
            {"role":"user","content":"compact this"}
        ]));
        let replay_source = CompactionReplaySource::capture(&source).expect("source");
        assert!(tracker.remember(
            "service:conversation".into(),
            replay_source,
            "durable checkpoint".into()
        ));

        let mut extended = source.clone();
        extended.extend(messages(json!([
            {"role":"assistant","content":"new answer"},
            {"role":"user","content":"continue"}
        ])));
        let applied = tracker
            .apply("service:conversation", &mut extended)
            .expect("replay");
        assert_eq!(applied.source_message_count, 3);
        assert_eq!(applied.incoming_message_count, 5);
        assert_eq!(
            extended[3].content[0],
            json!({"type":"compaction","content":"durable checkpoint"})
        );
        assert_eq!(
            extended[3].content[1],
            json!({"type":"text","text":"new answer"})
        );

        let mut diverged = source;
        diverged[0].content = json!("different");
        diverged.extend(messages(json!([
            {"role":"assistant","content":"new answer"},
            {"role":"user","content":"continue"}
        ])));
        assert!(tracker
            .apply("service:conversation", &mut diverged)
            .is_none());
    }

    #[test]
    fn does_not_duplicate_a_client_retained_checkpoint() {
        let tracker = CompactionReplayTracker::default();
        let source = messages(json!([{"role":"user","content":"old"}]));
        tracker.remember(
            "scope".into(),
            CompactionReplaySource::capture(&source).unwrap(),
            "checkpoint".into(),
        );
        let mut extended = source;
        extended.extend(messages(json!([
            {"role":"assistant","content":[
                {"type":"compaction","content":"checkpoint"},
                {"type":"text","text":"answer"}
            ]},
            {"role":"user","content":"continue"}
        ])));
        assert!(tracker.apply("scope", &mut extended).is_none());
        assert_eq!(extended[1].content.as_array().unwrap().len(), 2);
    }
}
