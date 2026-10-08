//! Wire models. Optional and unknown fields intentionally remain forward compatible.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Matches an unversioned protocol type or any versioned/aliased member of
/// that type family without assuming a date-shaped suffix.
pub fn matches_type_family(kind: &str, base: &str) -> bool {
    kind == base
        || kind
            .strip_prefix(base)
            .is_some_and(|suffix| suffix.starts_with('_'))
}

/// Anthropic Messages request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeRequest {
    pub model: String,
    pub messages: Vec<ClaudeMessage>,
    pub max_tokens: u32,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub top_k: Option<u32>,
    #[serde(default)]
    pub stop_sequences: Vec<String>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub system: Option<Value>,
    #[serde(default)]
    pub tools: Vec<ClaudeTool>,
    #[serde(default)]
    pub tool_choice: Option<ClaudeToolChoice>,
    #[serde(default)]
    pub thinking: Option<ThinkingConfig>,
    #[serde(default)]
    pub output_config: Option<ClaudeOutputConfig>,
    /// Anthropic automatic prompt-caching control. Block-level controls remain
    /// embedded in the untyped content values below.
    #[serde(default)]
    pub cache_control: Option<Value>,
    #[serde(default)]
    pub service_tier: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
    /// Optional Kiro-compatible extension used by clients that already own a
    /// stable conversation identifier. The daemon hashes it with the
    /// authenticated client namespace before putting it on the upstream wire.
    #[serde(default, alias = "conversationId")]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub context_management: Option<Value>,
    /// Additive client controls are accepted for reference-gateway compatibility
    /// and are not automatically forwarded to Kiro.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeMessage {
    pub role: String,
    pub content: Value,
    #[serde(default)]
    pub cache_control: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_config: Option<ClaudeOutputConfig>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeTool {
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "object_schema")]
    pub input_schema: Value,
    #[serde(default)]
    pub cache_control: Option<Value>,
    #[serde(default)]
    pub strict: Option<bool>,
    #[serde(default)]
    pub input_examples: Option<Vec<Value>>,
    /// Anthropic Tool Search hint. Deferred definitions stay in the request
    /// catalog but are not loaded into the model context until discovered.
    #[serde(default)]
    pub defer_loading: bool,
    /// Anthropic programmatic-tool-calling policy. Only direct calls can be
    /// represented by the Kiro upstream today; validation rejects the other
    /// values instead of silently weakening the contract.
    #[serde(default)]
    pub allowed_callers: Option<Vec<String>>,
    /// Accepted as a compatibility hint; Kiro's stream behavior is unchanged.
    #[serde(default)]
    pub eager_input_streaming: Option<bool>,
    /// Server-side web search controls.
    #[serde(default)]
    pub max_uses: Option<u32>,
    #[serde(default)]
    pub allowed_domains: Option<Vec<String>>,
    #[serde(default)]
    pub blocked_domains: Option<Vec<String>>,
    #[serde(default)]
    pub user_location: Option<Value>,
    #[serde(default)]
    pub response_inclusion: Option<String>,
    /// Extra tool hints are accepted but do not enter the Kiro tool definition.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeToolChoice {
    pub r#type: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub disable_parallel_tool_use: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingConfig {
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub budget_tokens: Option<u32>,
    #[serde(default)]
    pub display: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeOutputConfig {
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// OpenAI Chat Completions request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiRequest {
    pub model: String,
    pub messages: Vec<OpenAiMessage>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub max_completion_tokens: Option<u32>,
    #[serde(default, deserialize_with = "null_default")]
    pub stream: bool,
    #[serde(default)]
    pub stop: Option<Value>,
    #[serde(default)]
    pub n: Option<u32>,
    /// Restore the deprecated function_call response shape for legacy clients.
    #[serde(skip)]
    pub legacy_functions: bool,
    #[serde(default)]
    pub stream_options: Option<Value>,
    #[serde(default, deserialize_with = "null_default")]
    pub tools: Vec<OpenAiTool>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default = "default_parallel")]
    pub parallel_tool_calls: bool,
    #[serde(default)]
    pub thinking: Option<ThinkingConfig>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
    #[serde(default)]
    pub response_format: Option<Value>,
}

impl OpenAiRequest {
    /// Prefer the current API's limit, retaining max_tokens as a legacy alias.
    pub fn output_token_limit(&self) -> Option<u32> {
        self.max_completion_tokens.or(self.max_tokens)
    }

