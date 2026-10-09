//! 管理面行分隔 JSON-RPC 协议。

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crosslane_core::config::ServiceAccountTags;

/// Capability required before changing global model deny rules.
pub const GLOBAL_MODEL_DISABLE_FEATURE: &str = "global_model_disable";

/// 管理面方法名。
pub mod method {
    /// RPC/provider feature handshake.
    pub const V2_CAPABILITIES: &str = "v2.capabilities";
    /// List configured provider instances.
    pub const V2_PROVIDER_LIST: &str = "v2.provider.list";
    /// Show one provider instance.
    pub const V2_PROVIDER_SHOW: &str = "v2.provider.show";
    /// Aggregated provider-neutral account list.
    pub const V2_ACCOUNT_LIST: &str = "v2.account.list";
    /// Provider-neutral account detail.
    pub const V2_ACCOUNT_SHOW: &str = "v2.account.show";
    /// Import a provider token without placing it in argv.
    pub const V2_ACCOUNT_IMPORT_TOKEN: &str = "v2.account.importToken";
    /// Start provider-specific interactive login.
    pub const V2_LOGIN_START: &str = "v2.login.start";
    /// Poll an interactive login task.
    pub const V2_LOGIN_STATUS: &str = "v2.login.status";
    /// Cancel an interactive login task.
    pub const V2_LOGIN_CANCEL: &str = "v2.login.cancel";
    /// Supply a one-time code to a remote authorization browser.
    pub const V2_LOGIN_SUBMIT_CODE: &str = "v2.login.submitCode";
    /// Enable or disable a provider-neutral account.
    pub const V2_ACCOUNT_SET_ENABLED: &str = "v2.account.setEnabled";
    /// Remove a provider-neutral account.
    pub const V2_ACCOUNT_REMOVE: &str = "v2.account.remove";
    /// Refresh a provider-neutral account.
    pub const V2_ACCOUNT_REFRESH: &str = "v2.account.refresh";
    /// Update provider-neutral account tags.
    pub const V2_ACCOUNT_TAG: &str = "v2.account.tag";
    /// Probe one provider-neutral account's authentication and model catalog.
    pub const V2_ACCOUNT_PROBE: &str = "v2.account.probe";
    /// Clear one provider-neutral account's transient health state.
    pub const V2_ACCOUNT_RESET_HEALTH: &str = "v2.account.resetHealth";
    /// Export provider-neutral accounts, optionally redacting credentials.
    pub const V2_ACCOUNT_EXPORT: &str = "v2.account.export";
    /// Aggregated provider-neutral model catalog.
    pub const V2_MODELS: &str = "v2.models.list";
    /// 服务状态。
    pub const STATUS: &str = "status";
    /// 显示配置。
    pub const CONFIG_SHOW: &str = "config.show";
    /// 重载配置。
    pub const CONFIG_RELOAD: &str = "config.reload";
    /// 显示文件路径。
    pub const CONFIG_PATH: &str = "config.path";
    /// 列出账号。
    pub const ACCOUNT_LIST: &str = "account.list";
    /// 显示账号。
    pub const ACCOUNT_SHOW: &str = "account.show";
    /// 查询账号绑定的 API 代理服务。
    pub const ACCOUNT_SERVICES: &str = "account.services";
    /// 导入账号。
    pub const ACCOUNT_IMPORT: &str = "account.import";
    /// 导出账号。
    pub const ACCOUNT_EXPORT: &str = "account.export";
    /// 使用 IAM Identity Center 浏览器流添加账号。
    pub const ACCOUNT_ADD_SSO: &str = "account.addSso";
    /// 删除账号。
    pub const ACCOUNT_REMOVE: &str = "account.remove";
    /// 启停账号。
    pub const ACCOUNT_SET_ENABLED: &str = "account.setEnabled";
    /// 修改账号标签。
    pub const ACCOUNT_TAG: &str = "account.tag";
    /// 重新生成设备 ID。
    pub const ACCOUNT_REGEN_MACHINE_ID: &str = "account.regenMachineId";
    /// 刷新账号 token。
    pub const ACCOUNT_REFRESH: &str = "account.refresh";
    /// 探测账号模型端点。
    pub const ACCOUNT_PROBE: &str = "account.probe";
    /// 清除临时健康标记。
    pub const ACCOUNT_RESET_HEALTH: &str = "account.resetHealth";
    /// 账号池评分与健康状态。
    pub const POOL: &str = "pool";
    /// 上游诊断。
    pub const DIAGNOSE_ACCOUNT: &str = "diagnose.account";
    /// 探测公共上游端点网络连通性。
    pub const DIAGNOSE_ENDPOINTS: &str = "diagnose.endpoints";
    /// 查询可用订阅计划。
    pub const SUBSCRIPTIONS: &str = "subscriptions";
    /// 周期任务配置。
    pub const TASKS: &str = "tasks";
    /// 手动运行周期任务。
    pub const TASK_RUN: &str = "tasks.run";
    /// 代理统计。
    pub const STATS: &str = "stats";
    /// Request-log long polling over the admin socket.
    pub const LOGS: &str = "logs.follow";
    /// List physical daemon log files and their resolved locations.
    pub const LOG_FILES: &str = "logs.files";
    /// Search physical daemon log files for one request trace.
    pub const LOG_TRACE: &str = "logs.trace";
    /// 动态模型列表。
    pub const MODELS: &str = "models";
    /// 解析客户端模型 ID 到账号实际模型。
    pub const MODEL_RESOLVE: &str = "models.resolve";
    /// API key 列表与用量。
    pub const APIKEY_LIST: &str = "apikey.list";
    /// 清零 API key 用量。
    pub const APIKEY_RESET_USAGE: &str = "apikey.resetUsage";
    /// API 代理服务列表。
    pub const SERVICE_LIST: &str = "service.list";
    /// 创建 API 代理服务并生成首个 API key。
    pub const SERVICE_CREATE: &str = "service.create";
    /// 删除 API 代理服务。
    pub const SERVICE_DELETE: &str = "service.delete";
    /// 查询指定 API 代理服务绑定的 API key。
    pub const SERVICE_APIKEYS: &str = "service.apikeys";
    /// 查询指定 API 代理服务的有效账号池。
    pub const SERVICE_ACCOUNTS: &str = "service.accounts";
    /// 手工向指定 API 代理服务加入账号。
    pub const SERVICE_ACCOUNT_ADD: &str = "service.account.add";
    /// 从指定 API 代理服务排除账号。
    pub const SERVICE_ACCOUNT_REMOVE: &str = "service.account.remove";
    /// Webhook 列表。
    pub const WEBHOOK_LIST: &str = "webhook.list";
    /// 测试 webhook。
    pub const WEBHOOK_TEST: &str = "webhook.test";
    /// Webhook 发送历史。
    pub const WEBHOOK_LOGS: &str = "webhook.logs";

