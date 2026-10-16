//! 配置模型、校验规则与带注释的默认配置。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::account::Account;
use crate::provider::ProviderId;

/// Maximum number of immediately loaded tools accepted by the proxy.
pub const MAX_LOADED_TOOLS: usize = 512;

/// Public OAuth client ID used by the built-in GitHub Copilot Device Flow.
pub const DEFAULT_COPILOT_OAUTH_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

/// 配置校验错误。
#[derive(Debug, Error)]
pub enum ConfigError {
    /// 端口落入特权端口范围。
    #[error("server.port must be between 1024 and 65535, got {0}")]
    InvalidPort(u16),
    /// 对公网监听但没有启用的 API key。
    #[error("binding to a non-local host requires at least one enabled api key")]
    PublicBindWithoutApiKey,
    /// 比例字段不在闭区间 0..=1。
    #[error("{field} must be between 0.0 and 1.0, got {value}")]
    RatioOutOfRange {
        /// 字段名。
        field: &'static str,
        /// 非法值。
        value: f64,
    },
    #[error("invalid configuration value for {field}: {message}")]
    InvalidValue { field: String, message: String },
}

/// API key 格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ApiKeyFormat {
    /// `sk-` 风格。
    #[default]
    Sk,
    /// 简单字符串。
    Simple,
    /// token 风格。
    Token,
}

/// 上游端点偏好。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Endpoint {
    /// CodeWhisperer。
    Codewhisperer,
    /// Amazon Q。
    Amazonq,
    /// Regional Kiro CLI runtime.
    Runtime,
}

/// Kiro agent mode。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    /// 自动选择。
    Auto,
    /// Vibe mode。
    Vibe,
    /// Spec mode。
    Spec,
}

/// 思考内容输出格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingOutputFormat {
    /// Claude thinking block。
    Claude,
    /// OpenAI reasoning_content。
    Openai,
}

fn default_true() -> bool {
    true
}

/// TLS 配置。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TlsConfig {
    /// 是否启用 HTTPS。
    pub enabled: bool,
    /// 证书文件路径。
    pub cert_path: Option<String>,
    /// 私钥文件路径。
    pub key_path: Option<String>,
    /// 证书 PEM 内容。
    pub cert: Option<String>,
    /// 私钥 PEM 内容。
    pub key: Option<String>,
}

/// 基于代理内部排队与上游过载反馈的自适应并发。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AdaptiveConfig {
    /// 是否启用。
    pub enabled: bool,
    /// 检查间隔。
    pub check_interval_ms: u64,
    /// 上游流式连接槽位等待 P99 超过此值时降低并发。
    pub p99_degrade_ms: u64,
    /// 上游流式连接槽位等待 P99 低于此值时恢复并发。
    pub p99_recover_ms: u64,
    /// 每次决策所需的最少上游调用样本数。
    pub minimum_samples: usize,
    /// 上游 429/503 占比达到此值时降低并发。
    pub overload_error_rate: f64,
}

impl Default for AdaptiveConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            check_interval_ms: 10_000,
            p99_degrade_ms: 200,
            p99_recover_ms: 100,
            minimum_samples: 5,
            overload_error_rate: 0.05,
        }
    }
}

/// API 代理服务的共享参数和新建服务默认值。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// 新建代理服务的默认监听主机。
    pub host: String,
    /// 新建代理服务的默认监听端口。
    pub port: u16,
    /// 按协议校验客户端 User-Agent：Claude 仅限 Claude Code，OpenAI 仅限 Codex。
    pub enforce_user_agent_check: bool,
    /// 全局准入上限。
    pub max_concurrent_requests: usize,
    /// HTTP keepalive 超时。
    pub keep_alive_timeout_ms: u64,
    /// 最大连接数。
    pub max_connections: usize,
    /// TLS 配置。
    pub tls: TlsConfig,
    /// 自适应并发配置。
    pub adaptive: AdaptiveConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 5580,
            enforce_user_agent_check: true,
            max_concurrent_requests: 500,
            keep_alive_timeout_ms: 30_000,
            max_connections: 2_000,
            tls: TlsConfig::default(),
            adaptive: AdaptiveConfig::default(),
        }
    }
}

/// 双连接池配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UpstreamPoolConfig {
    /// 非流式连接数。
    pub http_max_connections: usize,
    /// 非流式 HTTP/1.1 pipelining 数。
    pub http_pipelining: usize,
    /// 流式连接数。
    pub stream_max_connections: usize,
    /// 流式连接必须为 1，避免队首阻塞。
    pub stream_pipelining: usize,
    /// 连接池目标空闲回收时间。
    pub keep_alive_idle_ms: u64,
    /// 连接池空闲回收时间上限；不限制正在使用连接的总寿命。
    pub keep_alive_max_ms: u64,
}

impl Default for UpstreamPoolConfig {
    fn default() -> Self {
        Self {
            http_max_connections: 128,
            http_pipelining: 5,
            stream_max_connections: 256,
            stream_pipelining: 1,
            keep_alive_idle_ms: 30_000,
            keep_alive_max_ms: 60_000,
        }
    }
}

/// 上游请求配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UpstreamConfig {
    /// 首选上游端点；空值按账号类型推断。
    pub preferred_endpoint: Option<Endpoint>,
    /// Kiro agent mode。
    pub agent_mode: AgentMode,
    /// 最大重试次数。
    pub max_retries: u32,
    /// 期望提前刷新 token 的秒数。实际值还会受定时扫描间隔的安全下限约束。
    pub token_refresh_before_expiry: i64,
    /// 双连接池参数。
    pub pool: UpstreamPoolConfig,
    /// Kiro MCP Web Search 端点。支持 `{region}` 占位符；空值使用
    /// `https://runtime.{region}.kiro.dev/`，通过 InvokeMCP RPC 调用。
    pub web_search_endpoint: Option<String>,
    /// MCP Web Search 请求超时。
    pub web_search_timeout_ms: u64,
    /// 等待流式连接槽位的最长时间。
    pub stream_slot_wait_timeout_ms: u64,
    /// 流式上游连续无响应数据的最长时间；不是请求总时长。
    pub stream_read_timeout_ms: u64,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            preferred_endpoint: None,
            agent_mode: AgentMode::Vibe,
            max_retries: 3,
            token_refresh_before_expiry: 900,
            pool: UpstreamPoolConfig::default(),
            web_search_endpoint: None,
            web_search_timeout_ms: 60_000,
            stream_slot_wait_timeout_ms: 30_000,
            stream_read_timeout_ms: 600_000,
        }
    }
}

/// 三因子选号权重。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BalanceConfig {
    /// 并发压力权重。
    pub weight_active: f64,
    /// 额度消耗权重。
    pub weight_credit: f64,
    /// 最近使用权重。
    pub weight_idle: f64,
    /// 空闲归一化窗口。
    pub idle_window_ms: u64,
}

impl Default for BalanceConfig {
    fn default() -> Self {
        Self {
            weight_active: 0.5,
            weight_credit: 0.4,
            weight_idle: 0.1,
            idle_window_ms: 300_000,
        }
    }
}

/// 错误冷却与配额恢复。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CooldownConfig {
    /// 连续错误阈值。
    pub max_error_count: u32,
    /// 一般冷却时长。
    pub cooldown_ms: u64,
    /// 单次错误短冷却。
    pub error_cooldown_ms: u64,
    /// 额度耗尽后重新探测间隔。
    pub quota_reset_ms: u64,
    /// 额度错误滑动窗口。
    pub quota_error_window_ms: u64,
    /// 额度错误阈值。
    pub quota_error_threshold: u32,
}

impl Default for CooldownConfig {
    fn default() -> Self {
        Self {
            max_error_count: 3,
            cooldown_ms: 30_000,
            error_cooldown_ms: 5_000,
            quota_reset_ms: 300_000,
            quota_error_window_ms: 300_000,
            quota_error_threshold: 50,
        }
    }
}

/// 账号池配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PoolConfig {
    /// 单账号并发上限。
    pub max_concurrent_per_account: usize,
    /// 排队容量。
    pub max_queue_size: usize,
    /// 最大排队时间。
    pub max_queue_wait_ms: u64,
    /// 队列满时等待时间。
    pub queue_full_wait_ms: u64,
    /// 低额度绝对值阈值。
    pub low_credit_min_remaining: f64,
    /// 允许 Kiro 超额使用；开启时不按本地额度余额暂停账号。
    pub enable_overage: bool,
    /// 每个账号最多使用的 Kiro 超额 credits；None 表示仅遵循上游上限。
    pub max_overage_credits_per_account: Option<f64>,
    /// 每日额度上限；0 表示不限。
    pub daily_credit_limit: f64,
    /// 每 1,000 个估算 token 预留的 credits。
    pub credit_estimate_per_1k_tokens: f64,
    /// 预留估算中计入的最大输出 token 数。
    pub credit_estimate_output_token_cap: u32,
    /// 额度耗尽时是否自动切号。
    pub auto_switch_on_quota_exhausted: bool,
    /// 选号权重。
    pub balance: BalanceConfig,
    /// 错误冷却。
    pub cooldown: CooldownConfig,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_concurrent_per_account: 50,
            max_queue_size: 10,
            max_queue_wait_ms: 30_000,
            queue_full_wait_ms: 5_000,
            low_credit_min_remaining: 4.0,
            enable_overage: false,
            max_overage_credits_per_account: None,
            daily_credit_limit: 0.0,
            credit_estimate_per_1k_tokens: 1.0,
            credit_estimate_output_token_cap: 8_192,
            auto_switch_on_quota_exhausted: true,
            balance: BalanceConfig::default(),
            cooldown: CooldownConfig::default(),
        }
    }
}

/// 功能开关。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FeaturesConfig {
    /// 工具调用后自动续轮次数。
    pub auto_continue_rounds: u32,
    /// 代理内部 Tool Search 的最大续轮次数（硬上限 8）。
    pub tool_search_max_rounds: u32,
    /// 单个客户端请求内允许实际执行的 Tool Search 操作数。
    pub tool_search_max_operations: u32,
    /// 是否启用原生 Anthropic Tool Search 模拟。
    pub enable_tool_search: bool,
    /// 单个客户端请求内代理执行 MCP Web Search 的安全上限。
    pub web_search_max_rounds: u32,
    /// 是否移除工具。
    pub disable_tools: bool,
    /// 上游限流或模型暂时不可用时是否启用同族模型降级。
    pub enable_model_fallback: bool,
    /// 是否模拟 prompt cache。
    pub enable_prompt_cache: bool,
    /// 是否注入增强 system prompt。
    pub enhance_system_prompt: bool,
    /// 是否缓冲工具调用。
    pub buffer_tool_calls: bool,
    /// 旧配置兼容字段；工具参数校验完成后立即发送，不再额外休眠。
    pub tool_call_buffer_delay_ms: u64,
    /// thinking 输出格式。
    pub thinking_output_format: ThinkingOutputFormat,
    /// 旧配置兼容字段；现在逐轮按客户端请求转换 thinking，不再启用轮次启停启发式。
    pub adaptive_thinking: bool,
    /// 旧配置兼容字段。原生 adaptive/effort 模式不提供独立的 thinking token 硬上限。
    pub max_thinking_budget_tokens: u32,
    /// 是否转换 web 工具。
    pub enable_web_tools: bool,
    /// 是否过滤 tool leak。
    pub enable_tool_leak_filter: bool,
    /// 空字符串表示自动选择。
    pub default_model_id: String,
}

