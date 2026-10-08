//! Usage normalization shared by streaming and non-streaming responses.

use std::sync::Arc;

use crosslane_translate::KiroPayload;
use serde_json::json;

use crate::state::AppState;

use super::response::DecodedResponse;

/// Fill token fields only when Kiro did not report them. Input usage is
/// estimated from the exact payload for this round; output usage includes
/// visible text, reasoning, and structured tool calls.
pub async fn fill_missing_usage(
    state: &Arc<AppState>,
    decoded: &mut DecodedResponse,
    payload: &KiroPayload,
) {
    if decoded.usage.input_tokens == 0 {
        match state.tokenizer.estimate_kiro_payload(payload).await {
            Ok(tokens) => decoded.usage.input_tokens = tokens as u64,
            Err(error) => {
                tracing::warn!(%error, "failed to estimate missing upstream input usage")
            }
        }
    }
    if decoded.usage.reasoning_tokens == 0 && !decoded.reasoning.is_empty() {
        let fallback_tokens = (decoded.reasoning.chars().count() as u64 / 4).max(1);
        decoded.usage.reasoning_tokens =
            match state.tokenizer.count(decoded.reasoning.clone()).await {
                Ok(tokens) => tokens.max(1) as u64,
                Err(error) => {
                    tracing::warn!(%error, "failed to tokenize missing upstream reasoning usage");
                    fallback_tokens
                }
            };
    }
    if decoded.usage.output_tokens == 0 && produced_output(decoded) {
        let tools = decoded
            .tools
            .values()
            .map(|tool| {
                json!({
                    "id":tool.id,
                    "name":tool.name,
                    "input":super::response::repair_json(&tool.input),
                })
            })
            .collect::<Vec<_>>();
        let projection = json!({
            "text":decoded.text,
            "reasoning":decoded.reasoning,
            "tool_uses":tools,
        })
        .to_string();
        let fallback_tokens = (projection.chars().count() as u64 / 4).max(1);
        decoded.usage.output_tokens = match state.tokenizer.count(projection).await {
            Ok(tokens) => tokens.max(1) as u64,
            Err(error) => {
                tracing::warn!(%error, "failed to tokenize missing upstream output usage");
                fallback_tokens
            }
        };
    }
}

pub fn produced_output(decoded: &DecodedResponse) -> bool {
    !decoded.text.is_empty()
        || !decoded.reasoning.is_empty()
        || !decoded.tools.is_empty()
        || decoded.usage.output_tokens > 0
        || decoded.usage.credits > 0.0
}

/// Estimates credits from normalized token totals when the upstream omits its
/// own credit field. Unlike reservation estimates, settlement does not cap
/// actual output tokens.
pub fn fallback_credits(state: &Arc<AppState>, model: &str, input: u64, output: u64) -> f64 {
    let config = state.config.current();
    let multiplier = state
        .resolved_model_info(model)
        .and_then(|model| model.rate_multiplier)
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(1.0);
    (input.saturating_add(output)) as f64 / 1_000.0
        * config.pool.credit_estimate_per_1k_tokens
        * multiplier
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn output_fallback_counts_structured_tool_arguments() {
        let directory = tempfile::tempdir().expect("tempdir");
        let paths = crosslane_core::paths::Paths::from_env_values(
            Some(directory.path().to_str().expect("utf8")),
            None,
            None,
            None,
        );
        crosslane_store::bootstrap::ensure_layout(&paths)
            .await
            .expect("layout");
        let accounts = crosslane_store::accounts::AccountStore::load(&paths.accounts_file)
            .await
            .expect("accounts");
        let state = Arc::new(AppState::new(
            paths,
            crosslane_store::config_loader::ConfigHandle::new(
                crosslane_core::config::Config::default(),
            ),
            accounts,
        ));
        let payload: KiroPayload = serde_json::from_value(json!({
            "conversationState":{"chatTriggerType":"MANUAL","conversationId":"c","history":[],
                "currentMessage":{"userInputMessage":{"content":"hello","modelId":"model","origin":"CLI","images":[]}}}
        }))
        .expect("payload");
        let mut decoded = DecodedResponse {
            reasoning: "reasoning trace ".repeat(100),
            ..DecodedResponse::default()
        };
        decoded.tools.insert(
            "tool-1".into(),
            super::super::response::ToolBuffer {
                id: "tool-1".into(),
                name: "write_file".into(),
                input: json!({"content":"large argument ".repeat(1000)}).to_string(),
                complete: true,
            },
        );

        fill_missing_usage(&state, &mut decoded, &payload).await;
        assert!(decoded.usage.input_tokens > 0);
        assert!(decoded.usage.output_tokens > 100);
        assert!(decoded.usage.reasoning_tokens > 0);
    }

    #[tokio::test]
    async fn credit_fallback_honors_config_and_model_multiplier() {
        let directory = tempfile::tempdir().expect("tempdir");
        let paths = crosslane_core::paths::Paths::from_env_values(
            Some(directory.path().to_str().expect("utf8")),
            None,
            None,
            None,
        );
        crosslane_store::bootstrap::ensure_layout(&paths)
            .await
            .expect("layout");
        let accounts = crosslane_store::accounts::AccountStore::load(&paths.accounts_file)
            .await
            .expect("accounts");
        let mut config = crosslane_core::config::Config::default();
        config.pool.credit_estimate_per_1k_tokens = 2.0;
        let state = Arc::new(AppState::new(
            paths,
            crosslane_store::config_loader::ConfigHandle::new(config),
            accounts,
        ));
        state.models.finish_refresh(vec![crosslane_kiro::ModelInfo {
            model_id: "claude-sonnet-4.6".into(),
            model_name: String::new(),
            description: String::new(),
            rate_multiplier: Some(1.5),
            token_limits: None,
            additional_model_request_fields_schema: None,
        }]);

        assert_eq!(fallback_credits(&state, "claude-4.6-sonnet", 800, 200), 3.0);
    }
}