    /// 全部方法名。
    pub const ALL: &[&str] = &[
        V2_CAPABILITIES,
        V2_PROVIDER_LIST,
        V2_PROVIDER_SHOW,
        V2_ACCOUNT_LIST,
        V2_ACCOUNT_SHOW,
        V2_ACCOUNT_IMPORT_TOKEN,
        V2_LOGIN_START,
        V2_LOGIN_STATUS,
        V2_LOGIN_CANCEL,
        V2_LOGIN_SUBMIT_CODE,
        V2_ACCOUNT_SET_ENABLED,
        V2_ACCOUNT_REMOVE,
        V2_ACCOUNT_REFRESH,
        V2_ACCOUNT_TAG,
        V2_ACCOUNT_PROBE,
        V2_ACCOUNT_RESET_HEALTH,
        V2_ACCOUNT_EXPORT,
        V2_MODELS,
        STATUS,
        CONFIG_SHOW,
        CONFIG_RELOAD,
        CONFIG_PATH,
        ACCOUNT_LIST,
        ACCOUNT_SHOW,
        ACCOUNT_SERVICES,
        ACCOUNT_IMPORT,
        ACCOUNT_EXPORT,
        ACCOUNT_ADD_SSO,
        ACCOUNT_REMOVE,
        ACCOUNT_SET_ENABLED,
        ACCOUNT_TAG,
        ACCOUNT_REGEN_MACHINE_ID,
        ACCOUNT_REFRESH,
        ACCOUNT_PROBE,
        ACCOUNT_RESET_HEALTH,
        POOL,
        DIAGNOSE_ACCOUNT,
        DIAGNOSE_ENDPOINTS,
        SUBSCRIPTIONS,
        TASKS,
        TASK_RUN,
        STATS,
        LOGS,
        LOG_FILES,
        LOG_TRACE,
        MODELS,
        MODEL_RESOLVE,
        APIKEY_LIST,
        APIKEY_RESET_USAGE,
        SERVICE_LIST,
        SERVICE_CREATE,
        SERVICE_DELETE,
        SERVICE_APIKEYS,
        SERVICE_ACCOUNTS,
        SERVICE_ACCOUNT_ADD,
        SERVICE_ACCOUNT_REMOVE,
        WEBHOOK_LIST,
        WEBHOOK_TEST,
        WEBHOOK_LOGS,
    ];
}