impl Default for FeaturesConfig {
    fn default() -> Self {
        Self {
            auto_continue_rounds: 0,
            tool_search_max_rounds: 4,
            tool_search_max_operations: 32,
            enable_tool_search: true,
            web_search_max_rounds: 20,
            disable_tools: false,
            enable_model_fallback: true,
            enable_prompt_cache: false,
            enhance_system_prompt: true,
            buffer_tool_calls: true,
            tool_call_buffer_delay_ms: 0,
            thinking_output_format: ThinkingOutputFormat::Claude,
            adaptive_thinking: false,
            max_thinking_budget_tokens: 8192,
            enable_web_tools: true,
            enable_tool_leak_filter: true,
            default_model_id: String::new(),
        }
    }
}

/// 动态模型发现。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelsConfig {
    /// Globally disabled model IDs, across every provider and internal caller.
    pub disabled: Vec<String>,
    /// 是否从上游动态发现模型。
    pub dynamic_discovery: bool,
    /// 模型缓存有效期。
    pub cache_ttl_ms: u64,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            disabled: Vec::new(),
            dynamic_discovery: true,
            cache_ttl_ms: 3_600_000,
        }
    }
}

impl ModelsConfig {
    pub fn is_disabled(&self, model: &str) -> bool {
        if self.disabled.is_empty() {
            return false;
        }
        let key = crate::model_policy::model_key(model);
        self.disabled
            .iter()
            .any(|id| crate::model_policy::model_key(id) == key)
    }
}

/// 周期任务间隔。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TasksConfig {
    /// Token 预刷新扫描间隔。
    pub token_refresh_interval_ms: u64,
    /// 状态检查间隔。
    pub status_check_interval_ms: u64,
    /// 统计落盘间隔。
    pub stats_persist_interval_ms: u64,
}

impl Default for TasksConfig {
    fn default() -> Self {
        Self {
            token_refresh_interval_ms: 300_000,
            status_check_interval_ms: 60_000,
            stats_persist_interval_ms: 60_000,
        }
    }
}

/// 上下文窗口保护。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextConfig {
    /// 默认最大输入 token。
    pub max_input_tokens: u32,
    /// 普通请求安全比例。
    pub safe_input_ratio: f64,
    /// compact 请求安全比例。
    pub compact_safe_input_ratio: f64,
    /// 映射后的 Claude 模型窗口不足或上游拒绝超长请求时，自动 compact 并单次重试。
    pub auto_compact_on_overflow: bool,
    /// deferred Tool Search 工作集中工具定义允许占用的最大估算 token。
    pub max_tool_input_tokens: u32,
    /// 单次 Kiro 请求中允许发送的已加载工具数量。
    pub max_loaded_tools: usize,
    /// 单次序列化 Kiro 请求的最大字节数。
    pub max_upstream_payload_bytes: usize,
    /// compact 摘要模型；留空时复用当轮映射后的模型。
    pub compaction_summary_model: String,
    /// 主链路等待 compact 摘要的超时；后台结算另有固定有界宽限期。
    pub compaction_summary_timeout_ms: u64,
    /// 当轮 compact 后额外保留的最近完整 user/assistant 轮数。
    pub compaction_preserve_recent_turns: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_input_tokens: 200_000,
            safe_input_ratio: 0.95,
            compact_safe_input_ratio: 0.99,
            auto_compact_on_overflow: true,
            max_tool_input_tokens: 32_000,
            max_loaded_tools: MAX_LOADED_TOOLS,
            max_upstream_payload_bytes: 8 * 1024 * 1024,
            compaction_summary_model: String::new(),
            compaction_summary_timeout_ms: 120_000,
            compaction_preserve_recent_turns: 3,
        }
    }
}

/// 存储优化。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    /// 超过此账号数时压缩。
    pub compression_threshold: usize,
    /// 是否增量写。
    pub incremental_write: bool,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            compression_threshold: 100,
            incremental_write: true,
        }
    }
}

/// 旧版告警节流参数，仅为兼容已有配置保留。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyConfig {
    /// 旧版初始低额度档位；当前一次性异常告警不使用。
    pub low_credit_threshold_percent: f64,
    /// 旧版最大告警档位数；当前一次性异常告警不使用。
    pub max_notifications: u32,
    /// 旧版事件抑制窗口；当前改为异常恢复前永久去重。
    pub suppress_window_ms: u64,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            low_credit_threshold_percent: 10.0,
            max_notifications: 5,
            suppress_window_ms: 1_800_000,
        }
    }
}

/// 日志配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    /// tracing 级别。
    pub level: String,
    /// `json` 或 `pretty`。
    pub format: String,
    /// 日志基础路径；空字符串使用数据目录下的 `logs/crosslaned.log`。
    pub file_path: String,
    /// 每个级别、每个分片的最大大小。
    pub max_file_size_mb: u64,
    /// 按 UTC 日期保留最近多少天的文件。
    pub retention_days: u64,
    /// 每个日志级别每天最多保留多少个分片，达到上限后丢弃该级别文件日志。
    pub max_files_per_day: u64,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            format: "json".into(),
            file_path: String::new(),
            max_file_size_mb: 100,
            retention_days: 3,
            max_files_per_day: 3,
        }
    }
}

/// 管理面配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AdminConfig {
    /// Unix socket 路径。
    pub socket: String,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            socket: default_socket_path(),
        }
    }
}

/// IAM Identity Center SSO defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SsoConfig {
    /// Default IAM Identity Center start URL used by manual account login.
    pub start_url: String,
}

fn default_socket_path() -> String {
    default_socket_path_from(
        std::env::var("CROSSLANE_HOME").ok().as_deref(),
        std::env::var("XDG_RUNTIME_DIR").ok().as_deref(),
    )
}

fn default_socket_path_from(crosslane_home: Option<&str>, xdg_runtime: Option<&str>) -> String {
    if let Some(home) = crosslane_home {
        return format!("{home}/admin.sock");
    }
    if let Some(runtime) = xdg_runtime {
        return format!("{runtime}/crosslane/admin.sock");
    }
    "/run/crosslane/admin.sock".into()
}

/// One configured model-provider instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// Stable instance ID, independent from the driver kind.
    pub id: String,
    /// Driver kind such as `kiro` or `copilot`.
    pub kind: String,
    /// Whether this instance accepts new work.
    pub enabled: bool,
    /// Driver-specific settings. Unknown keys remain available to adapters.
    #[serde(default)]
    pub settings: BTreeMap<String, serde_json::Value>,
    /// Per-provider scheduling limits.
    pub pool: ProviderPoolConfig,
    /// Per-provider model catalog settings.
    pub models: ProviderModelsConfig,
    /// Per-provider routing defaults.
    pub routing: ProviderRoutingConfig,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            id: "kiro".into(),
            kind: "kiro".into(),
            enabled: true,
            settings: BTreeMap::new(),
            pool: ProviderPoolConfig::default(),
            models: ProviderModelsConfig::default(),
            routing: ProviderRoutingConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderPoolConfig {
    /// Optional provider-specific concurrency ceiling. Zero uses the
    /// adapter default.
    pub max_concurrent_per_account: usize,
    /// Scheduling weights for provider adapters with account pools.
    pub balance: ProviderBalanceConfig,
}

/// Account scheduling for providers without an authoritative credit balance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderBalanceConfig {
    /// Relative weight of occupied account concurrency slots.
    pub weight_active: f64,
    /// Relative weight of how recently the account was selected.
    pub weight_idle: f64,
    /// Time after which the recent-use penalty reaches zero.
    pub idle_window_ms: u64,
}

