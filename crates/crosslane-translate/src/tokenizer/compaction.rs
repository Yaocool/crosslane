//! Lossless input partitioning for semantic checkpoints. Only the already
//! rendered, tool-free summary transcript is split; live tool JSON stays intact.

use std::collections::VecDeque;

use super::{Arc, KiroHistoryMessage, KiroPayload, TokenCountCache};
use crate::sanitize_kiro_tool_history;

const MAX_SUMMARY_PARTS: usize = 16;
const PART_INSTRUCTION: &str = "\n\nThis is a contiguous part of a larger conversation, in chronological order. Preserve exact identifiers and decisions found in this part. Missing facts may occur in other parts: do not infer that they are absent, unknown, or null. Another model will receive all part summaries in order; later decisions supersede earlier ones.";

impl TokenCountCache {
    /// Keeps the complete summary input when it fits. Otherwise packs whole
    /// messages and splits only an individually oversized rendered text at
    /// lossless UTF-8 token boundaries. Every input character and document is
    /// assigned to a part; exceeding a bound returns an error, never a prefix.
    pub async fn partition_compaction_summary(
        &self,
        mut payload: KiroPayload,
        maximum: usize,
    ) -> Result<Vec<KiroPayload>, String> {
        let mut normalized = payload.clone();
        sanitize_kiro_tool_history(&mut normalized);
        if self.estimate_kiro_payload(&normalized).await? <= maximum {
            return Ok(vec![normalized]);
        }
        payload
            .conversation_state
            .current_message
            .user_input_message
            .content
            .push_str(PART_INSTRUCTION);
        let mut pending: VecDeque<_> =
            std::mem::take(&mut payload.conversation_state.history).into();
        let mut parts = Vec::new();
        let mut part = payload.clone();
        let base_tokens = self.estimate_kiro_payload(&payload).await?;
        let mut part_tokens = base_tokens;
        while let Some(message) = pending.pop_front() {
            // Count each record once, including its worst-case alternating-role
            // framing. Recounting the entire growing chunk is quadratic for
            // conversations with many short messages.
            let mut candidate = payload.clone();
            candidate.conversation_state.history.push(message.clone());
            sanitize_kiro_tool_history(&mut candidate);
            let message_tokens = self
                .estimate_kiro_payload(&candidate)
                .await?
                .saturating_sub(base_tokens);
            if part_tokens.saturating_add(message_tokens) <= maximum {
                // Keep the unnormalized records while packing so synthesized
                // role placeholders do not accumulate between attempts.
                part.conversation_state.history.push(message);
                part_tokens = part_tokens.saturating_add(message_tokens);
                continue;
            }
            if !part.conversation_state.history.is_empty() {
                sanitize_kiro_tool_history(&mut part);
                parts.push(part);
                if parts.len() >= MAX_SUMMARY_PARTS {
                    return Err("summary input exceeds the 16-part limit".into());
                }
                part = payload.clone();
                part_tokens = base_tokens;
                pending.push_front(message);
                continue;
            }

            let mut empty_message = message.clone();
            history_text_mut(&mut empty_message)?.clear();
            let mut overhead = payload.clone();
            overhead.conversation_state.history.push(empty_message);
            sanitize_kiro_tool_history(&mut overhead);
            let overhead = self.estimate_kiro_payload(&overhead).await?;
            // Also leave room for independently encoded fragment boundaries.
            let budget = maximum.saturating_sub(overhead).saturating_sub(32);
            if budget < 32 {
                return Err(
                    "summary instructions or an indivisible document exceed its model window"
                        .into(),
                );
            }
            let mut template = message;
            let content = std::mem::take(history_text_mut(&mut template)?);
            let fragments = self.split_summary_text(content, budget).await?;
            if fragments.len() < 2 {
                return Err("summary message cannot fit its model window".into());
            }
            let mut records = Vec::with_capacity(fragments.len());
            for (index, fragment) in fragments.into_iter().enumerate() {
                let mut record = template.clone();
                *history_text_mut(&mut record)? = fragment;
                if index > 0 {
                    if let Some(user) = &mut record.user_input_message {
                        user.documents.clear();
                    }
                }
                records.push(record);
            }
            for record in records.into_iter().rev() {
                pending.push_front(record);
            }
        }
        if !part.conversation_state.history.is_empty() {
            sanitize_kiro_tool_history(&mut part);
            parts.push(part);
        }
        if parts.is_empty() {
            return Err("summary instructions exceed its model window".into());
        }
        let count = parts.len();
        let output_budget = payload.max_output_tokens().unwrap_or(1024);
        for (index, part) in parts.iter_mut().enumerate() {
            if self.estimate_kiro_payload(part).await? > maximum {
                return Err("normalized summary part exceeds its model window".into());
            }
            part.conversation_state.conversation_id = format!(
                "{}-part-{}",
                payload.conversation_state.conversation_id,
                index + 1
            );
            if let Some(config) = &mut part.inference_config {
                config.max_tokens = Some((output_budget / count as u32).max(128));
            }
        }
        Ok(parts)
    }