/// Common provider filter used by v2 read APIs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderSelector {
    /// Provider instance ID or `all`; omitted means all for v2 queries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Optional driver-kind filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_kind: Option<String>,
}

/// Provider-neutral account summary returned by v2 management APIs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderAccountSummary {
    pub provider_id: String,
    pub provider_kind: String,
    pub id: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub enabled: bool,
    pub health: String,
    pub auth_state: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_current: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_limit: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_unit: Option<String>,
    #[serde(default)]
    pub supported_models: Vec<String>,
    #[serde(default)]
    pub details: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderAccountListResult {
    pub schema_version: u32,
    pub scope: ProviderSelector,
    pub accounts: Vec<ProviderAccountSummary>,
    #[serde(default)]
    pub errors: std::collections::BTreeMap<String, String>,
    pub complete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderModelListResult {
    /// Global deny rules, including models no longer present in a catalog.
    #[serde(default)]
    pub disabled_models: Vec<String>,
    pub schema_version: u32,
    pub scope: ProviderSelector,
    pub models: Vec<crosslane_core::provider::ProviderModel>,
    #[serde(default)]
    pub errors: std::collections::BTreeMap<String, String>,
    pub complete: bool,
}

/// 编解码错误。
#[derive(Debug, Error)]
pub enum RpcCodecError {
    /// 序列化失败。
    #[error("encode failed: {0}")]
    Encode(#[source] serde_json::Error),
    /// 反序列化失败。
    #[error("decode failed: {0}")]
    Decode(#[source] serde_json::Error),
}

/// RPC 错误载荷。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    /// HTTP 风格数字错误码。
    pub code: i32,
    /// 人类可读错误消息。
    pub message: String,
}

impl RpcError {
    /// 构造未知方法错误。
    pub fn unknown_method(method: &str) -> Self {
        Self {
            code: 404,
            message: format!("unknown method: {method}"),
        }
    }

    /// 构造参数错误。
    pub fn bad_params(detail: impl Into<String>) -> Self {
        Self {
            code: 400,
            message: detail.into(),
        }
    }

    /// 构造内部错误。
    pub fn internal(detail: impl Into<String>) -> Self {
        Self {
            code: 500,
            message: detail.into(),
        }
    }
}

/// 请求帧。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// 客户端生成的请求 ID。
    pub id: u64,
    /// 方法名。
    pub method: String,
    /// 方法参数。
    #[serde(default)]
    pub params: serde_json::Value,
}

impl Request {
    /// 构造请求。
    pub fn new(id: u64, method: impl Into<String>, params: serde_json::Value) -> Self {
        Self {
            id,
            method: method.into(),
            params,
        }
    }
}

/// 响应帧，成功与失败互斥。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Response {
    /// 成功响应。
    Ok {
        /// 请求 ID。
        id: u64,
        /// 方法结果。
        result: serde_json::Value,
    },
    /// 失败响应。
    Err {
        /// 请求 ID。
        id: u64,
        /// 错误载荷。
        error: RpcError,
    },
}

impl Response {
    /// 构造成功响应。
    pub fn ok(id: u64, result: serde_json::Value) -> Self {
        Self::Ok { id, result }
    }

    /// 构造失败响应。
    pub fn err(id: u64, error: RpcError) -> Self {
        Self::Err { id, error }
    }
}

/// 编码为一行，末尾仅有一个换行。
pub fn encode_line<T: Serialize>(value: &T) -> Result<String, RpcCodecError> {
    let mut line = serde_json::to_string(value).map_err(RpcCodecError::Encode)?;
    line.push('\n');
    Ok(line)
}

/// 从一行解码。
pub fn decode_line<T: DeserializeOwned>(line: &str) -> Result<T, RpcCodecError> {
    serde_json::from_str(line).map_err(RpcCodecError::Decode)
}

/// `status` 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderStatusView {
    pub id: String,
    pub kind: String,
    pub enabled: bool,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub account_total: usize,
    #[serde(default)]
    pub account_enabled: usize,
    #[serde(default)]
    pub account_available: usize,
}

