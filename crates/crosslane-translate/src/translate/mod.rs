//! Request translators.

mod auto_continue;
mod claude;
pub(crate) mod common;
mod openai;
mod tool_search;
mod web_search;

pub use auto_continue::{
    auto_continue_payload, resume_tool_search_payload, tool_search_continue_payload,
    tool_search_continue_payload_batch,
};
pub use claude::{
    claude_loaded_tools, claude_pending_server_tool_uses, claude_to_kiro, claude_tool_name_map,
};
pub use common::{tool_name, ToolNameRegistry};
pub use openai::openai_to_kiro;
pub use tool_search::{
    is_tool_search_tool, is_tool_search_type, tool_search_kiro_tool, ClaudeToolSearchBudget,
    ClaudeToolSearchCatalog, ClaudeToolSearchError, ClaudeToolSearchOutcome, ClaudeToolSearchTrace,
};
pub use web_search::{
    format_web_search_results, resume_web_search_payload, validate_web_search_replay_content,
    web_search_continue_payload, web_search_continue_payload_batch, ClaudeServerToolEmission,
    ClaudeWebSearchError, ClaudeWebSearchTrace, WebSearchReplayCodec, WebSearchReplayError,
};

/// Assistant half of the synthetic history pair that carries a caller's
/// system prompt. Protection is tracked separately in proxy-local metadata,
/// so ordinary conversations containing the same text are not misclassified.
pub(crate) const SYSTEM_PROMPT_ACKNOWLEDGEMENT: &str = "I will follow these instructions.";

#[derive(Debug, Clone)]
pub struct TranslationOptions {
    pub model_id: String,
    pub origin: String,
    pub profile_arn: Option<String>,
    pub enhance_system_prompt: bool,
    /// Retained for source compatibility. System prompts are now always
    /// represented by a protected history pair, independent of compaction.
    pub compact_mode: bool,
    /// Keep a recognized Claude Code summary instruction separate from the
    /// preceding user/tool-result blocks so that source context can compact.
    pub separate_compaction_instruction: bool,
    pub web_search_replay: Option<WebSearchReplayCodec>,
    /// Stable, caller-namespaced upstream conversation identifier.
    pub conversation_id: Option<String>,
    /// Emit native Kiro cachePoint blocks for validated Claude cache controls.
    pub enable_prompt_cache: bool,
    /// Model metadata used to select output_config/reasoning and valid effort
    /// levels. This is not a generic pass-through allowlist for client fields.
    pub additional_model_request_fields_schema: Option<serde_json::Value>,
}

impl TranslationOptions {
    pub fn new(model_id: impl Into<String>, origin: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
            origin: origin.into(),
            profile_arn: None,
            enhance_system_prompt: true,
            compact_mode: false,
            separate_compaction_instruction: false,
            web_search_replay: None,
            conversation_id: None,
            enable_prompt_cache: false,
            additional_model_request_fields_schema: None,
        }
    }
}
