use serde::{Deserialize, Serialize};

use super::{
    collect_assistant_message, collect_tools, collect_user_message, KiroHistoryMessage,
    KiroPayload, TokenCountCache, CONVERSATION_OVERHEAD_TOKENS,
};

/// Disjoint estimates of the translated request. The protected prefix includes
/// both the system prompt and long tool documentation moved there by translation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextTokenBreakdown {
    pub total_input_tokens: u64,
    pub protected_prefix_tokens: u64,
    pub tool_definition_tokens: u64,
    /// Current message content, attachments and results, excluding definitions.
    pub current_message_tokens: u64,
    pub history_tokens: u64,
    pub overhead_tokens: u64,
    /// Input left after removing all compactable history.
    pub minimum_input_tokens: u64,
}

impl TokenCountCache {
    /// Computed on rejection only, using the same tokenizer and framing as the
    /// context guard. This never returns source text, tool names or schemas.
    pub async fn context_token_breakdown(
        &self,
        payload: &KiroPayload,
    ) -> Result<ContextTokenBreakdown, String> {
        let (prefix, history) = payload
            .conversation_state
            .history
            .split_at(payload.protected_history_len());
        let protected_prefix_tokens = self.history_tokens(prefix).await?;
        let history_tokens = self.history_tokens(history).await?;
        let current = &payload
            .conversation_state
            .current_message
            .user_input_message;
        let mut segments = Vec::new();
        let mut fixed = 0;
        collect_user_message(current, &mut segments, &mut fixed);
        let current_tokens = self.section_tokens(segments, fixed).await?;
        let mut segments = Vec::new();
        let mut fixed = 0;
        collect_tools(current, &mut segments, &mut fixed);
        let tool_definition_tokens = self.section_tokens(segments, fixed).await?;
        let current_message_tokens = current_tokens.saturating_sub(tool_definition_tokens);
        let overhead_tokens = CONVERSATION_OVERHEAD_TOKENS as u64;
        let minimum_input_tokens = protected_prefix_tokens
            .saturating_add(current_tokens)
            .saturating_add(overhead_tokens);
        Ok(ContextTokenBreakdown {
            total_input_tokens: minimum_input_tokens.saturating_add(history_tokens),
            protected_prefix_tokens,
            tool_definition_tokens,
            current_message_tokens,
            history_tokens,
            overhead_tokens,
            minimum_input_tokens,
        })
    }

    async fn history_tokens(&self, history: &[KiroHistoryMessage]) -> Result<u64, String> {
        let mut segments = Vec::new();
        let mut fixed = 0;
        for message in history {
            if let Some(user) = &message.user_input_message {
                collect_user_message(user, &mut segments, &mut fixed);
            }
            if let Some(assistant) = &message.assistant_response_message {
                collect_assistant_message(assistant, &mut segments, &mut fixed);
            }
        }
        self.section_tokens(segments, fixed).await
    }

    async fn section_tokens(&self, segments: Vec<String>, fixed: usize) -> Result<u64, String> {
        let mut total = fixed as u64;
        for segment in segments {
            total = total.saturating_add(self.count(segment).await? as u64);
        }
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{claude_to_kiro, sanitize_kiro_tool_history, ClaudeRequest, TranslationOptions};

    #[tokio::test]
    async fn context_components_match_full_and_minimum_payload_estimates() {
        let request: ClaudeRequest = serde_json::from_value(serde_json::json!({
            "model":"source", "max_tokens":64,
            "system":"Keep rules intact. 雪☃",
            "tools":[{"name":"Read", "description":"Tool documentation. ".repeat(4000),
                "input_schema":{"type":"object","properties":{"path":{"type":"string"}},"additionalProperties":false}}],
            "messages":[
                {"role":"user","content":"Read the old records"},
                {"role":"assistant","content":[{"type":"tool_use","id":"read-1","name":"Read","input":{"path":"old.txt"}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"read-1","content":"current result\n雪☃"}]}
            ]
        })).unwrap();
        let mut options = TranslationOptions::new("target", "AI_EDITOR");
        options.enhance_system_prompt = false;
        let mut payload = claude_to_kiro(&request, &options);
        sanitize_kiro_tool_history(&mut payload);
        let tokenizer = TokenCountCache::new(16).unwrap();
        let breakdown = tokenizer.context_token_breakdown(&payload).await.unwrap();
        assert_eq!(
            breakdown.total_input_tokens,
            tokenizer.estimate_kiro_payload(&payload).await.unwrap() as u64
        );
        assert_eq!(
            breakdown.total_input_tokens,
            breakdown.protected_prefix_tokens
                + breakdown.tool_definition_tokens
                + breakdown.current_message_tokens
                + breakdown.history_tokens
                + breakdown.overhead_tokens
        );
        assert!(
            breakdown.protected_prefix_tokens > 4000,
            "relocated tool documentation belongs to the protected prefix"
        );
        assert!(
            breakdown.tool_definition_tokens > 0
                && breakdown.current_message_tokens > 0
                && breakdown.history_tokens > 0
        );
        payload.retain_protected_history();
        assert_eq!(
            breakdown.minimum_input_tokens,
            tokenizer.estimate_kiro_payload(&payload).await.unwrap() as u64
        );
        let encoded = serde_json::to_string(&breakdown).unwrap();
        assert!(!encoded.contains("old.txt") && !encoded.contains("Keep rules"));
    }
}