/// `status` result for the daemon and its selected provider scope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResult {
    /// daemon 版本。
    pub version: String,
    /// 进程 ID。
    pub pid: u32,
    /// 运行秒数。
    pub uptime_secs: u64,
    /// 业务监听地址。
    pub listen: String,
    /// 已配置的 API 代理服务数。
    #[serde(default)]
    pub proxy_service_total: usize,
    /// 当前运行的 API 代理服务数。
    #[serde(default)]
    pub proxy_service_running: usize,
    /// 管理 socket。
    pub admin_socket: String,
    /// 账号总数。
    pub account_total: usize,
    /// 启用账号数。
    pub account_enabled: usize,
    /// 当前可参与账号池调度的账号数（不含模型兼容性筛选）。
    #[serde(default)]
    pub account_available: usize,
    /// 因低额度保护而不参与调度的账号数。
    #[serde(default)]
    pub account_protected: usize,
    /// 冷却账号数。
    #[serde(default)]
    pub account_cooling: usize,
    /// 额度耗尽账号数。
    #[serde(default)]
    pub account_exhausted: usize,
    /// 封禁账号数。
    #[serde(default)]
    pub account_banned: usize,
    /// 正在刷新凭证的账号数。
    #[serde(default)]
    pub account_refreshing: usize,
    /// 当前在途请求数。
    #[serde(default)]
    pub active_requests: usize,
    /// 当前生效的全局并发上限（可能由自适应准入动态调整）。
    #[serde(default)]
    pub max_concurrent_requests: usize,
    /// 等待账号许可的请求数。
    #[serde(default)]
    pub queued_requests: usize,
    /// 已记录请求数。
    #[serde(default)]
    pub request_count: u64,
    /// 成功率百分比。
    #[serde(default)]
    pub success_rate: f64,
    /// 平均延迟毫秒。
    #[serde(default)]
    pub average_latency_ms: u64,
    /// 已记录 credits。
    #[serde(default)]
    pub credits: f64,
    /// 统计作用域；`session` 表示当前 daemon 启动后。
    #[serde(default)]
    pub stats_scope: String,
    /// 当前统计范围的有效起点（Unix 秒，含）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats_start: Option<i64>,
    /// 当前统计范围的有效终点（Unix 秒，含）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats_end: Option<i64>,
    /// 聚合统计的时间分辨率。
    #[serde(default)]
    pub stats_resolution_secs: u64,
    /// 请求范围是否早于当前可查询的时间序列历史。
    #[serde(default)]
    pub stats_truncated: bool,
    #[serde(default)]
    pub daily_credit_day: String,
    #[serde(default)]
    pub daily_credit_used: f64,
    #[serde(default)]
    pub daily_credit_reserved: f64,
    #[serde(default)]
    pub daily_credit_limit: f64,
    /// 配置路径。
    pub config_path: String,
    /// 最近手工重载时间。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_reloaded_at: Option<i64>,
    /// 面向用户的提示。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// 业务面是否具备接收请求的条件；不影响 daemon 存活状态。
    #[serde(default)]
    pub ready: bool,
    /// readiness 降级原因。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readiness_reasons: Vec<String>,
    /// Provider selector used for this status view (`all` by default).
    #[serde(default)]
    pub provider_scope: String,
    /// Per-provider lifecycle and account counts.
    #[serde(default)]
    pub providers: Vec<ProviderStatusView>,
}

/// API 代理服务运行视图。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyServiceView {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub enabled: bool,
    #[serde(default)]
    pub skip_user_agent_check: bool,
    pub running: bool,
    #[serde(default)]
    pub api_key_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_tag: Option<String>,
    /// Full list of base-pool tags; `account_tag` remains for single-tag clients.
    #[serde(default)]
    pub account_tags: Vec<String>,
    #[serde(default)]
    pub account_ids: Vec<String>,
    #[serde(default)]
    pub excluded_account_ids: Vec<String>,
    #[serde(default)]
    pub default_provider: String,
    #[serde(default)]
    pub allowed_providers: Vec<String>,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `service.list` 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyServiceListResult {
    pub services: Vec<ProxyServiceView>,
}

/// `service.create` 参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyServiceCreateParams {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default)]
    pub skip_user_agent_check: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_tag: Option<ServiceAccountTags>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_format: Option<String>,
    #[serde(default)]
    pub allowed_providers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
}

/// 创建响应中的 API key 明文。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatedApiKey {
    pub id: String,
    pub name: String,
    pub key: String,
}

/// `service.create` 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyServiceCreateResult {
    pub service: ProxyServiceView,
    pub api_key: CreatedApiKey,
}

/// `service.delete` 参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyServiceDeleteParams {
    /// 服务 ID 或名称。
    pub service: String,
}

/// `service.delete` 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyServiceDeleteResult {
    pub service_id: String,
    pub service_name: String,
    /// Removed API key IDs that were exclusive to the deleted service.
    #[serde(default)]
    pub deleted_api_key_ids: Vec<String>,
    /// API key IDs retained because another proxy service still references them.
    #[serde(default)]
    pub retained_api_key_ids: Vec<String>,
}

/// `service.apikeys` 参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyServiceApiKeysParams {
    /// 服务 ID 或名称。
    pub service: String,
    /// 是否返回明文密钥。
    #[serde(default)]
    pub show_secret: bool,
}