impl Default for ProviderBalanceConfig {
    fn default() -> Self {
        Self {
            weight_active: 0.8,
            weight_idle: 0.2,
            idle_window_ms: 300_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderModelsConfig {
    /// Background model discovery's target refresh interval.
    pub cache_ttl_ms: u64,
    /// Maximum age of catalog snapshots eligible for request routing.
    pub max_stale_ms: u64,
}

impl Default for ProviderModelsConfig {
    fn default() -> Self {
        Self {
            cache_ttl_ms: 300_000,
            max_stale_ms: 1_800_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderRoutingConfig {
    pub default_model_id: String,
    pub enable_model_fallback: bool,
    pub allow_cross_provider_fallback: bool,
    /// Automatic-route attempt deadline before a response is returned. Zero
    /// disables the deadline; a started stream keeps its existing idle timeout.
    pub failover_timeout_ms: u64,
}

impl Default for ProviderRoutingConfig {
    fn default() -> Self {
        Self {
            default_model_id: String::new(),
            enable_model_fallback: false,
            allow_cross_provider_fallback: false,
            failover_timeout_ms: 300_000,
        }
    }
}

/// 模型映射规则。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMappingRule {
    /// 规则名。
    pub name: String,
    /// 是否启用。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// `replace`、`alias` 或 `loadbalance`。
    #[serde(rename = "type")]
    pub kind: String,
    /// 源模型 glob。
    #[serde(default)]
    pub source_models: Vec<String>,
    /// 目标模型。
    #[serde(default)]
    pub target_models: Vec<String>,
    /// Provider instance scope. An omitted scope preserves the legacy Kiro
    /// behavior instead of silently granting a newly added provider access.
    #[serde(default)]
    pub providers: Vec<String>,
    /// Optional proxy-service scope.
    #[serde(default)]
    pub service_ids: Vec<String>,
    /// 数字越小优先级越高。
    #[serde(default)]
    pub priority: i32,
    /// 负载均衡权重。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weights: Option<Vec<u32>>,
    /// 剩余额度条件。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_remaining_credit_percent: Option<f64>,
    /// API key 条件。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_ids: Option<Vec<String>>,
    /// 可选生效时间窗口。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<ModelMappingSchedule>,
}

impl Default for ModelMappingRule {
    fn default() -> Self {
        Self {
            name: String::new(),
            enabled: true,
            kind: "replace".into(),
            source_models: Vec::new(),
            target_models: Vec::new(),
            providers: Vec::new(),
            service_ids: Vec::new(),
            priority: 0,
            weights: None,
            max_remaining_credit_percent: None,
            api_key_ids: None,
            schedule: None,
        }
    }
}

/// 模型映射生效时间窗口。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMappingSchedule {
    /// always/daily/range。
    #[serde(default = "default_schedule_mode")]
    pub mode: String,
    /// daily: 0=周日..6=周六。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days_of_week: Option<Vec<u8>>,
    /// daily: 兼容配置文件中的 mon/tue/... 写法。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days: Option<Vec<String>>,
    /// daily: 当天开始分钟数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_minutes: Option<u16>,
    /// daily: 兼容配置文件中的 HH:MM 写法。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    /// daily: 当天结束分钟数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_minutes: Option<u16>,
    /// daily: 兼容配置文件中的 HH:MM 写法。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
    /// range: 起始 Unix 毫秒。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_at: Option<i64>,
    /// range: 结束 Unix 毫秒。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_at: Option<i64>,
}

fn default_schedule_mode() -> String {
    "always".into()
}

/// Webhook 目标。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    /// 可读名称。
    pub name: String,
    /// dingtalk/wechat-work/telegram/discord/feishu/custom。
    pub kind: String,
    /// 目标 URL。
    pub url: String,
    /// 是否启用。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 订阅事件。
    #[serde(default)]
    pub events: Vec<String>,
    /// 钉钉签名密钥。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dingtalk_sign: Option<String>,
    /// Telegram chat ID。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telegram_chat_id: Option<String>,
    /// 自定义模板。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_template: Option<String>,
}

/// API key 配置项。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyConfig {
    /// 稳定 ID；为空时后续阶段生成。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// 可读名称。
    pub name: String,
    /// key 内容。
    pub key: String,
    /// key 格式。
    #[serde(default)]
    pub format: ApiKeyFormat,
    /// 是否启用。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 是否允许该 key 跳过客户端 User-Agent 校验。
    #[serde(default)]
    pub skip_user_agent_check: bool,
    /// credits 上限。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits_limit: Option<f64>,
    /// Provider instances this key may use. Empty means the legacy `kiro`
    /// provider only.
    #[serde(default)]
    pub allowed_providers: Vec<String>,
    /// Provider-qualified or unqualified model globs. Empty allows all models
    /// within `allowed_providers`.
    #[serde(default)]
    pub allowed_models: Vec<String>,
}

impl Default for ApiKeyConfig {
    fn default() -> Self {
        Self {
            id: None,
            name: String::new(),
            key: String::new(),
            format: ApiKeyFormat::default(),
            enabled: true,
            skip_user_agent_check: false,
            credits_limit: None,
            allowed_providers: Vec::new(),
            allowed_models: Vec::new(),
        }
    }
}

/// 一个独立的 API 代理监听实例。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyServiceConfig {
    /// 稳定服务 ID。
    pub id: String,
    /// 可读名称。
    pub name: String,
    /// 监听地址。
    pub host: String,
    /// 监听端口。
    pub port: u16,
    /// 是否启动监听。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 是否允许该服务的已认证请求跳过客户端 User-Agent 校验。
    #[serde(default)]
    pub skip_user_agent_check: bool,
    /// 允许访问此服务的 API key ID。
    #[serde(default)]
    pub api_key_ids: Vec<String>,
    /// 作为服务基础账号池的一个或多个账号标签；未配置时使用全局账号池。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_tag: Option<ServiceAccountTags>,
    /// 无论标签是否匹配，都手工加入服务账号池的账号 ID。
    #[serde(default)]
    pub account_ids: Vec<String>,
    /// 从该服务账号池中显式排除的账号 ID。
    #[serde(default)]
    pub excluded_account_ids: Vec<String>,
    /// Default provider for explicit mappings and undiscovered models; also
    /// the initial rotation preference for automatic Kiro/Copilot routing.
    /// Empty means `kiro`.
    #[serde(default)]
    pub default_provider: String,
    /// Provider instances exposed by this listener. Empty means `kiro`.
    #[serde(default)]
    pub allowed_providers: Vec<String>,
    /// Kiro backend for actual web search and its generation continuation.
    /// Ordinary generation permissions are unchanged. Account scope still applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_search_provider: Option<String>,
    /// CRS response contract used only by crosslaned's `crs` feature. Console
    /// is the default; CCR preserves HTTP errors for the client's SDK recovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crs_relay: Option<CrsRelayMode>,
    /// 创建时间（Unix 秒）。
    #[serde(default)]
    pub created_at: i64,
}

impl Default for ProxyServiceConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            host: String::new(),
            port: 0,
            enabled: true,
            skip_user_agent_check: false,
            api_key_ids: Vec::new(),
            account_tag: None,
            account_ids: Vec::new(),
            excluded_account_ids: Vec::new(),
            default_provider: String::new(),
            allowed_providers: Vec::new(),
            web_search_provider: None,
            crs_relay: None,
            created_at: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrsRelayMode {
    Console,
    Ccr,
}

/// A proxy service's tag-derived account pool, stored as a legacy scalar or a
/// list of tags. An account matching any listed tag belongs to the base pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServiceAccountTags {
    /// Legacy single-tag configuration.
    One(String),
    /// Union of multiple tags.
    Many(Vec<String>),
}

impl ServiceAccountTags {
    pub fn tags(&self) -> &[String] {
        match self {
            Self::One(tag) => std::slice::from_ref(tag),
            Self::Many(tags) => tags,
        }
    }

    pub fn single_tag(&self) -> Option<&str> {
        match self.tags() {
            [tag] => Some(tag),
            _ => None,
        }
    }

    pub fn from_tags(mut tags: Vec<String>) -> Option<Self> {
        match tags.len() {
            0 => None,
            1 => Some(Self::One(tags.remove(0))),
            _ => Some(Self::Many(tags)),
        }
    }
}

impl From<String> for ServiceAccountTags {
    fn from(tag: String) -> Self {
        Self::One(tag)
    }
}

impl From<&str> for ServiceAccountTags {
    fn from(tag: &str) -> Self {
        Self::One(tag.to_owned())
    }
}

impl ProxyServiceConfig {
    /// All configured base-pool tags, regardless of scalar or list storage.
    pub fn selected_account_tags(&self) -> &[String] {
        self.account_tag
            .as_ref()
            .map(ServiceAccountTags::tags)
            .unwrap_or_default()
    }

    /// Returns whether an account belongs to this service's effective pool.
    ///
    /// Exclusions override both the tag-derived base pool and explicit manual
    /// bindings. A service without an account tag inherits the global pool.
    pub fn includes_account(&self, account: &Account) -> bool {
        self.includes_provider_account("kiro", &account.id, &account.tags)
    }

    /// Provider-qualified IDs prevent collisions between independent stores.
    /// Bare IDs retain their historical Kiro meaning.
    pub fn includes_provider_account(&self, provider: &str, id: &str, tags: &[String]) -> bool {
        if !effective_provider_scope(&self.allowed_providers).contains(&provider) {
            return false;
        }
        let qualified = format!("{provider}/{id}");
        let matches =
            |reference: &String| reference == &qualified || (provider == "kiro" && reference == id);
        if self.excluded_account_ids.iter().any(matches) {
            return false;
        }
        if self.account_ids.iter().any(matches) {
            return true;
        }
        self.uses_global_account_pool()
            || self
                .selected_account_tags()
                .iter()
                .any(|tag| tags.contains(tag))
    }

    /// Returns true when the service inherits every non-excluded account.
    pub fn uses_global_account_pool(&self) -> bool {
        self.account_tag.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserAgentCheckPolicy {
    Enforced,
    GlobalDisabled,
    ServiceBypass,
    ApiKeyBypass,
}

impl UserAgentCheckPolicy {
    pub fn enforced(self) -> bool {
        matches!(self, Self::Enforced)
    }

    pub fn reason(self) -> &'static str {
        match self {
            Self::Enforced => "enforced",
            Self::GlobalDisabled => "global_disabled",
            Self::ServiceBypass => "service_bypass",
            Self::ApiKeyBypass => "api_key_bypass",
        }
    }
}

pub fn resolve_user_agent_check_policy(
    globally_enforced: bool,
    service_bypass: bool,
    api_key_bypass: bool,
) -> UserAgentCheckPolicy {
    if !globally_enforced {
        UserAgentCheckPolicy::GlobalDisabled
    } else if service_bypass {
        UserAgentCheckPolicy::ServiceBypass
    } else if api_key_bypass {
        UserAgentCheckPolicy::ApiKeyBypass
    } else {
        UserAgentCheckPolicy::Enforced
    }
}

/// 顶层配置。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// API 代理服务共享参数和默认值。
    pub server: ServerConfig,
    /// 上游。
    pub upstream: UpstreamConfig,
    /// 账号池。
    pub pool: PoolConfig,
    /// 功能开关。
    pub features: FeaturesConfig,
    /// 模型发现。
    pub models: ModelsConfig,
    /// 周期任务。
    pub tasks: TasksConfig,
    /// 上下文保护。
    pub context: ContextConfig,
    /// 存储参数。
    pub storage: StorageConfig,
    /// 告警参数。
    pub notify: NotifyConfig,
    /// 日志。
    pub log: LogConfig,
    /// 管理面。
    pub admin: AdminConfig,
    /// IAM Identity Center SSO defaults.
    pub sso: SsoConfig,
    /// Model-provider instances. An empty list is interpreted as one enabled
    /// legacy `kiro` instance for backward compatibility.
    #[serde(default, rename = "provider")]
    pub provider: Vec<ProviderConfig>,
    /// 模型映射列表。
    #[serde(default, rename = "model_mapping")]
    pub model_mapping: Vec<ModelMappingRule>,
    /// Webhook 列表。
    #[serde(default, rename = "webhook")]
    pub webhook: Vec<WebhookConfig>,
    /// API key 列表。
    #[serde(default, rename = "api_key")]
    pub api_key: Vec<ApiKeyConfig>,
    /// API 代理服务列表。默认为空；crosslaned 只启动管理面。
    #[serde(default, rename = "proxy_service")]
    pub proxy_service: Vec<ProxyServiceConfig>,
    /// 模型级 thinking 默认值。
    #[serde(default, rename = "model_thinking_mode")]
    pub model_thinking_mode: BTreeMap<String, bool>,
}