    async fn split_summary_text(
        &self,
        text: String,
        maximum: usize,
    ) -> Result<Vec<String>, String> {
        let tokenizer = Arc::clone(&self.tokenizer);
        tokio::task::spawn_blocking(move || {
            let tokens = tokenizer.encode_ordinary(&text);
            let mut start = 0;
            let mut parts = Vec::new();
            while start < tokens.len() {
                if parts.len() >= MAX_SUMMARY_PARTS {
                    return Err("summary text exceeds the 16-part limit".into());
                }
                let mut end = (start + maximum).min(tokens.len());
                let fragment = loop {
                    if end == start {
                        return Err("summary token budget cannot hold one UTF-8 character".into());
                    }
                    if let Ok(fragment) = tokenizer.decode(tokens[start..end].to_vec()) {
                        if tokenizer.encode_ordinary(&fragment).len() <= maximum {
                            break fragment;
                        }
                    }
                    end -= 1;
                };
                parts.push(fragment);
                start = end;
            }
            Ok(parts)
        })
        .await
        .map_err(|error| error.to_string())?
    }
}

fn history_text_mut(message: &mut KiroHistoryMessage) -> Result<&mut String, String> {
    if let Some(user) = &mut message.user_input_message {
        return Ok(&mut user.content);
    }
    if let Some(assistant) = &mut message.assistant_response_message {
        return Ok(&mut assistant.content);
    }
    Err("summary history record has no text".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{claude_to_kiro, compaction_summary_payload, ClaudeRequest, TranslationOptions};

    #[tokio::test]
    async fn partitions_preserve_every_character_and_stay_within_the_window() {
        let cache = TokenCountCache::new(16).unwrap();
        let text = format!(
            "EARLY=cobalt-731\n{}MIDDLE=mango-482\n{}LATE=spruce-956",
            "雪人☃ e\u{301} 🦀 \"JSON\": [null, true] log\n".repeat(200),
            "事件日志 observation green\n".repeat(500)
        );
        let request: ClaudeRequest = serde_json::from_value(serde_json::json!({
            "model":"source", "max_tokens":64,
            "messages":[{"role":"user","content":text},
                {"role":"assistant","content":"SEED_READY"},
                {"role":"user","content":"Recall every exact identifier"}]
        }))
        .unwrap();
        let source = claude_to_kiro(&request, &TranslationOptions::new("target", "AI_EDITOR"));
        let plan = cache
            .plan_kiro_compaction(&source, 3000, 0)
            .await
            .unwrap()
            .unwrap();
        let summary = compaction_summary_payload(&source, &plan, "target");
        let expected = summary.conversation_state.history[0]
            .user_input_message
            .as_ref()
            .unwrap()
            .content
            .clone();
        let parts = cache
            .partition_compaction_summary(summary, 3000)
            .await
            .unwrap();
        assert!(parts.len() > 1);
        let mut restored = String::new();
        for part in &parts {
            assert!(cache.estimate_kiro_payload(part).await.unwrap() <= 3000);
            for message in &part.conversation_state.history {
                if let Some(user) = &message.user_input_message {
                    restored.push_str(&user.content);
                }
            }
        }
        assert_eq!(restored, expected);
        assert_eq!(
            source.conversation_state.history[source.protected_history_messages]
                .user_input_message
                .as_ref()
                .unwrap()
                .content,
            text
        );
    }

    #[tokio::test]
    async fn utf8_splits_round_trip_and_excess_parts_return_an_error() {
        let cache = TokenCountCache::new(16).unwrap();
        let input = "雪☃🦀e\u{301}\n".repeat(40);
        let parts = cache.split_summary_text(input.clone(), 64).await.unwrap();
        assert_eq!(parts.concat(), input);
        for part in parts {
            assert!(cache.count(part).await.unwrap() <= 64);
        }
        assert!(cache
            .split_summary_text("word ".repeat(2000), 32)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn long_tool_results_keep_middle_facts_and_documents_are_attached_once() {
        let cache = TokenCountCache::new(16).unwrap();
        let result = format!(
            "TOOL_EARLY={}\n{}\nTOOL_MIDDLE={}\n{}\nTOOL_LATE={}",
            "cobalt",
            "log 雪☃ ".repeat(500),
            "mango",
            "log 🦀 ".repeat(500),
            "spruce"
        );
        let request: ClaudeRequest = serde_json::from_value(serde_json::json!({
            "model":"source", "max_tokens":64,
            "tools":[{"name":"Read","input_schema":{"type":"object"}}],
            "messages":[{"role":"user","content":"Read the receipt"},
                {"role":"assistant","content":[{"type":"tool_use","id":"read-1","name":"Read","input":{"file_path":"receipt.txt"}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"read-1","content":result}]},
                {"role":"assistant","content":"READ_DONE"},
                {"role":"user","content":"Recall the tool result facts"}]
        })).unwrap();
        let mut source = claude_to_kiro(&request, &TranslationOptions::new("target", "AI_EDITOR"));
        source.conversation_state.history[source.protected_history_messages]
            .user_input_message
            .as_mut()
            .unwrap()
            .documents
            .push(
                serde_json::from_value(serde_json::json!({
                    "format":"pdf", "name":"receipt.pdf", "source":{"bytes":"aGVsbG8="}
                }))
                .unwrap(),
            );
        let plan = cache
            .plan_kiro_compaction(&source, 2200, 0)
            .await
            .unwrap()
            .unwrap();
        let payload = compaction_summary_payload(&source, &plan, "target");
        let parts = cache
            .partition_compaction_summary(payload, 2200)
            .await
            .unwrap();
        assert!(parts.len() > 1);
        let text = parts
            .iter()
            .map(|p| serde_json::to_string(p).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        for fact in [
            "TOOL_EARLY=cobalt",
            "TOOL_MIDDLE=mango",
            "TOOL_LATE=spruce",
            "read-1",
            "receipt.txt",
        ] {
            assert!(text.contains(fact), "summary input lost {fact}");
        }
        assert!(!text.contains("\"toolUses\":"));
        assert!(!text.contains("\"toolResults\":"));
        let mut documents = 0;
        for part in &parts {
            assert!(cache.estimate_kiro_payload(part).await.unwrap() <= 2200);
            for message in &part.conversation_state.history {
                if let Some(user) = &message.user_input_message {
                    documents += user.documents.len();
                }
            }
        }
        assert_eq!(documents, 1);
        assert!(cache
            .partition_compaction_summary(parts[0].clone(), 32)
            .await
            .is_err());
    }
}