/// `service.accounts` 参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyServiceAccountsParams {
    /// 服务 ID 或名称。
    pub service: String,
    #[serde(default)]
    pub provider: Option<String>,
}

/// `service.account.add` / `service.account.remove` 参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyServiceAccountChangeParams {
    /// 服务 ID 或名称。
    pub service: String,
    /// 账号 ID 或邮箱。
    pub accounts: Vec<String>,
    #[serde(default)]
    pub provider: Option<String>,
}

/// API 代理服务的有效账号池。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyServiceAccountsResult {
    pub service_id: String,
    pub service_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_tag: Option<String>,
    /// Full list of base-pool tags; `account_tag` remains for single-tag clients.
    #[serde(default)]
    pub account_tags: Vec<String>,
    pub uses_global_pool: bool,
    #[serde(default)]
    pub account_ids: Vec<String>,
    #[serde(default)]
    pub excluded_account_ids: Vec<String>,
    #[serde(default)]
    pub accounts: Vec<AccountSummary>,
    /// Unified pool view. The legacy Kiro-only accounts array remains available.
    #[serde(default)]
    pub provider_accounts: Vec<ProviderAccountSummary>,
}

/// 服务绑定的 API key 视图。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyServiceApiKeyView {
    pub id: String,
    pub name: String,
    pub format: String,
    pub enabled: bool,
    #[serde(default)]
    pub skip_user_agent_check: bool,
    #[serde(default = "default_true")]
    pub user_agent_check_enforced: bool,
    #[serde(default)]
    pub user_agent_check_reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits_limit: Option<f64>,
    /// 仅在显式请求明文时返回。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

fn default_true() -> bool {
    true
}

/// `service.apikeys` 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyServiceApiKeysResult {
    pub service_id: String,
    pub service_name: String,
    pub api_keys: Vec<ProxyServiceApiKeyView>,
}

/// 账号列表项。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountSummary {
    /// 账号 ID。
    pub id: String,
    /// 邮箱。
    pub email: String,
    /// 备注。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// 是否启用。
    pub enabled: bool,
    /// 运行时健康状态。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
    /// 标签。
    #[serde(default)]
    pub tags: Vec<String>,
    /// 订阅等级。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription: Option<String>,
    /// 当前额度。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credit_current: Option<f64>,
    /// 总额度。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credit_limit: Option<f64>,
    /// Token 过期时间。
    pub token_expires_at: i64,
    /// 持久化额度耗尽标记。
    pub credit_exhausted: bool,
}

/// `account.list` 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountListResult {
    /// 匹配账号。
    pub accounts: Vec<AccountSummary>,
}

/// `account.show` 结果，不包含 token。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountDetail {
    /// 列表字段。
    #[serde(flatten)]
    pub summary: AccountSummary,
    /// 设备 ID。
    pub machine_id: String,
    /// AWS 区域。
    pub region: String,
    /// 认证方式。
    pub auth_method: String,
    /// 创建时间。
    pub created_at: i64,
    /// 用量更新时间。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_updated_at: Option<i64>,
    /// 完整额度信息。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<crosslane_core::account::Usage>,
    /// 完整订阅信息。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription_detail: Option<crosslane_core::account::Subscription>,
    /// 最近发现的账号可用模型。
    #[serde(default)]
    pub supported_models: Vec<String>,
    /// 最近成功的生成端点。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_endpoint: Option<String>,
    /// 当前账号在途请求。
    #[serde(default)]
    pub active_requests: usize,
    /// 单账号并发上限。
    #[serde(default)]
    pub max_concurrent_requests: usize,
    /// 近期错误摘要。
    #[serde(default)]
    pub recent_errors: Vec<String>,
}

/// 一个账号与 API 代理服务的有效绑定关系。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountServiceBinding {
    /// 服务稳定 ID。
    pub service_id: String,
    /// 服务名称。
    pub service_name: String,
    /// 监听地址。
    pub host: String,
    /// 监听端口。
    pub port: u16,
    /// 配置是否启用。
    pub enabled: bool,
    /// 监听是否正在运行。
    pub running: bool,
    /// 绑定来源：`global`、`tag:<tag>` 或 `manual`；同一服务可有多个来源。
    #[serde(default)]
    pub binding_sources: Vec<String>,
}

/// `account.services` 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountServicesResult {
    /// 账号稳定 ID。
    pub account_id: String,
    /// 账号邮箱。
    pub account_email: String,
    /// 包含该账号的代理服务，按名称排序。
    pub services: Vec<AccountServiceBinding>,
}