fn is_local_host(host: &str) -> bool {
    let lower = host.trim().to_ascii_lowercase();
    let normalized = lower
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(&lower);
    normalized.is_empty()
        || normalized == "localhost"
        || normalized == "127.0.0.1"
        || normalized == "::1"
}

impl Config {
    pub fn is_model_disabled(&self, model: &str) -> bool {
        self.models.is_disabled(self.unqualified_model(model))
    }

    pub fn unqualified_model<'a>(&self, model: &'a str) -> &'a str {
        model
            .split_once('/')
            .filter(|(prefix, _)| {
                if self.provider.is_empty() {
                    *prefix == "kiro"
                } else {
                    self.provider.iter().any(|provider| provider.id == *prefix)
                }
            })
            .map_or(model, |(_, id)| id)
    }

    /// Provider instances after applying the legacy no-provider migration.
    pub fn effective_providers(&self) -> Vec<ProviderConfig> {
        if self.provider.is_empty() {
            vec![ProviderConfig::default()]
        } else {
            self.provider.clone()
        }
    }

    /// Provider IDs allowed for a key, preserving legacy Kiro-only access.
    pub fn allowed_providers_for_key<'a>(&self, key: &'a ApiKeyConfig) -> Vec<&'a str> {
        effective_provider_scope(&key.allowed_providers)
    }

    /// Provider IDs allowed for a service, preserving legacy Kiro-only access.
    pub fn allowed_providers_for_service<'a>(
        &self,
        service: &'a ProxyServiceConfig,
    ) -> Vec<&'a str> {
        effective_provider_scope(&service.allowed_providers)
    }

    /// Default provider for one service.
    pub fn default_provider_for_service<'a>(&self, service: &'a ProxyServiceConfig) -> &'a str {
        if service.default_provider.trim().is_empty() {
            "kiro"
        } else {
            service.default_provider.as_str()
        }
    }

    /// 是否存在启用且非空的 API key。
    pub fn has_enabled_api_key(&self) -> bool {
        self.api_key
            .iter()
            .any(|key| key.enabled && !key.key.trim().is_empty())
    }

    /// 返回运行时实际使用的 token 提前刷新窗口。
    ///
    /// 定时扫描可能在临界点前刚好跳过一个 token，因此窗口至少为扫描间隔的
    /// 两倍，且不低于 10 分钟。这也保证旧配置中的 300 秒不会导致刷新过晚。
    pub fn effective_token_refresh_before_expiry(&self) -> i64 {
        const MINIMUM_LEAD_SECS: u64 = 600;

        let configured = u64::try_from(self.upstream.token_refresh_before_expiry).unwrap_or(0);
        let scan_interval_secs = self.tasks.token_refresh_interval_ms.div_ceil(1_000);
        let effective = configured
            .max(scan_interval_secs.saturating_mul(2))
            .max(MINIMUM_LEAD_SECS);
        i64::try_from(effective).unwrap_or(i64::MAX)
    }

    /// 校验配置。
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.server.port < 1024 {
            return Err(ConfigError::InvalidPort(self.server.port));
        }
        if self.server.keep_alive_timeout_ms == 0 {
            return invalid_config("server.keep_alive_timeout_ms", "must be greater than zero");
        }
        if self.server.tls.enabled {
            let inline = self
                .server
                .tls
                .cert
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
                && self
                    .server
                    .tls
                    .key
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty());
            let files = self
                .server
                .tls
                .cert_path
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
                && self
                    .server
                    .tls
                    .key_path
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty());
            if !inline && !files {
                return invalid_config(
                    "server.tls",
                    "enabled TLS requires both inline cert/key or both cert_path/key_path",
                );
            }
        }
        for (field, value) in [
            ("context.safe_input_ratio", self.context.safe_input_ratio),
            (
                "context.compact_safe_input_ratio",
                self.context.compact_safe_input_ratio,
            ),
        ] {
            if !(0.0..=1.0).contains(&value) {
                return Err(ConfigError::RatioOutOfRange { field, value });
            }
        }
        for (field, value) in [(
            "notify.low_credit_threshold_percent",
            self.notify.low_credit_threshold_percent / 100.0,
        )] {
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return invalid_config(field, "must be a finite value in 0.0..=1.0");
            }
        }
        for (field, value) in [
            ("pool.daily_credit_limit", self.pool.daily_credit_limit),
            (
                "pool.credit_estimate_per_1k_tokens",
                self.pool.credit_estimate_per_1k_tokens,
            ),
            (
                "pool.low_credit_min_remaining",
                self.pool.low_credit_min_remaining,
            ),
        ] {
            if !value.is_finite() || value < 0.0 {
                return invalid_config(field, "must be a finite non-negative number");
            }
        }
        if self
            .pool
            .max_overage_credits_per_account
            .is_some_and(|value| !value.is_finite() || value < 0.0)
        {
            return invalid_config(
                "pool.max_overage_credits_per_account",
                "must be a finite non-negative number",
            );
        }
        for (field, value) in [
            (
                "pool.balance.weight_active",
                self.pool.balance.weight_active,
            ),
            (
                "pool.balance.weight_credit",
                self.pool.balance.weight_credit,
            ),
            ("pool.balance.weight_idle", self.pool.balance.weight_idle),
        ] {
            if !value.is_finite() || value < 0.0 {
                return invalid_config(field, "must be a finite non-negative number");
            }
        }
        if self.pool.balance.weight_active
            + self.pool.balance.weight_credit
            + self.pool.balance.weight_idle
            <= 0.0
        {
            return invalid_config(
                "pool.balance",
                "at least one scheduling weight must be greater than zero",
            );
        }
        for (field, value) in [
            (
                "server.max_concurrent_requests",
                self.server.max_concurrent_requests,
            ),
            ("server.max_connections", self.server.max_connections),
            (
                "upstream.pool.http_max_connections",
                self.upstream.pool.http_max_connections,
            ),
            (
                "upstream.pool.stream_max_connections",
                self.upstream.pool.stream_max_connections,
            ),
            (
                "upstream.pool.http_pipelining",
                self.upstream.pool.http_pipelining,
            ),
        ] {
            if value == 0 {
                return invalid_config(field, "must be greater than zero");
            }
        }
        if self.context.max_loaded_tools > MAX_LOADED_TOOLS {
            return invalid_config(
                "context.max_loaded_tools",
                format!("must not exceed the proxy ceiling of {MAX_LOADED_TOOLS}"),
            );
        }
        if self.context.compaction_preserve_recent_turns > 64 {
            return invalid_config(
                "context.compaction_preserve_recent_turns",
                "must not exceed 64",
            );
        }
        if !(1..=256).contains(&self.features.tool_search_max_operations) {
            return invalid_config(
                "features.tool_search_max_operations",
                "must be between 1 and 256",
            );
        }
        if self.upstream.pool.stream_pipelining != 1 {
            return invalid_config(
                "upstream.pool.stream_pipelining",
                "must be exactly 1 to avoid stream head-of-line blocking",
            );
        }
        if self.upstream.pool.keep_alive_idle_ms == 0 || self.upstream.pool.keep_alive_max_ms == 0 {
            return invalid_config(
                "upstream.pool.keep_alive",
                "idle and maximum lifetime must be greater than zero",
            );
        }
        if self.upstream.stream_slot_wait_timeout_ms == 0
            || self.upstream.stream_read_timeout_ms == 0
        {
            return invalid_config(
                "upstream.stream_timeout",
                "slot wait and read timeout must be greater than zero",
            );
        }
        if self.server.adaptive.p99_recover_ms > self.server.adaptive.p99_degrade_ms {
            return invalid_config(
                "server.adaptive.p99_recover_ms",
                "must not exceed p99_degrade_ms",
            );
        }
        if self.server.adaptive.enabled
            && (self.server.adaptive.check_interval_ms == 0
                || self.server.adaptive.p99_degrade_ms == 0
                || self.server.adaptive.minimum_samples == 0
                || self.server.adaptive.minimum_samples > 10_000)
        {
            return invalid_config(
                "server.adaptive",
                "enabled adaptive admission requires a positive interval and degrade threshold, with sample count in 1..=10000",
            );
        }
        if !self.server.adaptive.overload_error_rate.is_finite()
            || !(0.0..=1.0).contains(&self.server.adaptive.overload_error_rate)
        {
            return invalid_config(
                "server.adaptive.overload_error_rate",
                "must be a finite number between 0 and 1",
            );
        }
        if self.upstream.token_refresh_before_expiry < 0 {
            return invalid_config(
                "upstream.token_refresh_before_expiry",
                "must be non-negative",
            );
        }
        if self.pool.cooldown.max_error_count == 0 || self.pool.cooldown.quota_error_threshold == 0
        {
            return invalid_config(
                "pool.cooldown",
                "error and quota thresholds must be greater than zero",
            );
        }
        for (field, value) in [
            (
                "context.max_input_tokens",
                self.context.max_input_tokens as usize,
            ),
            (
                "context.max_tool_input_tokens",
                self.context.max_tool_input_tokens as usize,
            ),
            ("context.max_loaded_tools", self.context.max_loaded_tools),
            (
                "context.max_upstream_payload_bytes",
                self.context.max_upstream_payload_bytes,
            ),
        ] {
            if value == 0 {
                return invalid_config(field, "must be greater than zero");
            }
        }
        if self.context.compaction_summary_timeout_ms == 0 {
            return invalid_config(
                "context.compaction_summary_timeout_ms",
                "must be greater than zero",
            );
        }
        if self.models.dynamic_discovery && self.models.cache_ttl_ms == 0 {
            return invalid_config("models.cache_ttl_ms", "must be greater than zero");
        }
        for model in &self.models.disabled {
            if model.trim().is_empty()
                || model.contains(['*', '?'])
                || model.chars().any(char::is_whitespace)
            {
                return invalid_config(
                    "models.disabled",
                    "must contain nonempty model IDs without whitespace or wildcards",
                );
            }
            if model.split_once('/').is_some_and(|(prefix, _)| {
                self.effective_providers()
                    .iter()
                    .any(|provider| provider.id == prefix)
            }) {
                return invalid_config(
                    "models.disabled",
                    "use unqualified model IDs; model disabling applies to all providers",
                );
            }
        }
        for (field, value) in [
            (
                "tasks.token_refresh_interval_ms",
                self.tasks.token_refresh_interval_ms,
            ),
            (
                "tasks.status_check_interval_ms",
                self.tasks.status_check_interval_ms,
            ),
            (
                "tasks.stats_persist_interval_ms",
                self.tasks.stats_persist_interval_ms,
            ),
        ] {
            if value == 0 {
                return invalid_config(field, "must be greater than zero");
            }
        }
        if !matches!(self.log.format.as_str(), "json" | "pretty") {
            return invalid_config("log.format", "expected json or pretty");
        }
        if !self.sso.start_url.trim().is_empty()
            && !self.sso.start_url.trim().starts_with("https://")
        {
            return invalid_config("sso.start_url", "must use https://");
        }
        if self.log.max_file_size_mb == 0
            || self.log.retention_days == 0
            || self.log.max_files_per_day == 0
        {
            return invalid_config(
                "log",
                "max_file_size_mb, retention_days, and max_files_per_day must be positive",
            );
        }
        let effective_providers = self.effective_providers();
        let mut provider_ids = BTreeSet::new();
        for (index, provider) in effective_providers.iter().enumerate() {
            ProviderId::parse(provider.id.clone()).map_err(|error| ConfigError::InvalidValue {
                field: format!("provider.{index}.id"),
                message: error.to_string(),
            })?;
            if !provider_ids.insert(provider.id.as_str()) {
                return invalid_config(format!("provider.{index}.id"), "must be unique");
            }
            if provider.kind.trim().is_empty() {
                return invalid_config(format!("provider.{index}.kind"), "must not be empty");
            }
            if provider.kind == "kiro" && provider.id != "kiro" {
                return invalid_config(
                    format!("provider.{index}.id"),
                    "the built-in Kiro compatibility provider must use id 'kiro'",
                );
            }
            if provider.id == "kiro" && provider.kind != "kiro" {
                return invalid_config(
                    format!("provider.{index}.kind"),
                    "provider id 'kiro' is reserved for the built-in Kiro compatibility provider",
                );
            }
            for (name, value) in [
                ("weight_active", provider.pool.balance.weight_active),
                ("weight_idle", provider.pool.balance.weight_idle),
            ] {
                if !value.is_finite() || value < 0.0 {
                    return invalid_config(
                        format!("provider.{index}.pool.balance.{name}"),
                        "must be a finite non-negative number",
                    );
                }
            }
            let balance_weight_sum =
                provider.pool.balance.weight_active + provider.pool.balance.weight_idle;
            if !balance_weight_sum.is_finite() || balance_weight_sum <= 0.0 {
                return invalid_config(
                    format!("provider.{index}.pool.balance"),
                    "at least one scheduling weight must be greater than zero",
                );
            }
            if provider.pool.balance.idle_window_ms == 0 {
                return invalid_config(
                    format!("provider.{index}.pool.balance.idle_window_ms"),
                    "must be greater than zero",
                );
            }
            if provider.models.cache_ttl_ms == 0 || provider.models.max_stale_ms == 0 {
                return invalid_config(
                    format!("provider.{index}.models"),
                    "cache_ttl_ms and max_stale_ms must be greater than zero",
                );
            }
            if provider.models.max_stale_ms < provider.models.cache_ttl_ms {
                return invalid_config(
                    format!("provider.{index}.models.max_stale_ms"),
                    "must be greater than or equal to cache_ttl_ms",
                );
            }
            if provider.routing.failover_timeout_ms > 86_400_000 {
                return invalid_config(
                    format!("provider.{index}.routing.failover_timeout_ms"),
                    "must not exceed 86400000 milliseconds (24 hours); zero disables the deadline",
                );
            }
        }

        let mut api_key_ids = BTreeSet::new();
        let mut api_key_names = BTreeSet::new();
        let mut api_key_values = BTreeSet::new();
        for (index, key) in self.api_key.iter().enumerate() {
            if key.name.trim().is_empty() {
                return invalid_config(format!("api_key.{index}.name"), "must not be empty");
            }
            if key.enabled && key.key.trim().is_empty() {
                return invalid_config(format!("api_key.{index}.key"), "must not be empty");
            }
            if !api_key_names.insert(key.name.as_str()) {
                return invalid_config(format!("api_key.{index}.name"), "must be unique");
            }
            if !key.key.is_empty() && !api_key_values.insert(key.key.as_str()) {
                return invalid_config(format!("api_key.{index}.key"), "must be unique");
            }
            if let Some(id) = key.id.as_deref() {
                if id.trim().is_empty() || !api_key_ids.insert(id) {
                    return invalid_config(
                        format!("api_key.{index}.id"),
                        "must be non-empty and unique",
                    );
                }
            }
            if key
                .credits_limit
                .is_some_and(|limit| !limit.is_finite() || limit < 0.0)
            {
                return invalid_config(
                    format!("api_key.{index}.credits_limit"),
                    "must be a finite non-negative number",
                );
            }
            validate_provider_scope(
                &format!("api_key.{index}.allowed_providers"),
                &key.allowed_providers,
                &provider_ids,
            )?;
        }
        let mut service_ids = BTreeSet::new();
        let mut service_names = BTreeSet::new();
        let mut service_addresses = BTreeSet::new();
        for (index, service) in self.proxy_service.iter().enumerate() {
            let field = format!("proxy_service.{index}");
            if service.id.trim().is_empty() || !service_ids.insert(service.id.as_str()) {
                return invalid_config(format!("{field}.id"), "must be non-empty and unique");
            }
            if service.name.trim().is_empty() || !service_names.insert(service.name.as_str()) {
                return invalid_config(format!("{field}.name"), "must be non-empty and unique");
            }
            if service.host.trim().is_empty() {
                return invalid_config(format!("{field}.host"), "must not be empty");
            }
            if service.port < 1024 {
                return invalid_config(format!("{field}.port"), "must be between 1024 and 65535");
            }
            if service.enabled && !service_addresses.insert((service.host.as_str(), service.port)) {
                return invalid_config(
                    format!("{field}.port"),
                    "enabled services must use unique host and port pairs",
                );
            }
            if service.api_key_ids.is_empty() {
                return invalid_config(
                    format!("{field}.api_key_ids"),
                    "at least one API key is required",
                );
            }
            validate_provider_scope(
                &format!("{field}.allowed_providers"),
                &service.allowed_providers,
                &provider_ids,
            )?;
            let default_provider = self.default_provider_for_service(service);
            if !provider_ids.contains(default_provider) {
                return invalid_config(
                    format!("{field}.default_provider"),
                    format!("references unknown provider {default_provider}"),
                );
            }
            if !effective_provider_scope(&service.allowed_providers).contains(&default_provider) {
                return invalid_config(
                    format!("{field}.default_provider"),
                    "must be included in allowed_providers",
                );
            }
            if let Some(backend) = &service.web_search_provider {
                if !self
                    .effective_providers()
                    .iter()
                    .any(|provider| provider.id == *backend && provider.kind == "kiro")
                {
                    return invalid_config(
                        format!("{field}.web_search_provider"),
                        "must reference a Kiro provider instance",
                    );
                }
            }
            let mut bound_ids = BTreeSet::new();
            for key_id in &service.api_key_ids {
                if !bound_ids.insert(key_id.as_str()) {
                    return invalid_config(
                        format!("{field}.api_key_ids"),
                        "must not contain duplicate IDs",
                    );
                }
                if !api_key_ids.contains(key_id.as_str()) {
                    return invalid_config(
                        format!("{field}.api_key_ids"),
                        format!("references unknown API key ID {key_id}"),
                    );
                }
            }
            if let Some(tags) = &service.account_tag {
                if tags.tags().is_empty() {
                    return invalid_config(format!("{field}.account_tag"), "must not be empty");
                }
                let mut unique_tags = BTreeSet::new();
                for tag in tags.tags() {
                    if tag.trim().is_empty() || tag.as_str() != tag.trim() {
                        return invalid_config(
                            format!("{field}.account_tag"),
                            "must contain non-empty tags without surrounding whitespace",
                        );
                    }
                    if !unique_tags.insert(tag.as_str()) {
                        return invalid_config(
                            format!("{field}.account_tag"),
                            "must not contain duplicate tags",
                        );
                    }
                }
            }
            let mut manual_account_ids = BTreeSet::new();
            for account_id in &service.account_ids {
                if account_id.trim().is_empty() || account_id.as_str() != account_id.trim() {
                    return invalid_config(
                        format!("{field}.account_ids"),
                        "must contain non-empty IDs without surrounding whitespace",
                    );
                }
                if !manual_account_ids.insert(account_id.as_str()) {
                    return invalid_config(
                        format!("{field}.account_ids"),
                        "must not contain duplicate IDs",
                    );
                }
            }
            let mut excluded_account_ids = BTreeSet::new();
            for account_id in &service.excluded_account_ids {
                if account_id.trim().is_empty() || account_id.as_str() != account_id.trim() {
                    return invalid_config(
                        format!("{field}.excluded_account_ids"),
                        "must contain non-empty IDs without surrounding whitespace",
                    );
                }
                if !excluded_account_ids.insert(account_id.as_str()) {
                    return invalid_config(
                        format!("{field}.excluded_account_ids"),
                        "must not contain duplicate IDs",
                    );
                }
                if manual_account_ids.contains(account_id.as_str()) {
                    return invalid_config(
                        format!("{field}.excluded_account_ids"),
                        format!("account {account_id} is also manually included"),
                    );
                }
            }
            if service.enabled && !is_local_host(&service.host) {
                let has_enabled_key = self.api_key.iter().any(|key| {
                    key.enabled
                        && !key.key.trim().is_empty()
                        && key
                            .id
                            .as_ref()
                            .is_some_and(|id| service.api_key_ids.contains(id))
                });
                if !has_enabled_key {
                    return Err(ConfigError::PublicBindWithoutApiKey);
                }
            }
        }
        for (index, rule) in self.model_mapping.iter().enumerate() {
            if !matches!(rule.kind.as_str(), "replace" | "alias" | "loadbalance") {
                return invalid_config(
                    format!("model_mapping.{index}.type"),
                    "expected replace, alias, or loadbalance",
                );
            }
            if rule.enabled && (rule.source_models.is_empty() || rule.target_models.is_empty()) {
                return invalid_config(
                    format!("model_mapping.{index}"),
                    "enabled rules require source_models and target_models",
                );
            }
            validate_provider_scope(
                &format!("model_mapping.{index}.providers"),
                &rule.providers,
                &provider_ids,
            )?;
            if rule.enabled {
                for source_provider in effective_provider_scope(&rule.providers) {
                    let allows_cross_provider = effective_providers
                        .iter()
                        .find(|provider| provider.id == source_provider)
                        .is_some_and(|provider| provider.routing.allow_cross_provider_fallback);
                    let cross_target = rule.target_models.iter().find_map(|target| {
                        target.split_once('/').and_then(|(target_provider, _)| {
                            (target_provider != source_provider
                                && provider_ids.contains(target_provider))
                            .then_some(target_provider)
                        })
                    });
                    if let Some(target_provider) = cross_target.filter(|_| !allows_cross_provider) {
                        return invalid_config(
                            format!("model_mapping.{index}.target_models"),
                            format!(
                                "routing from provider {source_provider} to {target_provider} requires provider {source_provider} routing.allow_cross_provider_fallback = true"
                            ),
                        );
                    }
                }
            }
            for service_id in &rule.service_ids {
                if !service_ids.contains(service_id.as_str()) {
                    return invalid_config(
                        format!("model_mapping.{index}.service_ids"),
                        format!("references unknown proxy service {service_id}"),
                    );
                }
            }
            if let Some(weights) = &rule.weights {
                if weights.len() != rule.target_models.len()
                    || weights.iter().all(|weight| *weight == 0)
                {
                    return invalid_config(
                        format!("model_mapping.{index}.weights"),
                        "must match target_models and contain a positive weight",
                    );
                }
            }
            if rule
                .max_remaining_credit_percent
                .is_some_and(|value| !value.is_finite() || !(0.0..=100.0).contains(&value))
            {
                return invalid_config(
                    format!("model_mapping.{index}.max_remaining_credit_percent"),
                    "must be in 0..=100",
                );
            }
            if rule.max_remaining_credit_percent.is_some() {
                let scoped_providers = effective_provider_scope(&rule.providers);
                if scoped_providers.iter().any(|provider| *provider != "kiro") {
                    return invalid_config(
                        format!("model_mapping.{index}.providers"),
                        "max_remaining_credit_percent is available only for the Kiro provider",
                    );
                }
                let crosses_provider = rule.target_models.iter().any(|target| {
                    target.split_once('/').is_some_and(|(target_provider, _)| {
                        target_provider != "kiro" && provider_ids.contains(target_provider)
                    })
                });
                if crosses_provider {
                    return invalid_config(
                        format!("model_mapping.{index}.max_remaining_credit_percent"),
                        "account-credit conditions cannot route from Kiro to another provider",
                    );
                }
            }
            if let Some(schedule) = &rule.schedule {
                if schedule
                    .days_of_week
                    .as_ref()
                    .is_some_and(|days| days.iter().any(|day| *day > 6))
                {
                    return invalid_config(
                        format!("model_mapping.{index}.schedule.days_of_week"),
                        "weekday values must be in 0..=6",
                    );
                }
                if [schedule.start_minutes, schedule.end_minutes]
                    .into_iter()
                    .flatten()
                    .any(|minutes| minutes >= 24 * 60)
                {
                    return invalid_config(
                        format!("model_mapping.{index}.schedule"),
                        "minute-of-day values must be below 1440",
                    );
                }
            }
        }
        let mut webhook_names = BTreeSet::new();
        for (index, target) in self.webhook.iter().enumerate() {
            if target.name.trim().is_empty() || !webhook_names.insert(target.name.as_str()) {
                return invalid_config(
                    format!("webhook.{index}.name"),
                    "must be non-empty and unique",
                );
            }
            if !matches!(
                target.kind.as_str(),
                "dingtalk"
                    | "wechat-work"
                    | "wechat"
                    | "telegram"
                    | "discord"
                    | "feishu"
                    | "custom"
            ) {
                return invalid_config(format!("webhook.{index}.type"), "unsupported webhook type");
            }
            if target.enabled
                && !(target.url.starts_with("https://") || target.url.starts_with("http://"))
            {
                return invalid_config(
                    format!("webhook.{index}.url"),
                    "enabled webhook URL must use http or https",
                );
            }
            if target.events.iter().any(|event| {
                !matches!(
                    event.as_str(),
                    "account-credit-protected"
                        | "account-quota-exhausted"
                        | "service-quota-exhausted"
                        | "token-refresh-failed"
                        // Legacy aliases remain valid so upgrades do not
                        // invalidate an existing webhook configuration.
                        | "low-credit"
                        | "account-banned"
                        | "token-expired"
                        | "quota-exhausted"
                        | "service-degraded"
                )
            }) {
                return invalid_config(
                    format!("webhook.{index}.events"),
                    "contains an unsupported event",
                );
            }
            if target.kind == "telegram"
                && target.enabled
                && target
                    .telegram_chat_id
                    .as_deref()
                    .is_none_or(|value| value.trim().is_empty())
            {
                return invalid_config(
                    format!("webhook.{index}.telegram_chat_id"),
                    "is required for enabled Telegram targets",
                );
            }
        }
        Ok(())
    }
}

