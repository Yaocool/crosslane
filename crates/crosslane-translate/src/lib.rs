//! Claude/OpenAI 与 Kiro 协议之间的纯转换层。

pub mod bridge;
pub mod compatibility;
pub mod context;
pub mod error;
pub mod model;
pub mod protocol;
pub mod responses;
pub mod tokenizer;
pub mod tool_history;
pub mod tool_schema;
pub mod translate;
pub mod validate;

pub use compatibility::parse_openai_request;
pub use context::{
    apply_compaction_boundary, apply_context_management_edits, compact_trigger_tokens,
    estimate_context_management_input_tokens, has_context_management_edits, is_compact_edit_type,
    normalize_compaction_boundary, prepare_claude_code_compaction,
    ClaudeCompactionNormalizationStats, ClaudeContextEditStats, CLEARED_TOOL_RESULT_TEXT,
    DEFAULT_COMPACT_TRIGGER_TOKENS, DEFAULT_TOOL_CLEAR_TRIGGER_TOKENS, DEFAULT_TOOL_USES_TO_KEEP,
    MIN_COMPACT_TRIGGER_TOKENS,
};
pub use error::{error_envelope, sanitize_error_message, ErrorFormat};
pub use protocol::*;
pub use responses::{
    responses_to_openai, ResponsesRequest, ResponsesToolName, ResponsesTranslation,
};
pub use tokenizer::{
    compaction_summary_payload, prepare_compaction_source_history, ContextCompactionStats,
    ContextTokenBreakdown, KiroCompactionPlan, TokenCountCache, TokenCountStats,
};
pub use tool_history::{
    sanitize_kiro_tool_history, validate_kiro_tool_history, KiroToolHistoryStats,
};
pub use translate::{
    auto_continue_payload, claude_loaded_tools, claude_pending_server_tool_uses, claude_to_kiro,
    claude_tool_name_map, format_web_search_results, is_tool_search_tool, is_tool_search_type,
    openai_to_kiro, resume_tool_search_payload, resume_web_search_payload, tool_name,
    tool_search_continue_payload, tool_search_continue_payload_batch, tool_search_kiro_tool,
    validate_web_search_replay_content, web_search_continue_payload,
    web_search_continue_payload_batch, ClaudeServerToolEmission, ClaudeToolSearchBudget,
    ClaudeToolSearchCatalog, ClaudeToolSearchError, ClaudeToolSearchOutcome, ClaudeToolSearchTrace,
    ClaudeWebSearchError, ClaudeWebSearchTrace, ToolNameRegistry, TranslationOptions,
    WebSearchReplayCodec, WebSearchReplayError,
};
pub use validate::document_bytes_match_format;
mod web_search_domains;
pub use validate::{
    validate_claude, validate_claude_count, validate_claude_generation, validate_openai,
    ValidationError,
};
pub use web_search_domains::WebSearchDomainFilter;