/// 一个账号对客户端模型 ID 的解析结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelResolutionAccount {
    /// 账号稳定 ID。
    pub account_id: String,
    /// 账号展示名称。
    pub account_name: String,
    /// 当前有效健康状态。
    pub health: String,
    /// 当前是否可能被调度器选中。
    pub schedulable: bool,
    /// 该账号额度条件下，显式 model-map 后的模型。
    pub mapped_model: String,
    /// 命中的显式映射规则。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapping_rule: Option<String>,
    /// 最终发送到 Kiro 的精确模型 ID；无法匹配时为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_model: Option<String>,
    /// 是否因为原模型无法匹配而使用了默认模型。
    pub used_default: bool,
    /// 模型集合来源：account_cache 或 static_catalog。
    pub model_source: String,
    /// 该账号模型集合的数量。
    pub available_model_count: usize,
    /// 无法匹配时的原因。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `models.resolve` 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelResolutionResult {
    /// 客户端传入的模型 ID。
    pub input_model: String,
    /// 不考虑账号额度条件时的显式映射结果。
    pub mapped_model: String,
    /// 命中的初始显式映射规则。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapping_rule: Option<String>,
    /// 所有可匹配账号结果一致时的唯一最终模型。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_model: Option<String>,
    /// 当前账号池中所有可能的最终模型。
    #[serde(default)]
    pub possible_models: Vec<String>,
    /// 成功匹配的可调度账号数量。
    pub matched_accounts: usize,
    /// 当前可调度账号总数。
    pub total_accounts: usize,
    /// 各账号的真实缓存解析结果。
    #[serde(default)]
    pub accounts: Vec<ModelResolutionAccount>,
}

/// `config.show` 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigShowResult {
    /// 配置路径。
    pub path: String,
    /// 磁盘原文。
    pub raw: String,
    /// 合并默认值后的配置。
    pub effective_json: serde_json::Value,
}

/// `config.path` 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigPathResult {
    /// 配置文件。
    pub config_file: String,
    /// 账号文件。
    pub accounts_file: String,
    /// 日额度文件。
    pub daily_file: String,
    /// 统计文件。
    pub stats_file: String,
    /// 管理 socket。
    pub admin_socket: String,
    /// daemon 日志基础路径；实际文件会附加日期、级别和分片号。
    #[serde(default)]
    pub log_base_path: String,
    /// daemon 日志目录。
    #[serde(default)]
    pub log_directory: String,
}

/// A physical daemon log file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogFileView {
    /// Absolute or daemon-working-directory-relative file path.
    pub path: String,
    /// Corresponding path on the Docker host when supplied by the host wrapper.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_path: Option<String>,
    /// Log level partition represented by this file.
    pub level: String,
    /// UTC date partition in YYYY-MM-DD form.
    pub date: String,
    /// File size in bytes.
    pub size_bytes: u64,
    /// Last modification time as Unix seconds when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<i64>,
}

/// `logs.files` result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogFilesResult {
    /// Configured base path before date/level partition suffixes are added.
    pub base_path: String,
    /// Corresponding base path on the Docker host when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_base_path: Option<String>,
    /// Directory containing physical log files.
    pub directory: String,
    /// Corresponding directory on the Docker host when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_directory: Option<String>,
    /// Active formatter (`json` or `pretty`).
    pub format: String,
    /// Active tracing filter expression.
    pub level_filter: String,
    /// Physical files, sorted newest first.
    pub files: Vec<LogFileView>,
}

/// One physical log record associated with a trace ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogTraceEntry {
    /// Physical log file containing the record.
    pub path: String,
    /// Exact severity shard containing the record.
    pub level: String,
    /// UTC date shard in YYYY-MM-DD form.
    pub date: String,
    /// One-based line number in the physical file.
    pub line: u64,
    /// Parsed JSON record, or a string for pretty-formatted logs.
    pub record: serde_json::Value,
}

/// `logs.trace` result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogTraceResult {
    /// Exact trace ID used for the search.
    pub trace_id: String,
    /// Matching records ordered by timestamp.
    pub entries: Vec<LogTraceEntry>,
    /// Number of physical shards scanned.
    pub files_scanned: usize,
    /// Number of bytes read from physical shards.
    pub bytes_scanned: u64,
    /// Total matching records encountered before the tail limit.
    pub matched_records: usize,
    /// Whether safety or tail limits truncated the result.
    pub truncated: bool,
}

/// `config.reload` 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigReloadResult {
    /// 是否应用。
    pub applied: bool,
    /// 失败原因。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 需重启字段。
    #[serde(default)]
    pub needs_restart: Vec<String>,
}