fn invalid_config<T>(
    field: impl Into<String>,
    message: impl Into<String>,
) -> Result<T, ConfigError> {
    Err(ConfigError::InvalidValue {
        field: field.into(),
        message: message.into(),
    })
}

fn effective_provider_scope(values: &[String]) -> Vec<&str> {
    if values.is_empty() {
        vec!["kiro"]
    } else {
        values.iter().map(String::as_str).collect()
    }
}

fn validate_provider_scope(
    field: &str,
    values: &[String],
    provider_ids: &BTreeSet<&str>,
) -> Result<(), ConfigError> {
    let mut seen = BTreeSet::new();
    for provider_id in effective_provider_scope(values) {
        if !seen.insert(provider_id) {
            return invalid_config(field, "must not contain duplicate provider IDs");
        }
        if !provider_ids.contains(provider_id) {
            return invalid_config(field, format!("references unknown provider {provider_id}"));
        }
    }
    Ok(())
}

/// 首次运行写入的默认配置，每个字段附说明。
pub const DEFAULT_CONFIG_TOML: &str = r#"# ============================================================================
# crosslane 配置文件
# ============================================================================
#
# 本文件列出当前版本支持的全部配置。常用配置使用默认值生效；以 `#` 开头的
# 配置是可选示例，删除行首 `#` 后再按需修改。请勿把真实 API key、Webhook
# 密钥或 TLS 私钥提交到 Git。
#
# 除特别说明外：
# - `*_ms` 的单位是毫秒，`*_tokens` 的单位是 token，credits 支持小数。
# - daemon 会监听文件并热重载；仅 `admin.socket` 和 TLS enabled 模式切换需重启。
# - 修改已有服务/API key/模型映射时，优先使用 `crosslane service`、
#   `crosslane provider`、`crosslane apikey`、`crosslane model-map` 等命令，以免写错关联 ID。