    /// Validation checks the shape and resource limits before execution.
    pub fn stop_sequences(&self) -> Vec<String> {
        match self.stop.as_ref() {
            Some(Value::String(value)) => vec![value.clone()],
            Some(Value::Array(values)) => values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        }
    }
}

pub(crate) fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiMessage {
    pub role: String,
    #[serde(default)]
    pub content: Option<Value>,
    #[serde(default, deserialize_with = "null_default")]
    pub tool_calls: Vec<Value>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub refusal: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub cache_control: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiTool {
    pub r#type: String,
    #[serde(flatten)]
    pub body: Value,
}

/// Original client controls, retained across account routing, model fallbacks,
/// and internal continuation turns. Never part of Kiro's wire protocol.
#[derive(Debug, Clone, Default)]
pub struct ModelRequestIntent {
    pub requested_model: String,
    pub thinking: Option<ThinkingConfig>,
    /// Explicit OpenAI reasoning_effort or the effective Claude output_config.effort.
    pub effort: Option<String>,
    /// Responses opt-in, retained across model retries and continuations.
    /// This intent is proxy-local and never serialized onto the Kiro wire.
    pub automatic_history_truncation: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroPayload {
    pub conversation_state: KiroConversationState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_arn: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inference_config: Option<KiroInferenceConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_model_request_fields: Option<Value>,
    #[doc(hidden)]
    #[serde(skip)]
    pub model_request_intent: Option<ModelRequestIntent>,
    /// Proxy-local metadata. This is deliberately excluded from the upstream
    /// wire payload so protected history cannot be forged through JSON or
    /// interpreted as part of the prompt by Kiro.
    #[doc(hidden)]
    #[serde(skip)]
    pub protected_history_messages: usize,
}

impl KiroPayload {
    pub fn max_output_tokens(&self) -> Option<u32> {
        self.inference_config
            .as_ref()
            .and_then(|inference| inference.max_tokens)
    }

    pub fn thinking_summary_omitted(&self) -> bool {
        self.model_request_intent
            .as_ref()
            .and_then(|intent| intent.thinking.as_ref())
            .and_then(|thinking| thinking.display.as_deref())
            == Some("omitted")
    }

    /// Whether this particular upstream attempt requested visible reasoning.
    /// Omission is not a guarantee that the upstream model itself cannot think.
    pub fn thinking_enabled(&self) -> bool {
        let Some(fields) = self.additional_model_request_fields.as_ref() else {
            return false;
        };
        matches!(
            fields.pointer("/thinking/type").and_then(Value::as_str),
            Some("adaptive" | "enabled")
        ) || fields
            .pointer("/reasoning/effort")
            .and_then(Value::as_str)
            .is_some_and(|effort| effort != "none")
    }

    pub fn protected_history_len(&self) -> usize {
        self.protected_history_messages
            .min(self.conversation_state.history.len())
    }

    /// Removes compactable history while retaining the proxy-owned prefix.
    pub fn retain_protected_history(&mut self) {
        let protected = self.protected_history_len();
        self.conversation_state.history.truncate(protected);
    }

    /// Applies an internal history bound without discarding the proxy-owned
    /// prefix. Valid Kiro histories and the protected prefix both contain
    /// complete user/assistant pairs, so retaining an even recent capacity
    /// also preserves turn boundaries.
    pub(crate) fn truncate_history_preserving_protected_prefix(&mut self, maximum: usize) {
        let history_len = self.conversation_state.history.len();
        if history_len <= maximum {
            return;
        }
        let protected = self.protected_history_len();
        let recent_capacity = maximum.saturating_sub(protected);
        let recent_start = history_len.saturating_sub(recent_capacity).max(protected);
        let recent = self.conversation_state.history.split_off(recent_start);
        self.conversation_state.history.truncate(protected);
        self.conversation_state.history.extend(recent);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroConversationState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_continuation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_task_type: Option<String>,
    pub chat_trigger_type: String,
    pub conversation_id: String,
    pub current_message: KiroCurrentMessage,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub history: Vec<KiroHistoryMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroCurrentMessage {
    pub user_input_message: KiroUserInputMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroUserInputMessage {
    pub content: String,
    pub model_id: String,
    pub origin: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub images: Vec<KiroImage>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub documents: Vec<KiroDocument>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_point: Option<KiroCachePoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_cache_config: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_input_message_context: Option<KiroMessageContext>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroImage {
    pub format: String,
    pub source: KiroImageSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroImageSource {
    pub bytes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroDocument {
    pub format: String,
    pub name: String,
    pub source: KiroDocumentSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub citations: Option<KiroCitationsConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroDocumentSource {
    pub bytes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroCitationsConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KiroCachePoint {
    pub r#type: String,
}

impl KiroCachePoint {
    pub fn new() -> Self {
        Self {
            r#type: "default".into(),
        }
    }
}

impl Default for KiroCachePoint {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct KiroMessageContext {
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tool_results: Vec<KiroToolResult>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tools: Vec<KiroTool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub env_state: Option<KiroEnvState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct KiroEnvState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_working_directory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operating_system: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroToolResult {
    pub content: Vec<KiroText>,
    pub status: String,
    pub tool_use_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroText {
    pub text: String,
}

/// Normalized Kiro MCP web search payload (the JSON string nested inside the
/// MCP `result.content[].text` block).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchResults {
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub total_results: u64,
    #[serde(default)]
    pub results: Vec<WebSearchResult>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchResult {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub snippet: String,
    #[serde(default)]
    pub published_date: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", untagged)]
pub enum KiroTool {
    Specification {
        #[serde(rename = "toolSpecification")]
        tool_specification: KiroToolSpecification,
    },
    CachePoint {
        #[serde(rename = "cachePoint")]
        cache_point: KiroCachePoint,
    },
}

impl KiroTool {
    pub fn specification(&self) -> Option<&KiroToolSpecification> {
        match self {
            Self::Specification { tool_specification } => Some(tool_specification),
            Self::CachePoint { .. } => None,
        }
    }

    pub fn specification_mut(&mut self) -> Option<&mut KiroToolSpecification> {
        match self {
            Self::Specification { tool_specification } => Some(tool_specification),
            Self::CachePoint { .. } => None,
        }
    }

    pub fn cache_point(&self) -> Option<&KiroCachePoint> {
        match self {
            Self::CachePoint { cache_point } => Some(cache_point),
            Self::Specification { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroToolSpecification {
    pub name: String,
    pub description: String,
    pub input_schema: KiroInputSchema,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroInputSchema {
    pub json: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroHistoryMessage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_input_message: Option<KiroUserInputMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_response_message: Option<KiroAssistantMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroAssistantMessage {
    /// Preserve the actual assistant text, including an empty string for a
    /// tool-only turn. Natural-language filler here pollutes replayed history.
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_point: Option<KiroCachePoint>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tool_uses: Vec<KiroToolUse>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroToolUse {
    pub tool_use_id: String,
    pub name: String,
    pub input: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroInferenceConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
}

fn object_schema() -> Value {
    serde_json::json!({"type":"object","properties":{}})
}

fn default_parallel() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        matches_type_family, KiroCachePoint, KiroInputSchema, KiroTool, KiroToolSpecification,
    };

    #[test]
    fn protocol_type_families_do_not_assume_a_version_format() {
        assert!(matches_type_family("web_search", "web_search"));
        assert!(matches_type_family("web_search_20260318", "web_search"));
        assert!(matches_type_family("web_search_next", "web_search"));
        assert!(matches_type_family("web_search_", "web_search"));
        assert!(!matches_type_family("web_searcher", "web_search"));
        assert!(!matches_type_family("other_web_search_next", "web_search"));
    }

    #[test]
    fn kiro_tool_variants_match_the_wire_shape() {
        let specification = KiroTool::Specification {
            tool_specification: KiroToolSpecification {
                name: "lookup".into(),
                description: "Lookup a value".into(),
                input_schema: KiroInputSchema {
                    json: json!({"type":"object"}),
                },
            },
        };
        assert_eq!(
            serde_json::to_value(&specification).expect("serialize specification"),
            json!({"toolSpecification": {
                "name": "lookup",
                "description": "Lookup a value",
                "inputSchema": {"json": {"type":"object"}}
            }})
        );

        let cache_point = KiroTool::CachePoint {
            cache_point: KiroCachePoint::new(),
        };
        let value = serde_json::to_value(&cache_point).expect("serialize cache point");
        assert_eq!(value, json!({"cachePoint":{"type":"default"}}));
        assert!(serde_json::from_value::<KiroTool>(value).is_ok());

        let extended: KiroTool = serde_json::from_value(json!({
            "cachePoint":{"type":"default","ttl":"1h"}
        }))
        .expect("read an older payload");
        assert_eq!(
            serde_json::to_value(&extended).expect("serialize only supported cache fields"),
            json!({"cachePoint":{"type":"default"}})
        );
    }
}