/// `account.import` 参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountImportParams {
    /// 待导入账号。
    pub accounts: Vec<crosslane_core::account::Account>,
    /// 合并到每个导入账号的标签。
    #[serde(default)]
    pub tags: Vec<String>,
}

/// `account.import` 结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountImportResult {
    /// 成功导入数量。
    pub imported: usize,
    /// 因重复跳过的 ID。
    #[serde(default)]
    pub skipped: Vec<String>,
}

/// Ephemeral credentials for a daemon-side Copilot authorization browser.
/// Passwords belong to their named origin and are never persisted with accounts.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopilotBrowserCredentials {
    /// GitHub username, or a verified email for password-only GitHub login.
    pub github_username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sso_username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sso_password: Option<String>,
    /// Optional organization/enterprise SSO entry point on the GitHub origin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sso_start_url: Option<String>,
}

impl std::fmt::Debug for CopilotBrowserCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CopilotBrowserCredentials")
            .field("github_username", &self.github_username)
            .field(
                "github_password",
                &self.github_password.as_ref().map(|_| "<redacted>"),
            )
            .field("sso_username", &self.sso_username)
            .field(
                "sso_password",
                &self.sso_password.as_ref().map(|_| "<redacted>"),
            )
            .field("sso_start_url", &self.sso_start_url)
            .finish()
    }
}

/// `account.addSso` 参数。
#[derive(Clone, Serialize, Deserialize)]
pub struct AccountAddSsoParams {
    /// IAM Identity Center 登录邮箱。
    pub email: String,
    /// 仅在管理 socket 上传输的登录密码。
    pub password: String,
    /// IAM Identity Center start URL。
    pub start_url: String,
    /// AWS 区域。
    #[serde(default = "default_account_sso_region")]
    pub region: String,
    /// 是否显示浏览器窗口。
    #[serde(default)]
    pub headful: bool,
    /// 新账号标签。
    #[serde(default)]
    pub tags: Vec<String>,
}

impl std::fmt::Debug for AccountAddSsoParams {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AccountAddSsoParams")
            .field("email", &self.email)
            .field("password", &"<redacted>")
            .field("start_url", &self.start_url)
            .field("region", &self.region)
            .field("headful", &self.headful)
            .field("tags", &self.tags)
            .finish()
    }
}

fn default_account_sso_region() -> String {
    "us-east-1".into()
}

/// 单账号定位参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountRefParams {
    /// 账号 ID 或邮箱。
    pub id: String,
}

/// `account.setEnabled` 参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountSetEnabledParams {
    /// 账号 ID 或邮箱。
    pub id: String,
    /// 目标状态。
    pub enabled: bool,
}

/// `account.tag` 参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountTagParams {
    /// 账号 ID 或邮箱。
    pub id: String,
    /// 新增标签。
    #[serde(default)]
    pub add: Vec<String>,
    /// 删除标签。
    #[serde(default)]
    pub remove: Vec<String>,
}

/// `account.tag` 批量参数，与单账号的 `id` 参数二选一。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountTagBatchParams {
    /// 批量账号 ID 或邮箱。
    pub ids: Vec<String>,
    /// 新增标签。
    #[serde(default)]
    pub add: Vec<String>,
    /// 删除标签。
    #[serde(default)]
    pub remove: Vec<String>,
}

/// `account.tag` 修改后的单账号标签。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountTagResult {
    pub id: String,
    pub tags: Vec<String>,
}

/// `account.tag` 批量修改结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountTagBatchResult {
    pub accounts: Vec<AccountTagResult>,
}