# ----------------------------------------------------------------------------
# 模型提供源实例（可重复数组）
# ----------------------------------------------------------------------------
# 未配置任何 [[provider]] 时自动使用旧版 Kiro 提供源。只要开始显式配置，必须把
# 仍需使用的 Kiro 实例也写成 id="kiro"、kind="kiro"。推荐使用
# `crosslane provider add/edit/enable/disable/delete` 管理。
# Copilot 的 OAuth、API endpoint、账号并发和模型缓存设置均属于其 [[provider]] 实例，
# 不使用下方 Kiro 专项的 [sso]、[upstream]、[pool] 或 [models]。

# GitHub Copilot 提供源示例。
# [[provider]]
# 稳定实例 ID；小写字母开头，只能包含小写字母、数字和连字符。
# id = "copilot"
# 驱动类型；当前内置 "kiro" 和 "copilot"。
# kind = "copilot"
# 是否接受新请求。
# enabled = true

# [provider.settings]
# 新建 Copilot provider 会写入默认公开 client ID；可在此修改为组织批准的 OAuth App ID。
# client_id = "Iv1.b507a08c87ecfe98"
# Device Flow 的 refresh token 通常不需要 client secret；仅自有应用要求时配置。
# client_secret = "replace-me"
# GitHub Enterprise 主机标识。
# github_host = "github.com"
# GitHub REST API 根地址。
# github_api_base = "https://api.github.com"
# GitHub Device Flow / OAuth 根地址。
# oauth_base = "https://github.com"
# Copilot API token 到期前提前刷新的秒数。
# api_token_refresh_before_secs = 300
# 模型目录刷新同时查询的账号数；大账号池可按上游容量提高，必须大于零。
# model_refresh_concurrency = 32
# Copilot 上游连续无数据的读取超时；不会限制持续有数据的流总时长。
# stream_read_timeout_ms = 600000
# GitHub OAuth 刷新请求的总超时。
# oauth_refresh_timeout_ms = 30000
# 账号并发名额全部占用时的最长排队时间。
# account_queue_wait_timeout_ms = 30000
# token 响应缺少 endpoints.api 时使用的回退地址；Enterprise 专用实例按需配置。
# api_endpoint_fallback = "https://api.enterprise.githubcopilot.com"
# 动态 token 返回自定义 Copilot endpoint 时允许的额外主机。
# allowed_endpoint_hosts = ["api.example.ghe.com"]
# 仅供本地 mock 测试；生产环境必须保持 false。
# allow_insecure_http = false
# 发往 Copilot 的编辑器身份头。
# editor_version = "vscode/1.104.0"
# editor_plugin_version = "copilot-chat/0.31.0"
# integration_id = "vscode-chat"
# user_agent = "GitHubCopilotChat/0.31.0"

# [provider.pool]
# 单个账号并发上限；0 使用驱动默认值。
# max_concurrent_per_account = 2

# [provider.pool.balance]
# 非 Kiro 账号按并发压力与最近使用时间选号；上游未提供账号额度时不按额度打分。
# weight_active = 0.8
# weight_idle = 0.2
# idle_window_ms = 300000

# [provider.models]
# 后台模型目录刷新的目标间隔。
# cache_ttl_ms = 300000
# 业务查询允许复用旧目录快照的最长时间；过期目录不用于请求路由。
# max_stale_ms = 1800000

# [provider.routing]
# 未命中映射时使用的模型；空字符串保留请求模型。
# default_model_id = ""
# 是否允许驱动在同一提供源内自动降级模型。
# enable_model_fallback = false
# 是否允许映射规则把请求转发到另一提供源。
# allow_cross_provider_fallback = false
# failover_timeout_ms = 300000 # Automatic attempt deadline; 0 disables it.

# ----------------------------------------------------------------------------
# API 服务共享配置
# ----------------------------------------------------------------------------
# 这里的 host/port 是 `crosslane service create` 的默认值；实际监听实例记录在
# `[[proxy_service]]` 中。首次启动时服务列表为空，因此 daemon 只启动管理面。
[server]
# 新建 API 代理服务的默认监听地址。0.0.0.0 表示监听所有 IPv4 网卡。
host = "0.0.0.0"
# 新建 API 代理服务的默认端口；允许范围 1024-65535。
port = 5580
# 按协议校验客户端 User-Agent：Claude 路由仅限 Claude Code，OpenAI 路由仅限 Codex。
enforce_user_agent_check = true
# 所有代理服务共享的请求准入上限；超出时返回 503 和 Retry-After。
max_concurrent_requests = 500
# 下游 HTTP keep-alive 空闲超时。
keep_alive_timeout_ms = 30000
# 所有代理服务允许同时保持的最大下游连接数。
max_connections = 2000

# TLS 默认关闭。启用时必须配置一组证书来源：文件路径 cert_path/key_path，
# 或内联 PEM cert/key。不要同时混用两组。切换 enabled 后需重启 daemon。
[server.tls]
# 是否让代理服务直接提供 HTTPS。
enabled = false
# PEM 证书链文件路径。
# cert_path = "/etc/crosslane/fullchain.pem"
# PEM 私钥文件路径。
# key_path = "/etc/crosslane/privkey.pem"
# 内联 PEM 证书；适合由密钥管理系统生成配置的场景。
# cert = "-----BEGIN CERTIFICATE-----\n...\n-----END CERTIFICATE-----"
# 内联 PEM 私钥；配置文件权限应保持为 0600。
# key = "-----BEGIN PRIVATE KEY-----\n...\n-----END PRIVATE KEY-----"

# 自适应并发根据上游连接等待延迟和 429/503 比例动态收紧/恢复全局准入。
# 默认关闭；固定并发更容易预测，确认有持续高并发流量后再开启。
[server.adaptive]
# 是否启用自适应并发控制。
enabled = false
# 聚合样本并做一次升降并发决策的周期。
check_interval_ms = 10000
# 上游流式连接槽位等待 P99 达到该值时降低并发。
p99_degrade_ms = 200
# P99 低于该值时逐步恢复并发；不得高于 p99_degrade_ms。
p99_recover_ms = 100
# 每次决策至少需要的上游调用样本数；低流量时样本跨周期累计。
minimum_samples = 5
# 上游 429/503 占比达到该值时降低并发；范围 0.0-1.0。
overload_error_rate = 0.05

# ----------------------------------------------------------------------------
# Kiro 上游请求与连接池
# ----------------------------------------------------------------------------
[upstream]
# 强制首选 Kiro 上游端点；可选值："codewhisperer"、"amazonq"、"runtime"。
# 不配置时按账号类型和已探测结果自动选择；API key / GovCloud 使用区域化 runtime。
# preferred_endpoint = "amazonq"
# Kiro agent mode；可选值："auto"、"vibe"、"spec"。
agent_mode = "vibe"
# 一次请求失败后允许的重试次数；实际总尝试次数还受可用账号数限制。
max_retries = 3
# 期望在 token 到期前多少秒刷新。运行时至少取扫描间隔的 2 倍和 10 分钟。
token_refresh_before_expiry = 900
# 可选 Kiro MCP Web Search 地址，支持 `{region}` 占位符。显式 URL 使用兼容 JSON-RPC；
# 不设置时使用 Runtime 根路径的原生 InvokeMCP RPC。
# web_search_endpoint = "https://runtime.{region}.kiro.dev/mcp"
# 单次 MCP Web Search 请求超时。
web_search_timeout_ms = 60000
# 等待上游流式连接槽位的最长时间；超时后本次尝试失败。
stream_slot_wait_timeout_ms = 30000
# 流式上游连续无数据的最长时间；不是整个流的总时长限制。
stream_read_timeout_ms = 600000

# 上游使用独立的非流式和流式连接池，避免长连接阻塞普通 HTTP 请求。
[upstream.pool]
# 非流式请求连接池的最大连接数。
http_max_connections = 128
# 每条非流式 HTTP/1.1 连接允许的流水线深度。
http_pipelining = 5
# 流式请求连接池的最大连接数。
stream_max_connections = 256
# 必须为 1，让每个流独占连接，避免队首阻塞。
stream_pipelining = 1
# 上游连接池的目标空闲回收时间。
keep_alive_idle_ms = 30000
# 空闲回收时间上限；不会切断正在使用的流式连接。
keep_alive_max_ms = 60000

# ----------------------------------------------------------------------------
# Kiro 账号池、排队、额度保护与选号
# ----------------------------------------------------------------------------
[pool]
# 每个 Kiro 账号允许同时处理的最大请求数。
max_concurrent_per_account = 50
# 所有账号都繁忙时，等待队列最多容纳的请求数。
max_queue_size = 10
# 已进入队列的请求最长等待时间。
max_queue_wait_ms = 30000
# 队列已满后，为短暂释放槽位额外等待的时间。
queue_full_wait_ms = 5000
# 按剩余 credits 绝对值停用账号；剩余值小于等于该值时不再分配请求，0 表示关闭。
low_credit_min_remaining = 4.0
# 允许使用 Kiro 超额额度；开启后跳过代理侧低额度保护和余额耗尽拦截。
enable_overage = false
# 每个账号最多使用的 Kiro 超额 credits；不设置时仅遵循 Kiro 上游上限。
# max_overage_credits_per_account = 500.0
# 整个代理服务每天最多消耗的 credits；0 表示不限，按 UTC 自然日重置。
daily_credit_limit = 0.0
# 上游返回真实 usage 前，每 1,000 个估算 token 预留的 credits。
credit_estimate_per_1k_tokens = 1.0
# 估算预留额度时最多计入多少输出 token，避免超大 max_tokens 过度预留。
credit_estimate_output_token_cap = 8192
# 遇到额度耗尽错误时是否自动尝试其他可用账号。
auto_switch_on_quota_exhausted = true

# 选号分数由并发压力、额度消耗和最近使用三个权重组成。
# 权重无需相加为 1，但至少一个必须大于 0。
[pool.balance]
# 当前活跃请求数的权重。
weight_active = 0.5
# 已用额度比例的权重。
weight_credit = 0.4
# 最近使用时间的权重，鼓励轮换账号。
weight_idle = 0.1
# 将账号空闲时长归一化的窗口。
idle_window_ms = 300000

# 账号错误冷却和额度恢复探测。
[pool.cooldown]
# 连续错误达到该次数后进入一般冷却。
max_error_count = 3
# 一般冷却持续时间。
cooldown_ms = 30000
# 单次可恢复错误触发的短冷却时间。
error_cooldown_ms = 5000
# 账号被标记额度耗尽后，重新探测额度的间隔。
quota_reset_ms = 300000
# 统计额度类错误的滑动窗口长度。
quota_error_window_ms = 300000
# 滑动窗口内达到该错误数后，将账号标记为额度耗尽。
quota_error_threshold = 50

# ----------------------------------------------------------------------------
# Kiro 协议转换与工具功能
# ----------------------------------------------------------------------------
[features]
# 普通工具调用结束后由代理自动续轮的最大次数；0 表示不自动续轮，硬上限 30。
auto_continue_rounds = 0
# Anthropic Tool Search 一次搜索工作流允许的最大内部续轮数；运行时限制为 1-8。
tool_search_max_rounds = 4
# 单个客户端请求内实际执行 Tool Search 操作的总上限；允许范围 1-256。
tool_search_max_operations = 32
# 是否模拟 Anthropic 原生 Tool Search，让大量工具按需加载。
enable_tool_search = true
# 单个客户端请求内代理最多执行多少次 MCP Web Search。
web_search_max_rounds = 20
# 是否移除请求中的全部工具定义；用于临时排查工具兼容问题。
disable_tools = false
# 遇到上游限流或模型暂时不可用时是否自动选择同族可用模型降级。
enable_model_fallback = true
# 是否模拟 prompt cache 计费字段；不代表 Kiro 提供真实 Anthropic cache。
enable_prompt_cache = false
# 是否向上游 system prompt 注入协议兼容和工具使用说明。
enhance_system_prompt = true
# 是否缓冲流式工具调用片段，降低参数 JSON 被拆碎或乱序的概率。
buffer_tool_calls = true
# 仅为兼容旧配置保留；工具参数校验完成后立即发送，此字段不再增加等待。
tool_call_buffer_delay_ms = 0
# 思考内容输出格式；"claude" 输出 thinking block，"openai" 输出 reasoning_content。
thinking_output_format = "claude"
# 仅为兼容旧配置保留；逐轮按客户端请求转换 thinking，不再自动跳过工具结果/控制轮次。
adaptive_thinking = false
# 仅为兼容旧配置保留；原生 adaptive/effort 模式不使用独立 thinking token 硬上限。
max_thinking_budget_tokens = 8192
# 是否转换 Claude/OpenAI 的 Web Search 工具并交给 Kiro MCP 执行。
enable_web_tools = true
# 是否过滤模型误输出的内部工具协议文本。
enable_tool_leak_filter = true
# 客户端未指定模型或模型解析需要兜底时使用的模型 ID；空字符串表示自动选择。
default_model_id = ""

# ----------------------------------------------------------------------------
# 全局模型开关、Kiro 模型发现和后台任务
# ----------------------------------------------------------------------------
[models]
# 全局禁用的模型 ID（不带 provider 前缀），覆盖 Kiro/Copilot 与所有内部探测、回退。
# 公共模型列表隐藏；CLI model list 标记不可用；model enable 解除，热更新生效。
# 默认不禁用模型；取消下一行注释可启用示例规则。
# disabled = ["claude-sonnet-4.6", "gpt-4.1"]
# 是否从 Kiro 上游动态发现可用模型。
dynamic_discovery = true
# 模型发现结果的缓存时间；启用动态发现时必须大于 0。
cache_ttl_ms = 3600000

# 以下任务间隔中，token_refresh/status_check 仅用于 Kiro；stats_persist 为全局统计。
# Copilot 的短期 API token 由提供源独立按需刷新，模型缓存时间在 [provider.models] 配置。
[tasks]
# 扫描即将过期 token 的周期。
token_refresh_interval_ms = 300000
# 刷新账号额度、订阅与健康状态的周期。
status_check_interval_ms = 60000
# 将请求统计和 credits 用量写入磁盘的周期。
stats_persist_interval_ms = 60000