/// `account.list` 参数。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccountListParams {
    /// 标签过滤。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// 是否只返回启用账号。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_only: Option<bool>,
    /// 状态过滤：available/low_credit/disabled/exhausted/cooling/banned/refreshing/unavailable。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// 排序字段：email（默认）/credit/id。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_browser_credentials_do_not_expose_passwords_in_debug() {
        let credentials = CopilotBrowserCredentials {
            github_username: "octocat".into(),
            github_password: Some("github-secret".into()),
            sso_username: Some("user@example.com".into()),
            sso_password: Some("azure-secret".into()),
            sso_start_url: None,
        };
        let debug = format!("{credentials:?}");
        assert!(!debug.contains("github-secret"));
        assert!(!debug.contains("azure-secret"));
        assert!(debug.contains("<redacted>"));
        let encoded = serde_json::to_value(&credentials).unwrap();
        assert_eq!(encoded["github_password"], "github-secret");
        assert_eq!(encoded["sso_password"], "azure-secret");
    }

    #[test]
    fn request_and_responses_have_flat_exclusive_shapes() {
        let request = Request::new(7, method::STATUS, serde_json::json!({}));
        let request_json = serde_json::to_value(request).expect("request");
        assert_eq!(request_json["id"], 7);
        assert_eq!(request_json["method"], "status");

        let ok = serde_json::to_value(Response::ok(1, serde_json::json!({"pid": 42})))
            .expect("ok response");
        assert_eq!(ok["result"]["pid"], 42);
        assert!(ok.get("error").is_none());

        let error = serde_json::to_value(Response::err(
            2,
            RpcError {
                code: 404,
                message: "not found".into(),
            },
        ))
        .expect("error response");
        assert_eq!(error["error"]["code"], 404);
        assert!(error.get("result").is_none());
    }

    #[test]
    fn line_codec_roundtrips_and_escapes_inner_newlines() {
        let response = Response::ok(1, serde_json::json!({"text": "multi\nline"}));
        let line = encode_line(&response).expect("encode");
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1);
        let decoded: Response = decode_line(line.trim_end()).expect("decode");
        match decoded {
            Response::Ok { id, result } => {
                assert_eq!(id, 1);
                assert_eq!(result["text"], "multi\nline");
            }
            Response::Err { .. } => panic!("expected ok"),
        }
        assert!(matches!(
            decode_line::<Request>("{not json"),
            Err(RpcCodecError::Decode(_))
        ));
    }

    #[test]
    fn method_names_are_unique() {
        let unique: std::collections::HashSet<_> = method::ALL.iter().collect();
        assert_eq!(unique.len(), method::ALL.len());
    }

    #[test]
    fn sso_params_debug_redacts_the_password() {
        let params = AccountAddSsoParams {
            email: "user@example.com".into(),
            password: "super-secret-password".into(),
            start_url: "https://example.awsapps.com/start".into(),
            region: "us-east-1".into(),
            headful: false,
            tags: vec!["team-a".into()],
        };

        let debug = format!("{params:?}");
        assert!(!debug.contains("super-secret-password"));
        assert!(debug.contains("<redacted>"));
        assert!(debug.contains("user@example.com"));
    }

    #[test]
    fn status_and_summary_serialization_omit_absent_values() {
        let status = StatusResult {
            version: "0.1.0".into(),
            pid: 42,
            uptime_secs: 5,
            listen: "127.0.0.1:5580".into(),
            proxy_service_total: 1,
            proxy_service_running: 1,
            admin_socket: "/run/crosslane/admin.sock".into(),
            account_total: 0,
            account_enabled: 0,
            account_available: 0,
            account_protected: 0,
            account_cooling: 0,
            account_exhausted: 0,
            account_banned: 0,
            account_refreshing: 0,
            active_requests: 0,
            max_concurrent_requests: 500,
            queued_requests: 0,
            request_count: 0,
            success_rate: 0.0,
            average_latency_ms: 0,
            credits: 0.0,
            stats_scope: "session".into(),
            stats_start: Some(1_700_000_000),
            stats_end: Some(1_700_000_005),
            stats_resolution_secs: 60,
            stats_truncated: false,
            daily_credit_day: "2026-08-06".into(),
            daily_credit_used: 0.0,
            daily_credit_reserved: 0.0,
            daily_credit_limit: 0.0,
            config_path: "/tmp/config.toml".into(),
            config_reloaded_at: None,
            hint: Some("empty".into()),
            ready: true,
            readiness_reasons: Vec::new(),
            provider_scope: "all".into(),
            providers: Vec::new(),
        };
        let back: StatusResult =
            serde_json::from_str(&serde_json::to_string(&status).expect("serialize status"))
                .expect("deserialize status");
        assert_eq!(back.pid, 42);
        assert!(back.ready);

        let legacy: StatusResult = serde_json::from_value(serde_json::json!({
            "version":"0.0.1","pid":1,"uptime_secs":1,"listen":"-",
            "admin_socket":"/tmp/admin.sock","account_total":0,"account_enabled":0,
            "config_path":"/tmp/config.toml"
        }))
        .expect("legacy status");
        assert!(!legacy.ready);
        assert_eq!(legacy.account_protected, 0);
        assert_eq!(legacy.account_refreshing, 0);

        let summary = AccountSummary {
            id: "acc_00000001".into(),
            email: "a@example.com".into(),
            label: None,
            enabled: true,
            health: Some("available".into()),
            tags: vec![],
            subscription: None,
            credit_current: None,
            credit_limit: None,
            token_expires_at: 0,
            credit_exhausted: false,
        };
        let json = serde_json::to_value(summary).expect("summary");
        assert!(json.get("label").is_none());
        assert!(json.get("subscription").is_none());
    }
}