# ----------------------------------------------------------------------------
# Kiro 上下文窗口与自动 compact
# ----------------------------------------------------------------------------
[context]
# 未发现模型元数据时使用的默认最大输入 token 数。
max_input_tokens = 200000
# 普通请求允许使用模型上下文窗口的比例；范围 0.0-1.0。
safe_input_ratio = 0.95
# compact 请求允许使用模型上下文窗口的比例；范围 0.0-1.0。
compact_safe_input_ratio = 0.99
# 模型映射后上下文超限或上游拒绝超长请求时，是否自动摘要压缩并单次重试。
auto_compact_on_overflow = true
# deferred Tool Search 工作集中，工具定义允许占用的最大估算 token 数。
max_tool_input_tokens = 32000
# 单次请求可直接发送给 Kiro 的已加载工具数量；不得超过程序硬上限 512。
max_loaded_tools = 512
# 单次序列化 Kiro 请求体的最大字节数。
max_upstream_payload_bytes = 8388608
# compact 摘要使用的模型；空字符串表示复用当轮映射后的模型。
compaction_summary_model = ""
# 主请求等待 compact 摘要结果的最长时间。
compaction_summary_timeout_ms = 120000
# compact 摘要之外额外保留的最近完整 user/assistant 轮数；最大 64。
compaction_preserve_recent_turns = 3

# ----------------------------------------------------------------------------
# Kiro 账号库本地存储
# ----------------------------------------------------------------------------
[storage]
# 账号数超过该值时，将账号库基线编码为压缩格式以减少磁盘占用。
compression_threshold = 100
# 是否只追加账号变更并在达到阈值后压实；关闭时每次保存完整重写账号库。
incremental_write = true

# ----------------------------------------------------------------------------
# Kiro 告警策略兼容项
# ----------------------------------------------------------------------------
[notify]
# 旧版低额度阈值，兼容已有配置；当前一次性异常告警不使用。
low_credit_threshold_percent = 10.0
# 旧版低额度递进档位数，兼容已有配置；当前一次性异常告警不使用。
max_notifications = 5
# 旧版重复抑制窗口；当前策略会在异常恢复前永久去重。
suppress_window_ms = 1800000

# ----------------------------------------------------------------------------
# 日志
# ----------------------------------------------------------------------------
[log]
# tracing 过滤级别，例如 "error"、"warn"、"info"、"debug" 或模块级过滤表达式。
level = "info"
# 文件日志格式；可选值："json"、"pretty"。
format = "json"
# 日志基础文件路径；空字符串使用数据目录下的 logs/crosslaned.log。
file_path = ""
# 每个日志级别、每个分片的最大文件大小（MiB）。
max_file_size_mb = 100
# 按 UTC 日期保留最近多少天的日志文件。
retention_days = 3
# 每个日志级别每天最多保留的分片数；达到上限后丢弃该级别的新增文件日志。
max_files_per_day = 3

# ----------------------------------------------------------------------------
# 本机管理面
# ----------------------------------------------------------------------------
[admin]
# CLI 与 daemon 通信的 Unix socket。首次生成时会按 CROSSLANE_HOME/XDG_RUNTIME_DIR 改写。
# 修改后必须重启 daemon；容器 wrapper 会使用同一数据目录解析该路径。
socket = "/run/crosslane/admin.sock"

# ----------------------------------------------------------------------------
# Kiro IAM Identity Center SSO（不适用于 Copilot 的 GitHub/Azure SSO）
# ----------------------------------------------------------------------------
[sso]
# `account add-sso` 未传 --start-url 时使用的默认入口；必须是 https:// URL。
# 留空表示每次命令都必须显式传入 --start-url。
start_url = ""

# ----------------------------------------------------------------------------
# Kiro 模型级 thinking 开关
# ----------------------------------------------------------------------------
# key 为模型 ID 或模型族前缀，value 为是否允许 thinking。未配置的模型默认为 true。
# 更精确的完整模型 ID 应避免与宽泛前缀产生歧义。
[model_thinking_mode]
# "claude-sonnet-4.6" = true
# "claude-haiku" = false

# ----------------------------------------------------------------------------
# API key 与代理服务（可重复数组，默认均为空）
# ----------------------------------------------------------------------------
# 推荐使用 `crosslane service create` 和 `crosslane apikey` 命令维护。首次启动不创建
# 代理监听；`crosslane service create --name main` 会自动分配端口并生成首个 key。

# 单个 API key 示例。每增加一个 key，就增加一个 `[[api_key]]` 块。
# [[api_key]]
# 稳定 ID，供 proxy_service.api_key_ids 和模型映射引用；必须非空且唯一。
# id = "ak_example"
# 可读名称；必须唯一。
# name = "alice"
# 客户端请求时提交的密钥；必须唯一，配置文件权限应保持为 0600。
# key = "sk-replace-me"
# 密钥格式；可选值："sk"、"simple"、"token"。
# format = "sk"
# 是否允许该 key 认证请求。
# enabled = true
# 是否允许该 key 在所有已绑定服务上跳过客户端 User-Agent 校验。
# skip_user_agent_check = false
# 该 key 的累计 credits 上限；不配置表示不限，0 会禁止产生任何新消耗。
# credits_limit = 5000.0
# 该 key 可访问的提供源；省略或空数组保持旧版 Kiro-only 语义。
# allowed_providers = ["copilot"]
# 允许的模型 glob；可写成 provider/model，空数组允许范围内全部模型。
# allowed_models = ["copilot/claude-*", "copilot/gpt-*"]

# 单个代理监听实例示例。每增加一个服务，就增加一个 `[[proxy_service]]` 块。
# [[proxy_service]]
# 稳定服务 ID；必须非空且唯一。
# id = "svc_example"
# 可读名称；必须唯一。
# name = "main"
# 实际监听地址；非本机地址必须绑定至少一个已启用 API key。
# host = "0.0.0.0"
# 实际监听端口；允许范围 1024-65535，启用的服务不得重复 host+port。
# port = 5580
# 是否随 daemon 启动该监听实例。
# enabled = true
# 是否允许该服务的已认证请求跳过客户端 User-Agent 校验。
# skip_user_agent_check = false
# 允许访问该服务的 API key ID；至少一个，且都必须存在于 [[api_key]]。
# api_key_ids = ["ak_example"]
# 可选的 Kiro 基础账号标签；一个标签用字符串，多个标签用数组（匹配任一标签即可）。
# 不配置时继承 Kiro 全局账号池。修改已有服务的标签前必须先停用服务。
# account_tag = "team-a"
# 多标签时将上面的字符串值替换为 ["team-a", "team-b"]。
# 手工加入的 Kiro 账号 ID；即使标签不匹配也会进入该服务账号池。
# account_ids = ["acc_00000001"]
# 从 Kiro 全局、标签或手工范围中排除的账号 ID；这些字段不筛选 Copilot 账号。
# excluded_account_ids = ["acc_00000002"]
# 未带 provider/ 前缀的请求默认发送到哪个提供源。
# default_provider = "copilot"
# 该监听实例公开的提供源；省略或空数组保持旧版 Kiro-only 语义。
# allowed_providers = ["copilot"]
# 创建时间（Unix 秒）；由 CLI 创建时自动填写。
# created_at = 0

# ----------------------------------------------------------------------------
# 模型映射（可重复数组，默认为空）
# ----------------------------------------------------------------------------
# 推荐使用 `crosslane model-map` 命令维护。规则按 priority 从小到大匹配，首个
# 满足模型、API key、剩余额度和时间窗条件的规则生效。

# [[model_mapping]]
# 规则名称；用于 CLI 管理和日志定位。
# name = "opus 降级到 sonnet"
# 是否启用该规则。
# enabled = true
# 规则类型："replace"、"alias" 或 "loadbalance"。
# type = "replace"
# 要匹配的客户端模型 glob 列表。
# source_models = ["claude-opus-4*"]
# 映射后的目标模型列表；replace/alias 通常配置一个，loadbalance 可配置多个。
# target_models = ["claude-sonnet-4.6"]
# 规则作用的提供源；省略或空数组只作用于 Kiro，避免新来源意外继承旧规则。
# providers = ["copilot"]
# 规则作用的代理服务 ID；空数组表示不限制服务。
# service_ids = ["svc_example"]
# 数字越小优先级越高。
# priority = 10
# loadbalance 各目标的权重；数量必须与 target_models 相同，且至少一个大于 0。
# weights = [100]
# 仅当所选 Kiro 账号的剩余额度百分比低于该值时生效；范围 0-100，不能用于 Copilot。
# max_remaining_credit_percent = 10.0
# 仅对这些 API key ID 生效；不配置表示不限制 key。
# api_key_ids = ["ak_example"]

# 可选生效时间窗，隶属于上方最近一个 [[model_mapping]]。
# [model_mapping.schedule]
# 模式："always"、"daily" 或 "range"。
# mode = "daily"
# daily 的星期条件，0=周日、1=周一、...、6=周六；与 days 二选一。
# days_of_week = [1, 2, 3, 4, 5]
# daily 的可读星期条件；支持 sun/mon/tue/wed/thu/fri/sat。
# days = ["mon", "tue", "wed", "thu", "fri"]
# daily 的开始分钟数（从 00:00 起算）；与 start 二选一。
# start_minutes = 540
# daily 的可读开始时间，HH:MM。
# start = "09:00"
# daily 的结束分钟数；与 end 二选一。
# end_minutes = 1080
# daily 的可读结束时间，HH:MM。
# end = "18:00"
# range 的开始时间（Unix 毫秒）。
# start_at = 1787587200000
# range 的结束时间（Unix 毫秒）。
# end_at = 1787673600000

# ----------------------------------------------------------------------------
# Webhook 告警目标（可重复数组，默认为空）
# ----------------------------------------------------------------------------
# 推荐使用 `crosslane alert` 命令维护。每增加一个目标，就增加一个 `[[webhook]]` 块。

# [[webhook]]
# 可读名称；必须唯一。
# name = "运维群"
# 通知平台；对应 CLI 的 --platform。
# 可选 "dingtalk"、"wechat-work"、"telegram"、"discord"、"feishu"、"custom"。
# kind = "dingtalk"
# Webhook 接收地址；启用时必须使用 http:// 或 https://。
# url = "https://oapi.dingtalk.com/robot/send?access_token=replace-me"
# 是否启用该目标。
# enabled = true
# 订阅事件；可选 account-credit-protected、account-quota-exhausted、
# service-quota-exhausted、token-refresh-failed。空数组表示不订阅任何事件。
# 旧名称 low-credit、quota-exhausted、service-degraded、token-expired、account-banned 仍兼容。
# events = ["account-credit-protected", "account-quota-exhausted", "service-quota-exhausted", "token-refresh-failed"]
# 钉钉加签密钥；仅 kind="dingtalk" 且机器人开启加签时需要。
# dingtalk_sign = "SEC-replace-me"
# Telegram chat ID；kind="telegram" 且目标启用时必填。
# telegram_chat_id = "123456789"
# custom JSON/文本模板；支持 {{event}}、{{title}}、{{message}} 占位符。
# custom_template = '{"event":"{{event}}","title":"{{title}}","message":"{{message}}"}'
"#;

#[cfg(test)]
mod tests;
