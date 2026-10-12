//! Pool, diagnostics, statistics, API-key and alert commands.

use anyhow::{anyhow, Context, Result};
use clap::{Subcommand, ValueEnum};
use crosslane_core::config::DEFAULT_COPILOT_OAUTH_CLIENT_ID;
use crosslane_core::paths::Paths;
use crosslane_ipc::protocol::method;
use crosslane_ipc::protocol::{
    ConfigPathResult, ConfigReloadResult, ConfigShowResult, LogFilesResult, LogTraceResult,
    ModelResolutionResult, ProxyServiceAccountsResult, ProxyServiceApiKeysResult,
    ProxyServiceCreateResult, ProxyServiceDeleteResult, ProxyServiceListResult,
};
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};
use toml_edit::DocumentMut;

use crate::client::AdminClient;
use crate::output::{format_timestamp, print_json, print_json_line, render_table};
use crate::ModelMapCommand;

#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// 列出 Kiro/Copilot API 代理服务。
    #[command(
        after_help = "可用 --provider <ID> 筛选允许该来源的服务；省略时显示全部。\n\n示例：\n  crosslane service list --provider kiro\n  crosslane service list --provider copilot"
    )]
    List {
        /// 只显示允许该提供源的服务；`all` 等同于不筛选。
        #[arg(long)]
        provider: Option<String>,
    },
    /// 显示单个服务的监听、默认来源与允许来源。
    #[command(
        after_help = "示例：\n  crosslane service show main\n  crosslane --json service show svc_abcd"
    )]
    Show {
        /// 服务 ID 或名称。
        service: String,
    },
    /// 创建服务并生成首个 API key；业务监听受 CROSSLANE_DISABLE_HTTP 控制。
    #[command(
        after_help = "省略 --provider 时保持 Kiro-only；服务与自动生成的 API key 使用相同来源范围。--account-tag 筛选服务允许的 Kiro/Copilot 账号。省略 --port 时，首个服务配置为 server.port（监听可由 CROSSLANE_HTTP_PORT 覆盖），之后使用现有 service 最大生效端口加 1；可用 --port 显式指定。\n\n示例：\n  crosslane service create --name kiro --provider kiro --default-provider kiro\n  crosslane service create --name copilot --provider copilot --default-provider copilot\n  crosslane service create --name unified --provider kiro --provider copilot --default-provider kiro"
    )]
    Create {
        /// 代理服务的唯一可读名称。
        #[arg(long)]
        name: String,
        /// 监听地址；省略时使用 server.host。
        #[arg(long)]
        host: Option<String>,
        /// 监听端口，范围 1024..=65535；省略时为现有 service 最大生效端口加 1，首个服务配置为 server.port。
        #[arg(long, value_parser = parse_service_port)]
        port: Option<u16>,
        /// 使用一个或多个 Kiro/Copilot 账号标签的并集作为基础账号池；不指定时使用全局账号池。
        #[arg(long, num_args = 1.., value_delimiter = ',', value_name = "TAG")]
        account_tag: Vec<String>,
        /// 是否允许该服务的已认证请求跳过客户端 User-Agent 校验。
        #[arg(
            long,
            value_name = "BOOL",
            action = clap::ArgAction::Set,
            default_value_t = false
        )]
        skip_user_agent_check: bool,
        /// 自动生成的首个客户端 Key 名称；默认 <服务名称>-default。
        #[arg(long)]
        api_key_name: Option<String>,
        /// 首个客户端 Key 的密钥格式。
        #[arg(
            long,
            default_value = "sk",
            value_parser = ["sk", "token", "simple"]
        )]
        api_key_format: String,
        /// 服务和首个 API key 允许使用的提供源，可重复或逗号分隔。
        #[arg(id = "provider", long = "provider", value_delimiter = ',')]
        providers: Vec<String>,
        /// 默认实例及多来源均衡的初始偏好；省略时使用 --provider 的第一个 ID，未指定来源时为 kiro。
        #[arg(long)]
        default_provider: Option<String>,
    },
    /// 修改服务监听、Kiro/Copilot 来源范围或 API key。
    #[command(
        after_help = "--provider 替换允许来源；原默认来源不在新范围时自动使用第一个 ID。--default-provider 可显式指定默认来源。--account-tag/账号手工绑定支持 Kiro/Copilot，修改标签前需停用服务。API key 参数接受 ID 或名称，可重复使用。\n\n示例：\n  crosslane service edit main --provider kiro --provider copilot --default-provider kiro\n  crosslane service edit main --add-api-key ci\n  crosslane service edit main --skip-user-agent-check true"
    )]
    Edit {
        /// 当前服务 ID 或名称。
        service: String,
        /// 新服务名称。
        #[arg(long)]
        rename: Option<String>,
        /// 新监听地址。
        #[arg(long)]
        host: Option<String>,
        /// 新监听端口，范围 1024..=65535。
        #[arg(long, value_parser = parse_service_port)]
        port: Option<u16>,
        /// 设置是否允许该服务的已认证请求跳过客户端 User-Agent 校验。
        #[arg(long, value_name = "BOOL", action = clap::ArgAction::Set)]
        skip_user_agent_check: Option<bool>,
        /// 修改服务的基础账号标签（可指定多个）；服务必须先停用。
        #[arg(long, num_args = 1.., value_delimiter = ',', value_name = "TAG", conflicts_with = "clear_account_tag")]
        account_tag: Vec<String>,
        /// 清除基础账号标签并恢复使用其全局账号池；服务必须先停用。
        #[arg(long)]
        clear_account_tag: bool,
        /// 增加绑定的 API key ID 或名称，可重复或逗号分隔。
        #[arg(long, value_delimiter = ',', value_name = "KEY")]
        add_api_key: Vec<String>,
        /// 移除绑定的 API key ID 或名称，可重复或逗号分隔。
        #[arg(long, value_delimiter = ',', value_name = "KEY")]
        remove_api_key: Vec<String>,
        /// 替换服务允许的提供源，可重复或逗号分隔。
        #[arg(id = "provider", long = "provider", value_delimiter = ',')]
        providers: Vec<String>,
        /// 清空显式范围并恢复为旧版 Kiro-only 语义。
        #[arg(long)]
        clear_providers: bool,
        /// 修改未带 provider 前缀的默认提供源。
        #[arg(long)]
        default_provider: Option<String>,
    },
    /// 启动已停用的 Kiro/Copilot API 代理服务。
    #[command(after_help = "示例：\n  crosslane service enable main")]
    Enable {
        /// 服务 ID 或名称。
        service: String,
    },
    /// 停止并停用 Kiro/Copilot API 代理服务，保留配置和 API key。
    #[command(after_help = "示例：\n  crosslane service disable main")]
    Disable {
        /// 服务 ID 或名称。
        service: String,
    },
    /// 删除 Kiro/Copilot 服务；级联删除未共享的 API key。
    #[command(
        name = "delete",
        visible_alias = "rm",
        after_help = "示例：\n  crosslane service delete main\n  crosslane service rm svc_abcd\n\n执行前需输入 y 或 yes 确认。"
    )]
    Delete {
        /// 服务 ID 或名称。
        service: String,
        /// 跳过交互确认，用于自动化。
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// 查看服务绑定的 API key，默认隐藏密钥明文。
    #[command(
        name = "apikeys",
        visible_alias = "apikey",
        after_help = "默认隐藏密钥明文；需要复制完整密钥时显式使用 --show-secret。--redact 作为兼容选项保留。\n\n示例：\n  crosslane service apikey main\n  crosslane service apikeys svc_abcd --show-secret"
    )]
    ApiKeys {
        /// 服务 ID 或名称。
        service: String,
        /// 显示完整 API key 明文。
        #[arg(long, alias = "show-secrets", conflicts_with = "redact")]
        show_secret: bool,
        /// 隐藏 API key 明文；兼容旧命令，现为默认行为。
        #[arg(long)]
        redact: bool,
    },
    /// 查看服务的 Kiro/Copilot 有效账号池。
    #[command(
        after_help = "显示服务内两种来源的有效账号，可用 --provider 筛选。\n\n示例：\n  crosslane service accounts main"
    )]
    Accounts {
        /// 服务 ID 或名称。
        service: String,
        #[arg(long)]
        provider: Option<String>,
    },
    /// 手工向服务账号池加入 Kiro/Copilot 账号；账号可使用任意标签。
    #[command(
        name = "add-account",
        after_help = "两种来源均支持；裸 ID 自动解析归属，重名时使用 provider/account_id 或 --provider。\n\n示例：\n  crosslane service add-account main acc_7f3a2b1c\n  crosslane service add-account main user@example.com"
    )]
    AddAccount {
        /// 服务 ID 或名称。
        service: String,
        /// 一个或多个账号 ID/邮箱。
        #[arg(required = true, num_args = 1.., value_name = "ID_OR_EMAIL")]
        accounts: Vec<String>,
        #[arg(long)]
        provider: Option<String>,
    },
    /// 从服务账号池排除 Kiro/Copilot 账号。
    #[command(
        name = "remove-account",
        after_help = "两种来源均支持；裸 ID 自动解析归属，重名时使用 provider/account_id 或 --provider。\n\n示例：\n  crosslane service remove-account main acc_7f3a2b1c"
    )]
    RemoveAccount {
        /// 服务 ID 或名称。
        service: String,
        /// 一个或多个账号 ID/邮箱。
        #[arg(required = true, num_args = 1.., value_name = "ID_OR_EMAIL")]
        accounts: Vec<String>,
        #[arg(long)]
        provider: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ApiKeyCommand {
    /// 列出 Kiro/Copilot API key；可按来源筛选。
    #[command(
        after_help = "--provider 只显示允许该来源的 key，并仅统计该来源用量；省略时显示全部。\n\n示例：\n  crosslane apikey list --provider kiro\n  crosslane apikey list --provider copilot --detail\n  crosslane --json apikey list --detail"
    )]
    List {
        /// 展示每个 API key 的 token/credits 消耗明细。
        #[arg(long)]
        detail: bool,
        /// 只显示允许该提供源的 key，并只统计该来源用量。
        #[arg(long)]
        provider: Option<String>,
    },
    /// 显示 key 的允许来源、模型和累计用量，不显示明文。
    #[command(
        after_help = "参数接受 API key ID 或名称。\n\n示例：\n  crosslane apikey show ci\n  crosslane --json apikey show ak_ab12"
    )]
    Show {
        /// 客户端 Key 的稳定 ID 或名称，见 apikey list。
        id: String,
    },
    /// 创建 API key；明文只在创建结果中显示一次。
    #[command(
        visible_alias = "create",
        after_help = "默认随机生成密钥，省略 --provider 时仅允许 Kiro。使用 --key 可恢复误删的原密钥；注意命令行参数可能进入 shell 历史和进程列表。\n\n示例：\n  crosslane apikey add --name kiro-key --provider kiro\n  crosslane apikey add --name copilot-key --provider copilot\n  crosslane apikey add --name unified-key --provider kiro --provider copilot\n  crosslane apikey add --name team --credits-limit 100"
    )]
    Add {
        /// 客户端 Key 的可读名称。
        #[arg(long)]
        name: String,
        /// 生成的密钥格式：sk- 前缀、token_ 前缀或无前缀。
        #[arg(
            long,
            default_value = "sk",
            value_parser = ["sk", "token", "simple"]
        )]
        format: String,
        /// 使用指定的 API key 明文，而不是随机生成。
        #[arg(long, value_name = "API_KEY")]
        key: Option<String>,
        /// 此 Key 跨全部来源共用的累计 Credits 上限；省略时不限。
        #[arg(long, value_parser = parse_non_negative_credits)]
        credits_limit: Option<f64>,
        /// 是否允许该 key 在所有已绑定服务上跳过客户端 User-Agent 校验。
        #[arg(
            long,
            value_name = "BOOL",
            action = clap::ArgAction::Set,
            default_value_t = false
        )]
        skip_user_agent_check: bool,
        /// 允许使用的提供源，可重复或逗号分隔；省略时仅允许 Kiro。
        #[arg(id = "provider", long = "provider", value_delimiter = ',')]
        providers: Vec<String>,
        /// 允许的模型 glob，可使用 provider/model 形式。
        #[arg(long = "model", value_delimiter = ',')]
        models: Vec<String>,
    },
    /// 修改 API key 的 Kiro/Copilot 来源和模型权限。
    #[command(
        after_help = "--provider 替换允许来源；--clear-providers 恢复 Kiro-only。--model 可使用 provider/model glob；参数接受 key ID 或名称。\n\n示例：\n  crosslane apikey edit ci --provider copilot --model 'copilot/*'\n  crosslane apikey edit ci --provider kiro --provider copilot\n  crosslane apikey edit ci --skip-user-agent-check true"
    )]
    Edit {
        /// 客户端 Key 的稳定 ID 或名称，见 apikey list。
        id: String,
        /// 设置是否允许该 key 跳过客户端 User-Agent 校验。
        #[arg(long, value_name = "BOOL", action = clap::ArgAction::Set)]
        skip_user_agent_check: Option<bool>,
        /// 替换允许使用的提供源。
        #[arg(id = "provider", long = "provider", value_delimiter = ',')]
        providers: Vec<String>,
        /// 清除来源权限配置，恢复仅允许 Kiro。
        #[arg(long)]
        clear_providers: bool,
        /// 替换允许的模型 glob。
        #[arg(long = "model", value_delimiter = ',', conflicts_with = "clear_models")]
        models: Vec<String>,
        /// 清除模型范围限制，允许权限范围内的全部模型。
        #[arg(long)]
        clear_models: bool,
    },
    /// 删除 API key，执行前需输入 y 或 yes 确认。
    #[command(
        visible_alias = "delete",
        after_help = "参数接受 API key ID 或名称。\n\n示例：\n  crosslane apikey rm ak_ab12\n  crosslane apikey delete ci\n\n执行前需输入 y 或 yes 确认。"
    )]
    Rm {
        /// 客户端 Key 的稳定 ID 或名称，见 apikey list。
        id: String,
        /// 跳过交互确认，用于自动化。
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// 启用 API key。
    #[command(
        after_help = "示例：\n  crosslane apikey enable ak_ab12\n  crosslane --json apikey enable ak_ab12"
    )]
    Enable {
        /// 客户端 Key 的稳定 ID 或名称，见 apikey list。
        id: String,
    },
    /// 停用 API key，但保留配置与历史用量。
    #[command(
        after_help = "示例：\n  crosslane apikey disable ak_ab12\n  crosslane --json apikey disable ak_ab12"
    )]
    Disable {
        /// 客户端 Key 的稳定 ID 或名称，见 apikey list。
        id: String,
    },
    /// 设置或清除 API key 跨来源共用的累计 Credits 上限。
    #[command(
        after_help = "此上限作用于同一 key 的 Kiro 与 Copilot 累计用量，不按来源分别配置。`--clear` 恢复不限；`--credits 0` 阻止新消耗。\n\n示例：\n  crosslane apikey limit ci --credits 100\n  crosslane apikey limit ci --clear"
    )]
    Limit {
        /// 客户端 Key 的稳定 ID 或名称。
        id: String,
        /// 跨全部来源共用的累计 Credits 上限，0 阻止新消耗。
        #[arg(
            long,
            required_unless_present = "clear",
            conflicts_with = "clear",
            value_parser = parse_non_negative_credits
        )]
        credits: Option<f64>,
        /// 删除累计 credits 上限，恢复为不限。
        #[arg(long)]
        clear: bool,
    },
    /// 查看 API key 的 Kiro/Copilot 聚合与分来源用量。
    #[command(
        after_help = "省略 --provider 时统计全部来源；可按 kiro 或 copilot 限定。\n\n示例：\n  crosslane apikey usage ak_ab12 --provider kiro\n  crosslane apikey usage ak_ab12 --provider copilot"
    )]
    Usage {
        /// 客户端 Key 的稳定 ID 或名称。
        id: String,
        /// 只统计该提供源实例；省略或 all 聚合全部来源。
        #[arg(long)]
        provider: Option<String>,
    },
    /// 查看 API key 的 Kiro/Copilot 最近请求历史。
    #[command(
        after_help = "省略 --provider 时显示全部来源。\n\n示例：\n  crosslane apikey history ak_ab12 --provider kiro\n  crosslane apikey history ak_ab12 --provider copilot --tail 200"
    )]
    History {
        /// 客户端 Key 的稳定 ID 或名称。
        id: String,
        /// 最多显示多少条最近请求，范围 1..=1000。
        #[arg(long, default_value_t = 50, value_parser = parse_cli_tail)]
        tail: usize,
        /// 只查询该提供源实例；省略或 all 显示全部来源。
        #[arg(long)]
        provider: Option<String>,
    },
    /// 清除 API key 在 Kiro/Copilot 的全部累计用量，需确认。
    #[command(
        after_help = "示例：\n  crosslane apikey reset-usage ak_ab12\n\n执行前需输入 y 或 yes 确认。"
    )]
    ResetUsage {
        /// 客户端 Key 的稳定 ID 或名称。
        id: String,
        /// 跳过交互确认，用于自动化。
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum AlertEvent {
    /// Kiro 账号触发剩余额度保护并暂停调度。
    AccountCreditProtected,
    /// Kiro 账号额度完全耗尽。
    AccountQuotaExhausted,
    /// 服务的全部启用 Kiro 账号额度完全耗尽。
    ServiceQuotaExhausted,
    /// Kiro 账号 Token 自动或请求触发刷新失败。
    TokenRefreshFailed,
}

impl AlertEvent {
    fn as_str(self) -> &'static str {
        match self {
            Self::AccountCreditProtected => "account-credit-protected",
            Self::AccountQuotaExhausted => "account-quota-exhausted",
            Self::ServiceQuotaExhausted => "service-quota-exhausted",
            Self::TokenRefreshFailed => "token-refresh-failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum AlertPlatform {
    /// 钉钉群机器人 Webhook。
    Dingtalk,
    /// 企业微信群机器人 Webhook。
    #[value(name = "wechat-work")]
    WechatWork,
    /// 飞书群机器人 Webhook。
    Feishu,
    /// Telegram Bot API。
    Telegram,
    /// Discord Webhook。
    Discord,
    /// 自定义 Webhook，可配置消息模板。
    Custom,
}

impl AlertPlatform {
    fn as_str(self) -> &'static str {
        match self {
            Self::Dingtalk => "dingtalk",
            Self::WechatWork => "wechat-work",
            Self::Feishu => "feishu",
            Self::Telegram => "telegram",
            Self::Discord => "discord",
            Self::Custom => "custom",
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum AlertCommand {
    /// 查看 Kiro 一次性异常告警策略。
    #[command(after_help = "示例：\n  crosslane alert config")]
    Config,
    /// 列出 Kiro 可订阅事件及其触发条件。
    #[command(after_help = "示例：\n  crosslane alert events\n  crosslane --json alert events")]
    Events,
    /// 列出支持的通知平台及平台专用参数。
    #[command(
        after_help = "示例：\n  crosslane alert platforms\n  crosslane --json alert platforms"
    )]
    Platforms,
    /// 列出全部告警通知目标。
    #[command(after_help = "示例：\n  crosslane alert list\n  crosslane --json alert list")]
    List,
    /// 为 Kiro 异常事件添加告警目标。
    #[command(
        after_help = "`--platform` 表示接收 Webhook 的通知平台；先用 `crosslane alert platforms` 查看平台说明。\n用 `crosslane alert events` 查看事件说明。多选可重复传入 --event，也可使用逗号分隔。\n\n示例：\n  crosslane alert add --name alerts --platform dingtalk --webhook-url 'https://oapi.dingtalk.com/robot/send?access_token=replace-me' --dingtalk-sign 'SEC-replace-me' --event account-credit-protected --event account-quota-exhausted\n  crosslane alert add --name alerts --platform feishu --webhook-url https://example/hook --event account-credit-protected,account-quota-exhausted,service-quota-exhausted"
    )]
    Add {
        /// 告警目标的唯一名称。
        #[arg(long)]
        name: String,
        /// Webhook 接收平台。
        #[arg(long = "platform", value_name = "PLATFORM")]
        platform: AlertPlatform,
        /// Webhook 接收地址。
        #[arg(long = "webhook-url", value_name = "URL")]
        webhook_url: String,
        /// 要订阅的异常事件；可重复传入或使用逗号分隔。
        #[arg(
            long = "event",
            value_delimiter = ',',
            required = true,
            value_name = "EVENT"
        )]
        events: Vec<AlertEvent>,
        /// 创建目标但暂不启用。
        #[arg(long)]
        disabled: bool,
        /// 钉钉机器人加签密钥；仅启用加签时需要。
        #[arg(long)]
        dingtalk_sign: Option<String>,
        /// Telegram 目标的 chat ID；platform=telegram 时必填。
        #[arg(long)]
        telegram_chat_id: Option<String>,
        /// 自定义 Webhook 消息模板；支持 {{event}}、{{title}}、{{message}}。
        #[arg(long)]
        custom_template: Option<String>,
    },
    /// 编辑 Kiro 异常事件告警目标。
    #[command(
        after_help = "目标名称既可写成位置参数，也可通过 --name 指定。\n`--event` 会整体替换原订阅；可重复传入或使用逗号分隔。\n\n示例：\n  crosslane alert edit alerts --webhook-url https://example/new-hook\n  crosslane alert edit alerts --dingtalk-sign 'SEC-replace-me'\n  crosslane alert edit --name alerts --event token-refresh-failed --event service-quota-exhausted\n  crosslane alert edit --name alerts --platform feishu"
    )]
    Edit {
        /// 当前名称；也可使用 --name。
        #[arg(value_name = "NAME", required_unless_present = "name")]
        target: Option<String>,
        /// 当前名称；与位置参数 NAME 二选一。
        #[arg(long, value_name = "NAME", conflicts_with = "target")]
        name: Option<String>,
        /// 修改目标名称。
        #[arg(long)]
        rename: Option<String>,
        /// 修改 Webhook 接收平台。
        #[arg(long = "platform", value_name = "PLATFORM")]
        platform: Option<AlertPlatform>,
        /// 修改 Webhook 接收地址。
        #[arg(long = "webhook-url", value_name = "URL")]
        webhook_url: Option<String>,
        /// 整体替换要订阅的异常事件；可重复传入或使用逗号分隔。
        #[arg(
            long = "event",
            value_delimiter = ',',
            conflicts_with = "clear_events",
            value_name = "EVENT"
        )]
        events: Vec<AlertEvent>,
        /// 清空事件订阅；目标将不再接收告警。
        #[arg(long)]
        clear_events: bool,
        /// 启用目标。
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        /// 停用目标，但保留配置。
        #[arg(long, conflicts_with = "enable")]
        disable: bool,
        /// 设置钉钉机器人加签密钥。
        #[arg(long, conflicts_with = "clear_dingtalk_sign")]
        dingtalk_sign: Option<String>,
        /// 删除钉钉机器人加签密钥。
        #[arg(long)]
        clear_dingtalk_sign: bool,
        /// 设置 Telegram chat ID。
        #[arg(long, conflicts_with = "clear_telegram_chat_id")]
        telegram_chat_id: Option<String>,
        /// 删除 Telegram chat ID。
        #[arg(long)]
        clear_telegram_chat_id: bool,
        /// 设置自定义 Webhook 消息模板。
        #[arg(long, conflicts_with = "clear_custom_template")]
        custom_template: Option<String>,
        /// 删除自定义 Webhook 消息模板。
        #[arg(long)]
        clear_custom_template: bool,
    },
    /// 删除告警目标，执行前需输入 y 或 yes 确认。
    #[command(
        name = "delete",
        visible_alias = "rm",
        after_help = "删除通知目标，事件定义和历史投递记录保留。\n\n示例：\n  crosslane alert delete alerts\n  crosslane alert rm alerts --yes"
    )]
    Delete {
        /// 告警目标名称，见 alert list。
        name: String,
        /// 跳过交互确认，用于自动化。
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// 向一个或全部目标发送测试通知。
    #[command(
        after_help = "示例：\n  crosslane alert test alerts\n  crosslane alert test --all",
        group(clap::ArgGroup::new("alert_test_target").required(true).multiple(false).args(["name", "all"]))
    )]
    Test {
        /// 告警目标名称；与 --all 二选一。
        name: Option<String>,
        /// 向全部已配置目标发送测试通知。
        #[arg(long, conflicts_with = "name")]
        all: bool,
    },
    /// 查看最近的告警投递记录。
    #[command(after_help = "示例：\n  crosslane alert logs\n  crosslane alert logs --tail 200")]
    Logs {
        /// 最多显示多少条投递记录，范围 1..=1000。
        #[arg(long, default_value_t = 50, value_parser = parse_cli_tail)]
        tail: usize,
    },
}

pub(crate) fn parse_non_negative_credits(value: &str) -> std::result::Result<f64, String> {
    let credits = value
        .parse::<f64>()
        .map_err(|_| "额度必须是数字".to_owned())?;
    if credits.is_finite() && credits >= 0.0 {
        Ok(credits)
    } else {
        Err("额度必须是有限的非负数".to_owned())
    }
}

fn parse_cli_tail(value: &str) -> std::result::Result<usize, String> {
    let tail = value
        .parse::<usize>()
        .map_err(|_| "条数必须是整数".to_owned())?;
    if (1..=1_000).contains(&tail) {
        Ok(tail)
    } else {
        Err("条数必须在 1..=1000 之间".to_owned())
    }
}

fn parse_service_port(value: &str) -> std::result::Result<u16, String> {
    let port = value
        .parse::<u16>()
        .map_err(|_| "端口必须是 1024..=65535 的整数".to_owned())?;
    if port >= 1024 {
        Ok(port)
    } else {
        Err("端口必须在 1024..=65535 之间".to_owned())
    }
}

pub async fn simple_rpc(
    client: &mut AdminClient,
    method_name: &str,
    params: serde_json::Value,
    json: bool,
) -> Result<()> {
    let value: serde_json::Value = client.call(method_name, params).await?;
    if json {
        print_json(&value)?;
    } else if method_name == method::TASKS {
        let tasks = value
            .as_object()
            .ok_or_else(|| anyhow!("daemon 返回的任务列表无效"))?;
        let rows = tasks
            .iter()
            .map(|(name, task)| {
                vec![
                    name.clone(),
                    crate::cli::scope::task_scope(name).into(),
                    if crate::cli::scope::MANUAL_TASKS.contains(&name.as_str()) {
                        "是".into()
                    } else {
                        "否（仅调度器）".into()
                    },
                    task["interval_ms"]
                        .as_u64()
                        .map(|ms| format!("{ms} ms"))
                        .unwrap_or_else(|| "-".into()),
                    task["run"]["last_run_at"]
                        .as_i64()
                        .map(format_timestamp)
                        .unwrap_or_else(|| "尚未执行".into()),
                    task["run"]["last_result"].as_str().unwrap_or("-").into(),
                ]
            })
            .collect::<Vec<_>>();
        println!(
            "{}",
            render_table(
                &["任务", "适用范围", "可手工运行", "周期", "上次执行", "结果"],
                &rows
            )
        );
    } else {
        print_human_value(&value);
    }
    Ok(())
}

pub fn print_diagnose_all(
    endpoints: &serde_json::Value,
    accounts: &serde_json::Value,
    json: bool,
) -> Result<()> {
    if json {
        return print_json(&serde_json::json!({
            "endpoints": endpoints,
            "accounts": accounts
        }));
    }
    println!("端点诊断");
    print_human_value(endpoints);
    println!("\n账号诊断");
    print_human_value(accounts);
    Ok(())
}

#[derive(Debug, Deserialize)]
struct PoolOutput {
    model: String,
    #[serde(default)]
    queued: usize,
    #[serde(default)]
    accounts: Vec<PoolAccountOutput>,
    #[serde(default)]
    scoring: Option<PoolScoringOutput>,
}

#[derive(Debug, Deserialize)]
struct PoolScoringOutput {
    weight_active: f64,
    weight_credit: f64,
    weight_idle: f64,
    max_concurrent_per_account: usize,
    idle_window_ms: u64,
}

#[derive(Debug, Deserialize)]
struct PoolAccountOutput {
    account_id: String,
    #[serde(default)]
    account_name: String,
    score: Option<f64>,
    #[serde(default)]
    active_factor: f64,
    #[serde(default)]
    credit_factor: f64,
    #[serde(default)]
    idle_factor: f64,
    #[serde(default)]
    eligible: bool,
    #[serde(default)]
    reason: String,
}

pub async fn show_pool(
    client: &mut AdminClient,
    provider: &str,
    model: &str,
    explain: bool,
    json: bool,
    watch: bool,
) -> Result<()> {
    if provider != "kiro" {
        let value: serde_json::Value = client
            .call(
                method::V2_ACCOUNT_LIST,
                serde_json::json!({"provider":provider}),
            )
            .await?;
        let accounts = value["accounts"]
            .as_array()
            .ok_or_else(|| anyhow!("daemon 返回的 provider 账号池无效"))?;
        let errors = value["errors"].as_object();
        let complete = value["complete"].as_bool().unwrap_or(true)
            && errors.is_none_or(|errors| errors.is_empty());
        if json {
            let snapshot = serde_json::json!({
                "provider":provider,
                "model":model,
                "accounts":accounts,
                "complete":complete,
                "errors":errors.cloned().unwrap_or_default()
            });
            if watch {
                print_json_line(&snapshot)?;
            } else {
                print_json(&snapshot)?;
            }
        } else {
            let rows = accounts
                .iter()
                .map(|account| {
                    let models = account["supported_models"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(serde_json::Value::as_str)
                        .collect::<Vec<_>>();
                    let enabled = account["enabled"].as_bool().unwrap_or(false);
                    let supports = if model.is_empty() {
                        !models.is_empty()
                    } else {
                        models.contains(&model)
                    };
                    let healthy = account["health"] == "available";
                    vec![
                        account["provider_id"].as_str().unwrap_or("-").into(),
                        crate::cli::scope::kind_name(
                            account["provider_kind"].as_str().unwrap_or("-"),
                        )
                        .into(),
                        account["id"].as_str().unwrap_or("-").into(),
                        account["display_name"].as_str().unwrap_or("-").into(),
                        if enabled && supports && healthy {
                            "可调度"
                        } else {
                            "不可调度"
                        }
                        .into(),
                        account["auth_state"].as_str().unwrap_or("unknown").into(),
                        if models.is_empty() {
                            "-".into()
                        } else {
                            models.join(",")
                        },
                    ]
                })
                .collect::<Vec<_>>();
            println!(
                "提供源 {provider}  模型 {}  账号 {}",
                if model.is_empty() { "全部" } else { model },
                rows.len()
            );
            println!(
                "{}",
                render_table(
                    &[
                        "实例 ID",
                        "类型 (kind)",
                        "账号",
                        "名称",
                        "状态",
                        "认证",
                        "已发现模型"
                    ],
                    &rows
                )
            );
            for (provider, error) in errors.into_iter().flatten() {
                eprintln!("{provider}: {}", error.as_str().unwrap_or("查询失败"));
            }
        }
        if !complete {
            return Err(anyhow!("部分提供源的账号池查询失败"));
        }
        return Ok(());
    }
    let model = if model.is_empty() {
        "minimax-m2.5"
    } else {
        model
    };
    let value: serde_json::Value = client
        .call(method::POOL, serde_json::json!({"model":model}))
        .await?;
    if json {
        return if watch {
            print_json_line(&value)
        } else {
            print_json(&value)
        };
    }
    let output =
        serde_json::from_value::<PoolOutput>(value).context("daemon 返回的账号池评分无效")?;
    print!("{}", render_pool_output(&output, explain));
    Ok(())
}

fn render_pool_output(pool: &PoolOutput, explain: bool) -> String {
    let eligible = pool
        .accounts
        .iter()
        .filter(|account| account.eligible)
        .count();
    let unavailable = pool.accounts.len().saturating_sub(eligible);
    let mut output = format!(
        "模型 {}  排队 {}  可调度 {}/{}\n",
        pool.model,
        pool.queued,
        eligible,
        pool.accounts.len()
    );

    let mut unavailable_by_reason = BTreeMap::<&str, usize>::new();
    for account in pool.accounts.iter().filter(|account| !account.eligible) {
        *unavailable_by_reason
            .entry(account.reason.as_str())
            .or_default() += 1;
    }
    if !unavailable_by_reason.is_empty() {
        output.push_str("不可调度：");
        output.push_str(
            &unavailable_by_reason
                .into_iter()
                .map(|(reason, count)| format!("{} {count}", pool_reason_label(reason)))
                .collect::<Vec<_>>()
                .join("，"),
        );
        output.push('\n');
    }
    output.push('\n');

    let has_names = pool
        .accounts
        .iter()
        .any(|account| !account.account_name.trim().is_empty());
    if explain {
        let rows = pool
            .accounts
            .iter()
            .enumerate()
            .map(|(index, account)| {
                let mut row = vec![
                    if account.eligible {
                        (index + 1).to_string()
                    } else {
                        "-".into()
                    },
                    account.account_id.clone(),
                ];
                if has_names {
                    row.push(short_pool_name(&account.account_name));
                }
                row.extend([
                    if account.eligible {
                        "可调度".into()
                    } else {
                        pool_reason_label(&account.reason).into()
                    },
                    format_pool_score(account.score),
                    format_pool_factor(account.active_factor, account.eligible),
                    format_pool_factor(account.credit_factor, account.eligible),
                    format_pool_factor(account.idle_factor, account.eligible),
                ]);
                row
            })
            .collect::<Vec<_>>();
        let mut headers = vec!["排名", "账号"];
        if has_names {
            headers.push("名称");
        }
        headers.extend(["状态", "评分", "并发", "额度", "近期使用"]);
        output.push_str(&render_table(&headers, &rows));
    } else {
        let rows = pool
            .accounts
            .iter()
            .filter(|account| account.eligible)
            .enumerate()
            .map(|(index, account)| {
                let mut row = vec![(index + 1).to_string(), account.account_id.clone()];
                if has_names {
                    row.push(short_pool_name(&account.account_name));
                }
                row.push(format_pool_score(account.score));
                row
            })
            .collect::<Vec<_>>();
        if rows.is_empty() {
            output.push_str("暂无可调度账号。\n");
        } else {
            let mut headers = vec!["排名", "账号"];
            if has_names {
                headers.push("名称");
            }
            headers.push("评分");
            output.push_str(&render_table(&headers, &rows));
        }
        if unavailable > 0 {
            output.push_str(&format!(
                "提示：已折叠 {unavailable} 个不可调度账号；使用 --explain 查看详情。\n"
            ));
        }
    }
    output.push_str(&render_pool_score_help(pool.scoring.as_ref(), explain));
    output
}

fn render_pool_score_help(scoring: Option<&PoolScoringOutput>, explain: bool) -> String {
    if !explain {
        return "评分说明：越低越优；综合并发压力、额度使用率和近期使用情况计算。使用 --explain 查看公式。\n".into();
    }

    let Some(scoring) = scoring else {
        return "\n评分说明：越低越优；综合并发压力、额度使用率和近期使用情况计算。\n".into();
    };
    let concurrent_baseline = if scoring.max_concurrent_per_account == 0 {
        "10（未设置单账号上限时的归一化基准）".into()
    } else {
        scoring.max_concurrent_per_account.to_string()
    };
    format!(
        "\n评分说明：越低越优；评分 = 并发 × {} + 额度 × {} + 近期使用 × {}。\n\
         因子说明：并发 = 活跃请求数 ÷ {concurrent_baseline}；额度 = 已用额度 ÷ 总额度；近期使用 = 刚使用时 100%，空闲 {}后降为 0%。\n\
         调度说明：完全同分时会加入极小随机量打破平局。\n",
        format_pool_weight(scoring.weight_active),
        format_pool_weight(scoring.weight_credit),
        format_pool_weight(scoring.weight_idle),
        format_pool_duration(scoring.idle_window_ms),
    )
}

fn format_pool_weight(weight: f64) -> String {
    if weight.is_finite() {
        format!("{weight:.3}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned()
    } else {
        "-".into()
    }
}

fn format_pool_duration(duration_ms: u64) -> String {
    if duration_ms > 0 && duration_ms.is_multiple_of(60_000) {
        format!("{} 分钟", duration_ms / 60_000)
    } else if duration_ms > 0 && duration_ms.is_multiple_of(1_000) {
        format!("{} 秒", duration_ms / 1_000)
    } else {
        format!("{duration_ms} 毫秒")
    }
}

fn format_pool_score(score: Option<f64>) -> String {
    score
        .filter(|score| score.is_finite())
        .map(|score| format!("{score:.4}"))
        .unwrap_or_else(|| "-".into())
}

fn format_pool_factor(factor: f64, eligible: bool) -> String {
    if eligible && factor.is_finite() {
        format!("{:.1}%", factor * 100.0)
    } else {
        "-".into()
    }
}

fn pool_reason_label(reason: &str) -> &'static str {
    match reason {
        "disabled" => "已停用",
        "exhausted" => "额度耗尽",
        "low_credit" => "低额度保护",
        "cooling" => "冷却中",
        "banned" => "已封禁",
        "refreshing" => "刷新中",
        "model_unavailable" => "模型不支持",
        "available" | "" => "不可调度",
        _ => "其他",
    }
}

fn short_pool_name(name: &str) -> String {
    const MAX_CHARS: usize = 28;
    let mut characters = name.chars();
    let prefix = characters.by_ref().take(MAX_CHARS).collect::<String>();
    if characters.next().is_some() {
        format!("{prefix}…")
    } else if prefix.is_empty() {
        "-".into()
    } else {
        prefix
    }
}

mod config_redaction;
mod observability;

use observability::print_human_value;
#[cfg(test)]
use observability::{
    host_log_path, log_account, log_model_route, populate_host_log_paths, LogModelRoute,
};
pub use observability::{
    parse_duration, parse_timestamp, show_log_files, show_logs, show_stats, show_trace_logs,
};

mod alert;

pub use alert::{run_alert, show_alert_config, show_alert_events, show_alert_platforms};

mod apikey;

use apikey::format_credits;
pub use apikey::run_apikey;
#[cfg(test)]
use apikey::{apikey_list_json, ApiKeyListEntry, ApiKeyListSummary};

pub async fn run_service(
    client: &mut AdminClient,
    command: ServiceCommand,
    json: bool,
) -> Result<()> {
    match command {
        ServiceCommand::List { provider } => {
            let result: ProxyServiceListResult = client
                .call(
                    method::SERVICE_LIST,
                    serde_json::json!({"provider":provider}),
                )
                .await?;
            if json {
                print_json(&result)?;
            } else if result.services.is_empty() {
                println!("暂无 API 代理服务。使用 `crosslane service create --name <名称>` 创建。");
            } else {
                let rows = result
                    .services
                    .into_iter()
                    .map(|service| {
                        vec![
                            service.id,
                            service.name,
                            format!("{}:{}", service.host, service.port),
                            if service.running {
                                "运行中".into()
                            } else if service.enabled {
                                "异常".into()
                            } else {
                                "已停用".into()
                            },
                            service.api_key_ids.len().to_string(),
                            service_tag_label(
                                &service.account_tags,
                                service.account_tag.as_deref(),
                            ),
                            if service.allowed_providers.is_empty() {
                                "kiro".into()
                            } else {
                                service.allowed_providers.join(",")
                            },
                            if service.default_provider.is_empty() {
                                "kiro".into()
                            } else {
                                service.default_provider
                            },
                            service.error.unwrap_or_default(),
                        ]
                    })
                    .collect::<Vec<_>>();
                println!(
                    "{}",
                    render_table(
                        &[
                            "ID",
                            "名称",
                            "监听",
                            "状态",
                            "API Keys",
                            "账号池",
                            "提供源",
                            "默认源",
                            "错误",
                        ],
                        &rows,
                    )
                );
            }
            Ok(())
        }
        ServiceCommand::Show { service } => show_service(client, &service, json).await,
        ServiceCommand::Create {
            name,
            host,
            port,
            account_tag,
            skip_user_agent_check,
            api_key_name,
            api_key_format,
            providers,
            default_provider,
        } => {
            let account_tag = normalize_service_tags(&account_tag)?;
            let account_tag = match account_tag.as_slice() {
                [] => serde_json::Value::Null,
                [tag] => serde_json::json!(tag),
                tags => serde_json::json!(tags),
            };
            let result: ProxyServiceCreateResult = client
                .call(
                    method::SERVICE_CREATE,
                    serde_json::json!({
                        "name":name,
                        "host":host,
                        "port":port,
                        "account_tag":account_tag,
                        "skip_user_agent_check":skip_user_agent_check,
                        "api_key_name":api_key_name,
                        "api_key_format":api_key_format,
                        "allowed_providers":providers,
                        "default_provider":default_provider
                    }),
                )
                .await?;
            if json {
                print_json(&result)?;
            } else {
                if result.service.running {
                    println!(
                        "已创建并启动 {} ({})，监听 {}:{}",
                        result.service.id,
                        result.service.name,
                        result.service.host,
                        result.service.port
                    );
                } else {
                    println!(
                        "已创建 {} ({})，业务监听未启动；预期地址 {}:{}",
                        result.service.id,
                        result.service.name,
                        result.service.host,
                        result.service.port
                    );
                }
                println!(
                    "已创建默认 API key {} ({})：\n{}\n可用 `crosslane service apikey {} --show-secret` 再次查看。",
                    result.api_key.id, result.api_key.name, result.api_key.key, result.service.id
                );
            }
            Ok(())
        }
        ServiceCommand::Edit {
            service,
            rename,
            host,
            port,
            skip_user_agent_check,
            account_tag,
            clear_account_tag,
            add_api_key,
            remove_api_key,
            providers,
            clear_providers,
            default_provider,
        } => {
            if rename.is_none()
                && host.is_none()
                && port.is_none()
                && skip_user_agent_check.is_none()
                && account_tag.is_empty()
                && !clear_account_tag
                && add_api_key.is_empty()
                && remove_api_key.is_empty()
                && providers.is_empty()
                && !clear_providers
                && default_provider.is_none()
            {
                return Err(anyhow!(
                    "没有指定修改项；请使用 --rename、--host、--port、--skip-user-agent-check、--account-tag、--clear-account-tag、--add-api-key、--remove-api-key、--provider 或 --default-provider"
                ));
            }
            let account_tag = normalize_service_tags(&account_tag)?;
            let result_selector = rename.clone().unwrap_or_else(|| service.clone());
            mutate_config(client, |config| {
                // Resolve names only after the config file lock has been
                // acquired and the latest TOML snapshot has been read. This
                // prevents a concurrent rename/delete from leaving this edit
                // with selector results derived from an older runtime snapshot.
                let add_api_key = resolve_api_key_ids(config, &add_api_key)?;
                let remove_api_key = resolve_api_key_ids(config, &remove_api_key)?;
                let array = config
                    .entry("proxy_service")
                    .or_insert_with(|| toml::Value::Array(Vec::new()))
                    .as_array_mut()
                    .ok_or_else(|| anyhow!("proxy_service must be an array of tables"))?;
                let table = find_service_table_mut(array, &service)?;
                replace_optional_string(table, "name", rename.as_deref());
                replace_optional_string(table, "host", host.as_deref());
                if let Some(port) = port {
                    table.insert("port".into(), toml::Value::Integer(i64::from(port)));
                }
                if let Some(skip) = skip_user_agent_check {
                    table.insert("skip_user_agent_check".into(), toml::Value::Boolean(skip));
                }
                if clear_account_tag {
                    table.remove("account_tag");
                } else if let Some(value) = service_tag_toml_value(&account_tag) {
                    table.insert("account_tag".into(), value);
                }
                update_service_provider_scope(
                    table,
                    &providers,
                    clear_providers,
                    default_provider.as_deref(),
                );
                let key_ids = table
                    .entry("api_key_ids")
                    .or_insert_with(|| toml::Value::Array(Vec::new()))
                    .as_array_mut()
                    .ok_or_else(|| anyhow!("proxy service api_key_ids must be an array"))?;
                for key_id in add_api_key {
                    if !key_ids.iter().any(|value| value.as_str() == Some(&key_id)) {
                        key_ids.push(toml::Value::String(key_id));
                    }
                }
                key_ids.retain(|value| {
                    !value
                        .as_str()
                        .is_some_and(|id| remove_api_key.iter().any(|removed| removed == id))
                });
                Ok(())
            })
            .await?;
            if json {
                show_service(client, &result_selector, true).await
            } else {
                println!("已更新 API 代理服务 {result_selector}");
                Ok(())
            }
        }
        ServiceCommand::Enable { service } => {
            set_service_enabled(client, &service, true).await?;
            if json {
                show_service(client, &service, true).await
            } else {
                println!("已启用 API 代理服务 {service}");
                Ok(())
            }
        }
        ServiceCommand::Disable { service } => {
            set_service_enabled(client, &service, false).await?;
            if json {
                show_service(client, &service, true).await
            } else {
                println!("已停用 API 代理服务 {service}");
                Ok(())
            }
        }
        ServiceCommand::Delete { service, yes } => {
            if !crate::commands::confirm_unless(
                yes,
                &format!("确认删除 API 代理服务 {service} 及其专用 API key？"),
            )
            .await?
            {
                if json {
                    print_json(&serde_json::json!({
                        "service": service,
                        "deleted": false,
                        "cancelled": true
                    }))?;
                } else {
                    println!("已取消");
                }
                return Ok(());
            }
            let result: ProxyServiceDeleteResult = client
                .call(
                    method::SERVICE_DELETE,
                    serde_json::json!({"service":service}),
                )
                .await?;
            if json {
                print_json(&result)?;
            } else {
                println!(
                    "已停止并删除 API 代理服务 {} ({})，同时删除 {} 个专用 API key。",
                    result.service_id,
                    result.service_name,
                    result.deleted_api_key_ids.len()
                );
                if !result.retained_api_key_ids.is_empty() {
                    println!(
                        "{} 个由其他服务共享的 API key 已保留：{}",
                        result.retained_api_key_ids.len(),
                        result.retained_api_key_ids.join(",")
                    );
                }
            }
            Ok(())
        }
        ServiceCommand::ApiKeys {
            service,
            show_secret,
            redact: _,
        } => {
            let result: ProxyServiceApiKeysResult = client
                .call(
                    method::SERVICE_APIKEYS,
                    serde_json::json!({"service":service,"show_secret":show_secret}),
                )
                .await?;
            if json {
                print_json(&result)?;
            } else if result.api_keys.is_empty() {
                println!(
                    "API 代理服务 {} ({}) 未绑定 API key。",
                    result.service_id, result.service_name
                );
            } else {
                println!("服务 {} ({})", result.service_name, result.service_id);
                let rows = result
                    .api_keys
                    .into_iter()
                    .map(|key| {
                        vec![
                            key.id,
                            key.name,
                            key.format,
                            if key.enabled {
                                "已启用"
                            } else {
                                "已停用"
                            }
                            .into(),
                            if key.user_agent_check_enforced {
                                "校验".into()
                            } else {
                                format!("跳过 ({})", key.user_agent_check_reason)
                            },
                            key.credits_limit
                                .map(format_credits)
                                .unwrap_or_else(|| "-".into()),
                            key.key.unwrap_or_else(|| "<隐藏>".into()),
                        ]
                    })
                    .collect::<Vec<_>>();
                println!(
                    "{}",
                    render_table(
                        &[
                            "ID",
                            "名称",
                            "格式",
                            "状态",
                            "User-Agent",
                            "Credits 上限",
                            "API Key",
                        ],
                        &rows
                    )
                );
            }
            Ok(())
        }
        ServiceCommand::Accounts { service, provider } => {
            let result: ProxyServiceAccountsResult = client
                .call(
                    method::SERVICE_ACCOUNTS,
                    serde_json::json!({"service":service,"provider":provider}),
                )
                .await?;
            print_service_accounts(&result, json)
        }
        ServiceCommand::AddAccount {
            service,
            accounts,
            provider,
        } => {
            let result: ProxyServiceAccountsResult = client
                .call(
                    method::SERVICE_ACCOUNT_ADD,
                    serde_json::json!({"service":service,"accounts":accounts,"provider":provider}),
                )
                .await?;
            print_service_accounts(&result, json)
        }
        ServiceCommand::RemoveAccount {
            service,
            accounts,
            provider,
        } => {
            let result: ProxyServiceAccountsResult = client
                .call(
                    method::SERVICE_ACCOUNT_REMOVE,
                    serde_json::json!({"service":service,"accounts":accounts,"provider":provider}),
                )
                .await?;
            print_service_accounts(&result, json)
        }
    }
}

fn normalize_service_tags(tags: &[String]) -> Result<Vec<String>> {
    let mut normalized = tags
        .iter()
        .map(|tag| tag.trim().to_owned())
        .collect::<Vec<_>>();
    if normalized.iter().any(String::is_empty) {
        return Err(anyhow!("账号标签不能为空"));
    }
    normalized.sort();
    normalized.dedup();
    Ok(normalized)
}

fn service_tag_toml_value(tags: &[String]) -> Option<toml::Value> {
    match tags {
        [] => None,
        [tag] => Some(toml::Value::String(tag.clone())),
        tags => Some(toml::Value::Array(
            tags.iter().cloned().map(toml::Value::String).collect(),
        )),
    }
}

fn service_tag_label(tags: &[String], legacy_tag: Option<&str>) -> String {
    let tags = if tags.is_empty() {
        legacy_tag.into_iter().collect::<Vec<_>>()
    } else {
        tags.iter().map(String::as_str).collect::<Vec<_>>()
    };
    if tags.is_empty() {
        "global".into()
    } else {
        format!("tag:{}", tags.join(","))
    }
}

fn print_service_accounts(result: &ProxyServiceAccountsResult, json: bool) -> Result<()> {
    if json {
        return print_json(result);
    }
    println!(
        "账号池    {}",
        service_tag_label(&result.account_tags, result.account_tag.as_deref())
    );
    println!(
        "手工加入  {}",
        if result.account_ids.is_empty() {
            "-".into()
        } else {
            result.account_ids.join(",")
        }
    );
    println!(
        "显式排除  {}",
        if result.excluded_account_ids.is_empty() {
            "-".into()
        } else {
            result.excluded_account_ids.join(",")
        }
    );
    if !result.provider_accounts.is_empty() {
        let rows = result
            .provider_accounts
            .iter()
            .map(|account| {
                vec![
                    account.provider_id.clone(),
                    account.id.clone(),
                    account.display_name.clone(),
                    account.tags.join(","),
                    account.health.clone(),
                ]
            })
            .collect::<Vec<_>>();
        println!(
            "{}",
            render_table(&["PROVIDER", "ID", "账号", "标签", "状态"], &rows)
        );
        return Ok(());
    }
    if result.accounts.is_empty() {
        println!(
            "API 代理服务 {} ({}) 当前没有有效账号。",
            result.service_id, result.service_name
        );
        return Ok(());
    }
    let rows = result
        .accounts
        .iter()
        .map(|account| {
            vec![
                account.id.clone(),
                account.email.clone(),
                if account.tags.is_empty() {
                    "-".into()
                } else {
                    account.tags.join(",")
                },
                account.health.clone().unwrap_or_else(|| "-".into()),
            ]
        })
        .collect::<Vec<_>>();
    println!("{}", render_table(&["ID", "邮箱", "标签", "状态"], &rows));
    Ok(())
}

async fn show_service(client: &mut AdminClient, selector: &str, json: bool) -> Result<()> {
    let result: ProxyServiceListResult = client
        .call(method::SERVICE_LIST, serde_json::json!({}))
        .await?;
    let service = result
        .services
        .into_iter()
        .find(|service| service.id == selector || service.name == selector)
        .ok_or_else(|| anyhow!("proxy service not found: {selector}"))?;
    if json {
        return print_json(&service);
    }
    println!("ID        {}", service.id);
    println!("名称      {}", service.name);
    println!("监听      {}:{}", service.host, service.port);
    println!(
        "UA 校验   {}",
        if service.skip_user_agent_check {
            "该服务跳过"
        } else {
            "继承全局/API key 策略"
        }
    );
    println!(
        "配置状态  {}",
        if service.enabled {
            "已启用"
        } else {
            "已停用"
        }
    );
    println!(
        "运行状态  {}",
        if service.running {
            "运行中"
        } else {
            "未运行"
        }
    );
    println!(
        "API Keys  {}",
        if service.api_key_ids.is_empty() {
            "-".into()
        } else {
            service.api_key_ids.join(",")
        }
    );
    println!(
        "账号池    {}",
        service_tag_label(&service.account_tags, service.account_tag.as_deref())
    );
    println!(
        "手工账号  {}",
        if service.account_ids.is_empty() {
            "-".into()
        } else {
            service.account_ids.join(",")
        }
    );
    println!(
        "提供源    {} (默认 {})",
        if service.allowed_providers.is_empty() {
            "kiro".into()
        } else {
            service.allowed_providers.join(",")
        },
        if service.default_provider.is_empty() {
            "kiro"
        } else {
            &service.default_provider
        }
    );
    println!(
        "排除账号  {}",
        if service.excluded_account_ids.is_empty() {
            "-".into()
        } else {
            service.excluded_account_ids.join(",")
        }
    );
    if let Some(error) = service.error.filter(|error| !error.is_empty()) {
        println!("错误      {error}");
    }
    Ok(())
}

async fn set_service_enabled(
    client: &mut AdminClient,
    selector: &str,
    enabled: bool,
) -> Result<()> {
    mutate_config_array(client, "proxy_service", |array| {
        find_service_table_mut(array, selector)?
            .insert("enabled".into(), toml::Value::Boolean(enabled));
        Ok(())
    })
    .await
}

fn update_service_provider_scope(
    table: &mut toml::map::Map<String, toml::Value>,
    providers: &[String],
    clear: bool,
    default_provider: Option<&str>,
) {
    if clear {
        table.remove("allowed_providers");
    } else if !providers.is_empty() {
        table.insert("allowed_providers".into(), string_array_value(providers));
    }
    if let Some(default_provider) = default_provider {
        table.insert(
            "default_provider".into(),
            toml::Value::String(default_provider.into()),
        );
    } else if clear || !providers.is_empty() {
        let allowed = if clear {
            &["kiro".to_owned()][..]
        } else {
            providers
        };
        let current = table
            .get("default_provider")
            .and_then(toml::Value::as_str)
            .filter(|id| !id.is_empty())
            .unwrap_or("kiro");
        // Preserve an allowed default; otherwise follow service create's first-ID rule.
        if !allowed.iter().any(|id| id == current) {
            table.insert(
                "default_provider".into(),
                toml::Value::String(allowed[0].clone()),
            );
        }
    }
}

fn find_service_table_mut<'a>(
    array: &'a mut [toml::Value],
    selector: &str,
) -> Result<&'a mut toml::map::Map<String, toml::Value>> {
    array
        .iter_mut()
        .find(|value| {
            value.as_table().is_some_and(|table| {
                table.get("id").and_then(toml::Value::as_str) == Some(selector)
                    || table.get("name").and_then(toml::Value::as_str) == Some(selector)
            })
        })
        .and_then(toml::Value::as_table_mut)
        .ok_or_else(|| anyhow!("proxy service not found: {selector}"))
}

fn resolve_api_key_ids(
    config: &toml::map::Map<String, toml::Value>,
    selectors: &[String],
) -> Result<Vec<String>> {
    if selectors.is_empty() {
        return Ok(Vec::new());
    }
    let api_keys = config
        .get("api_key")
        .and_then(toml::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut ids = Vec::with_capacity(selectors.len());
    for selector in selectors {
        let key = api_keys
            .iter()
            .filter_map(toml::Value::as_table)
            .find(|key| {
                key.get("id").and_then(toml::Value::as_str) == Some(selector)
                    || key.get("name").and_then(toml::Value::as_str) == Some(selector)
            })
            .ok_or_else(|| anyhow!("API key not found: {selector}"))?;
        let id = key
            .get("id")
            .and_then(toml::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("API key {selector} does not have a stable ID"))?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

async fn show_keys(
    client: &mut AdminClient,
    selected: Option<&str>,
    tail: Option<usize>,
    provider: Option<&str>,
    json: bool,
) -> Result<()> {
    let mut value: serde_json::Value = client
        .call(
            method::APIKEY_LIST,
            serde_json::json!({"provider":provider}),
        )
        .await?;
    if let Some(id) = selected {
        let entry = value
            .as_array_mut()
            .and_then(|array| {
                array
                    .iter()
                    .position(|item| item["id"] == id || item["name"] == id)
                    .map(|index| array.remove(index))
            })
            .ok_or_else(|| anyhow!("API key not found: {id}"))?;
        value = if let Some(tail) = tail {
            let history = entry["usage"]["history"]
                .as_array()
                .map(|items| items.iter().rev().take(tail).cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            serde_json::json!({"id":entry["id"],"name":entry["name"],"history":history})
        } else {
            entry
        };
    }
    if json {
        print_json(&value)?;
    } else {
        println!("{}", serde_json::to_string_pretty(&value)?);
    }
    Ok(())
}

async fn mutate_key_and_reload(
    client: &mut AdminClient,
    id: &str,
    field: &str,
    value: toml::Value,
) -> Result<()> {
    mutate_config_array(client, "api_key", |array| {
        let table = array
            .iter_mut()
            .find(|item| matches_key(item, id))
            .and_then(toml::Value::as_table_mut)
            .ok_or_else(|| anyhow!("API key not found: {id}"))?;
        table.insert(field.into(), value);
        Ok(())
    })
    .await
}

async fn clear_key_field_and_reload(client: &mut AdminClient, id: &str, field: &str) -> Result<()> {
    mutate_config_array(client, "api_key", |array| {
        let table = array
            .iter_mut()
            .find(|item| matches_key(item, id))
            .and_then(toml::Value::as_table_mut)
            .ok_or_else(|| anyhow!("API key not found: {id}"))?;
        table.remove(field);
        Ok(())
    })
    .await
}

async fn mutate_config_array(
    client: &mut AdminClient,
    section: &str,
    mutate: impl FnOnce(&mut Vec<toml::Value>) -> Result<()>,
) -> Result<()> {
    mutate_config(client, |table| {
        let array = table
            .entry(section)
            .or_insert_with(|| toml::Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| anyhow!("{section} must be an array of tables"))?;
        mutate(array)
    })
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn add_provider(
    client: &mut AdminClient,
    id: &str,
    kind: &str,
    settings: &[String],
    max_concurrent_per_account: Option<usize>,
    default_model: Option<&str>,
    disabled: bool,
    json: bool,
) -> Result<()> {
    let settings = parse_provider_settings(settings)?;
    mutate_config_array(client, "provider", |array| {
        if array.is_empty() && id != "kiro" {
            array.push(provider_table("kiro", "kiro", true));
        }
        if array.iter().any(|value| provider_value_matches(value, id)) {
            return Err(anyhow!("provider already exists: {id}"));
        }
        let mut value = provider_table(id, kind, !disabled);
        let table = value.as_table_mut().expect("provider table");
        merge_new_provider_settings(table, &settings)?;
        if let Some(limit) = max_concurrent_per_account {
            let limit = i64::try_from(limit).context("并发上限过大")?;
            table.insert(
                "pool".into(),
                toml::Value::Table(toml::map::Map::from_iter([(
                    "max_concurrent_per_account".into(),
                    toml::Value::Integer(limit),
                )])),
            );
        }
        if let Some(model) = default_model {
            table.insert(
                "routing".into(),
                toml::Value::Table(toml::map::Map::from_iter([(
                    "default_model_id".into(),
                    toml::Value::String(model.into()),
                )])),
            );
        }
        array.push(value);
        Ok(())
    })
    .await?;
    if json {
        print_json(&serde_json::json!({"provider":id,"created":true}))
    } else {
        println!(
            "已添加提供源 {id}（{}）",
            crate::cli::scope::kind_name(kind)
        );
        crate::cli::scope::print_next_steps(id, kind);
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn edit_provider(
    client: &mut AdminClient,
    id: &str,
    settings: &[String],
    remove_settings: &[String],
    max_concurrent_per_account: Option<usize>,
    default_model: Option<&str>,
    enable_model_fallback: Option<bool>,
    allow_cross_provider_fallback: Option<bool>,
    failover_timeout_ms: Option<u64>,
    json: bool,
) -> Result<()> {
    if settings.is_empty()
        && remove_settings.is_empty()
        && max_concurrent_per_account.is_none()
        && default_model.is_none()
        && enable_model_fallback.is_none()
        && allow_cross_provider_fallback.is_none()
        && failover_timeout_ms.is_none()
    {
        return Err(anyhow!(
            "没有指定修改项；请使用 --setting、--remove-setting、--max-concurrent-per-account、--default-model、--enable-model-fallback、--allow-cross-provider-fallback 或 --failover-timeout-ms"
        ));
    }
    let settings = parse_provider_settings(settings)?;
    mutate_config_array(client, "provider", |array| {
        materialize_implicit_kiro(array, id);
        let table = array
            .iter_mut()
            .find(|value| provider_value_matches(value, id))
            .and_then(toml::Value::as_table_mut)
            .ok_or_else(|| anyhow!("provider not found: {id}"))?;
        if !settings.is_empty() || !remove_settings.is_empty() {
            let provider_settings = ensure_table(table, "settings")?;
            for key in remove_settings {
                provider_settings.remove(key);
            }
            provider_settings.extend(settings.clone());
        }
        if let Some(limit) = max_concurrent_per_account {
            ensure_table(table, "pool")?.insert(
                "max_concurrent_per_account".into(),
                toml::Value::Integer(i64::try_from(limit).context("并发上限过大")?),
            );
        }
        if let Some(model) = default_model {
            ensure_table(table, "routing")?
                .insert("default_model_id".into(), toml::Value::String(model.into()));
        }
        if let Some(enabled) = enable_model_fallback {
            ensure_table(table, "routing")?.insert(
                "enable_model_fallback".into(),
                toml::Value::Boolean(enabled),
            );
        }
        if let Some(enabled) = allow_cross_provider_fallback {
            ensure_table(table, "routing")?.insert(
                "allow_cross_provider_fallback".into(),
                toml::Value::Boolean(enabled),
            );
        }
        if let Some(timeout) = failover_timeout_ms {
            ensure_table(table, "routing")?.insert(
                "failover_timeout_ms".into(),
                toml::Value::Integer(i64::try_from(timeout).context("等待上限过大")?),
            );
        }
        Ok(())
    })
    .await?;
    if json {
        print_json(&serde_json::json!({"provider":id,"updated":true}))
    } else {
        println!("已更新提供源 {id}");
        Ok(())
    }
}

pub async fn set_provider_enabled(
    client: &mut AdminClient,
    id: &str,
    enabled: bool,
    json: bool,
) -> Result<()> {
    mutate_config_array(client, "provider", |array| {
        materialize_implicit_kiro(array, id);
        let table = array
            .iter_mut()
            .find(|value| provider_value_matches(value, id))
            .and_then(toml::Value::as_table_mut)
            .ok_or_else(|| anyhow!("provider not found: {id}"))?;
        table.insert("enabled".into(), toml::Value::Boolean(enabled));
        Ok(())
    })
    .await?;
    if json {
        print_json(&serde_json::json!({"provider":id,"enabled":enabled}))
    } else {
        println!("已{}提供源 {id}", if enabled { "启用" } else { "停用" });
        Ok(())
    }
}

pub async fn delete_provider(
    client: &mut AdminClient,
    id: &str,
    yes: bool,
    json: bool,
) -> Result<()> {
    if !crate::commands::confirm_unless(yes, &format!("确认删除提供源 {id} 的配置？")).await?
    {
        if json {
            return print_json(
                &serde_json::json!({"provider":id,"deleted":false,"cancelled":true}),
            );
        }
        println!("已取消");
        return Ok(());
    }
    mutate_config_array(client, "provider", |array| {
        remove_provider_config(array, id)
    })
    .await?;
    if json {
        print_json(&serde_json::json!({"provider":id,"deleted":true}))
    } else {
        println!("已删除提供源 {id} 的配置；账号文件未删除");
        Ok(())
    }
}

fn provider_table(id: &str, kind: &str, enabled: bool) -> toml::Value {
    let mut table = toml::map::Map::from_iter([
        ("id".into(), toml::Value::String(id.into())),
        ("kind".into(), toml::Value::String(kind.into())),
        ("enabled".into(), toml::Value::Boolean(enabled)),
    ]);
    if kind == "copilot" {
        table.insert(
            "settings".into(),
            toml::Value::Table(toml::map::Map::from_iter([(
                "client_id".into(),
                toml::Value::String(DEFAULT_COPILOT_OAUTH_CLIENT_ID.into()),
            )])),
        );
    }
    toml::Value::Table(table)
}

fn merge_new_provider_settings(
    table: &mut toml::map::Map<String, toml::Value>,
    settings: &toml::map::Map<String, toml::Value>,
) -> Result<()> {
    if !settings.is_empty() {
        ensure_table(table, "settings")?.extend(settings.clone());
    }
    Ok(())
}

fn provider_value_matches(value: &toml::Value, id: &str) -> bool {
    value
        .as_table()
        .and_then(|table| table.get("id"))
        .and_then(toml::Value::as_str)
        .is_some_and(|candidate| candidate == id)
}

fn materialize_implicit_kiro(array: &mut Vec<toml::Value>, id: &str) {
    if array.is_empty() && id == "kiro" {
        array.push(provider_table("kiro", "kiro", true));
    }
}

fn remove_provider_config(array: &mut Vec<toml::Value>, id: &str) -> Result<()> {
    let Some(index) = array
        .iter()
        .position(|value| provider_value_matches(value, id))
    else {
        if array.is_empty() && id == "kiro" {
            return Err(anyhow!(
                "implicit Kiro provider cannot be deleted; add another provider first"
            ));
        }
        return Err(anyhow!("provider not found: {id}"));
    };
    if array.len() == 1 {
        return Err(anyhow!(
            "the last configured provider cannot be deleted because an empty provider list enables legacy Kiro"
        ));
    }
    array.remove(index);
    Ok(())
}

fn parse_provider_settings(values: &[String]) -> Result<toml::map::Map<String, toml::Value>> {
    let mut output = toml::map::Map::new();
    for setting in values {
        let (key, raw) = setting
            .split_once('=')
            .ok_or_else(|| anyhow!("provider setting must use KEY=VALUE: {setting}"))?;
        let key = key.trim();
        if key.is_empty() || key.contains('.') {
            return Err(anyhow!(
                "provider setting key must be a non-empty direct key: {key}"
            ));
        }
        let document = format!("value = {raw}")
            .parse::<toml::Value>()
            .with_context(|| format!("provider setting {key} is not a TOML value"))?;
        let value = document
            .get("value")
            .cloned()
            .ok_or_else(|| anyhow!("provider setting {key} has no value"))?;
        output.insert(key.into(), value);
    }
    Ok(output)
}

fn ensure_table<'a>(
    table: &'a mut toml::map::Map<String, toml::Value>,
    key: &str,
) -> Result<&'a mut toml::map::Map<String, toml::Value>> {
    table
        .entry(key)
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow!("provider.{key} must be a table"))
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum OverageLimitUpdate {
    Preserve,
    Set(f64),
    Clear,
}

pub(crate) async fn update_pool_overage_config(
    client: &mut AdminClient,
    enabled: bool,
    limit: OverageLimitUpdate,
) -> Result<()> {
    mutate_config(client, |root| {
        apply_pool_overage_update(root, enabled, limit)
    })
    .await
}

fn apply_pool_overage_update(
    root: &mut toml::map::Map<String, toml::Value>,
    enabled: bool,
    limit: OverageLimitUpdate,
) -> Result<()> {
    let pool = root
        .entry("pool")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow!("pool must be a table"))?;
    pool.insert("enable_overage".into(), toml::Value::Boolean(enabled));
    match limit {
        OverageLimitUpdate::Preserve => {}
        OverageLimitUpdate::Set(value) => {
            if !value.is_finite() || value < 0.0 {
                return Err(anyhow!(
                    "overage credits must be a finite non-negative number"
                ));
            }
            pool.insert(
                "max_overage_credits_per_account".into(),
                toml::Value::Float(value),
            );
        }
        OverageLimitUpdate::Clear => {
            pool.remove("max_overage_credits_per_account");
        }
    }
    Ok(())
}

async fn mutate_config(
    client: &mut AdminClient,
    mutate: impl FnOnce(&mut toml::map::Map<String, toml::Value>) -> Result<()>,
) -> Result<()> {
    let paths: ConfigPathResult = client
        .call(method::CONFIG_PATH, serde_json::json!({}))
        .await?;
    let path = std::path::PathBuf::from(paths.config_file);
    let _config_lock = crosslane_store::atomic::lock_file_exclusive(&path)
        .await
        .with_context(|| format!("锁定配置文件 {} 失败", path.display()))?;
    let raw = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("读取 {} 失败", path.display()))?;
    let before = raw.parse::<toml::Value>().context("配置文件 TOML 无效")?;
    let mut root = before.clone();
    let table = root
        .as_table_mut()
        .ok_or_else(|| anyhow!("config root must be a TOML table"))?;
    mutate(table)?;
    let output =
        crosslane_store::config_update::render_update_preserving_comments(&raw, &before, &root)
            .context("更新配置失败")?;
    let config: crosslane_core::config::Config =
        toml::from_str(&output).context("修改后的配置无法解析")?;
    config.validate().context("修改后的配置校验失败")?;
    crosslane_store::atomic::write_bytes_atomically(&path, output.as_bytes(), Some(0o600)).await?;
    let result = reload_config_while_locked(client).await?;
    if result.applied {
        return Ok(());
    }
    crosslane_store::atomic::write_bytes_atomically(&path, raw.as_bytes(), Some(0o600)).await?;
    let rollback = reload_config_while_locked(client).await?;
    if !rollback.applied {
        return Err(anyhow!(
            "配置重载失败且回滚后的配置也无法重载: {}",
            rollback.error.unwrap_or_else(|| "unknown".into())
        ));
    }
    Err(anyhow!(
        "配置重载失败，磁盘文件已回滚: {}",
        result.error.unwrap_or_else(|| "unknown".into())
    ))
}

fn string_array_value(values: &[String]) -> toml::Value {
    toml::Value::Array(values.iter().cloned().map(toml::Value::String).collect())
}

fn alert_event_array_value(events: &[AlertEvent]) -> toml::Value {
    toml::Value::Array(
        events
            .iter()
            .map(|event| toml::Value::String(event.as_str().into()))
            .collect(),
    )
}

fn integer_array_value(values: &[u32]) -> toml::Value {
    toml::Value::Array(
        values
            .iter()
            .map(|value| toml::Value::Integer(i64::from(*value)))
            .collect(),
    )
}

fn insert_optional_string(
    table: &mut toml::map::Map<String, toml::Value>,
    field: &str,
    value: Option<&str>,
) {
    if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
        table.insert(field.into(), toml::Value::String(value.into()));
    }
}

fn replace_optional_string(
    table: &mut toml::map::Map<String, toml::Value>,
    field: &str,
    value: Option<&str>,
) {
    if let Some(value) = value {
        table.insert(field.into(), toml::Value::String(value.into()));
    }
}

fn replace_or_clear_optional_string(
    table: &mut toml::map::Map<String, toml::Value>,
    field: &str,
    value: Option<&str>,
    clear: bool,
) {
    if clear {
        table.remove(field);
    } else {
        replace_optional_string(table, field, value);
    }
}

fn named_value_matches(value: &toml::Value, name: &str) -> bool {
    value
        .as_table()
        .and_then(|table| table.get("name"))
        .and_then(toml::Value::as_str)
        == Some(name)
}

fn find_named_table_mut<'a>(
    array: &'a mut [toml::Value],
    name: &str,
    kind: &str,
) -> Result<&'a mut toml::map::Map<String, toml::Value>> {
    array
        .iter_mut()
        .find(|value| named_value_matches(value, name))
        .and_then(toml::Value::as_table_mut)
        .ok_or_else(|| anyhow!("{kind} not found: {name}"))
}

fn remove_named_value(array: &mut Vec<toml::Value>, name: &str, kind: &str) -> Result<()> {
    let before = array.len();
    array.retain(|value| !named_value_matches(value, name));
    if array.len() == before {
        return Err(anyhow!("{kind} not found: {name}"));
    }
    Ok(())
}

fn matches_key(value: &toml::Value, id: &str) -> bool {
    value.as_table().is_some_and(|table| {
        table.get("id").and_then(toml::Value::as_str) == Some(id)
            || table.get("name").and_then(toml::Value::as_str) == Some(id)
    })
}

fn generate_key(format: &str) -> Result<String> {
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    use std::fmt::Write as _;
    let random = bytes.iter().fold(String::new(), |mut output, byte| {
        let _ = write!(output, "{byte:02x}");
        output
    });
    match format {
        "sk" => Ok(format!("sk-{random}")),
        "token" => Ok(format!("token_{random}")),
        "simple" => Ok(random),
        other => Err(anyhow!("unsupported format: {other}")),
    }
}

fn key_id(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    digest[..8]
        .iter()
        .fold(String::from("ak_"), |mut output, byte| {
            use std::fmt::Write as _;
            let _ = write!(output, "{byte:02x}");
            output
        })
}

#[derive(Debug, Clone, Copy)]
struct ConfigModule {
    name: &'static str,
    key: &'static str,
    category: &'static str,
    scope: &'static str,
    description: &'static str,
    preferred_command: &'static str,
    aliases: &'static [&'static str],
    is_array: bool,
}

impl ConfigModule {
    fn resettable(&self) -> bool {
        !matches!(self.key, "provider" | "api_key" | "proxy_service")
    }

    fn scope_label(&self) -> &'static str {
        match self.scope {
            "global" => "全局",
            "kiro" => "Kiro",
            "mixed" => "混合",
            "provider" => "按来源",
            _ => unreachable!("unknown config scope"),
        }
    }
}

const CONFIG_MODULES: &[ConfigModule] = &[
    ConfigModule {
        name: "server",
        key: "server",
        category: "通用",
        scope: "global",
        description: "API 服务默认监听、准入、连接与 TLS",
        preferred_command: "-",
        aliases: &[],
        is_array: false,
    },
    ConfigModule {
        name: "upstream",
        key: "upstream",
        category: "来源参数",
        scope: "kiro",
        description: "Kiro 上游请求、重试、超时与连接池",
        preferred_command: "-",
        aliases: &[],
        is_array: false,
    },
    ConfigModule {
        name: "pool",
        key: "pool",
        category: "来源参数",
        scope: "kiro",
        description: "Kiro 账号池并发、排队、额度保护与选号",
        preferred_command: "-",
        aliases: &[],
        is_array: false,
    },
    ConfigModule {
        name: "features",
        key: "features",
        category: "来源参数",
        scope: "kiro",
        description: "Kiro 协议转换、工具、缓存与 thinking 开关",
        preferred_command: "-",
        aliases: &[],
        is_array: false,
    },
    ConfigModule {
        name: "models",
        key: "models",
        category: "来源参数",
        scope: "mixed",
        description: "全局模型禁用与 Kiro 模型发现/缓存；Copilot 缓存在 provider.models 配置",
        preferred_command: "model disable/enable",
        aliases: &[],
        is_array: false,
    },
    ConfigModule {
        name: "tasks",
        key: "tasks",
        category: "周期任务",
        scope: "mixed",
        description: "Kiro token/状态任务与全局统计持久化间隔",
        preferred_command: "-",
        aliases: &[],
        is_array: false,
    },
    ConfigModule {
        name: "context",
        key: "context",
        category: "来源参数",
        scope: "kiro",
        description: "Kiro 上下文限制、压缩与工具保护",
        preferred_command: "-",
        aliases: &[],
        is_array: false,
    },
    ConfigModule {
        name: "storage",
        key: "storage",
        category: "来源参数",
        scope: "kiro",
        description: "Kiro 账号库存储参数",
        preferred_command: "-",
        aliases: &[],
        is_array: false,
    },
    ConfigModule {
        name: "notify",
        key: "notify",
        category: "告警",
        scope: "kiro",
        description: "Kiro 告警兼容配置",
        preferred_command: "crosslane alert config",
        aliases: &["alert", "alerts"],
        is_array: false,
    },
    ConfigModule {
        name: "log",
        key: "log",
        category: "通用",
        scope: "global",
        description: "日志级别、格式、路径与保留策略",
        preferred_command: "-",
        aliases: &["logging"],
        is_array: false,
    },
    ConfigModule {
        name: "admin",
        key: "admin",
        category: "通用",
        scope: "global",
        description: "本地管理面 socket",
        preferred_command: "-",
        aliases: &[],
        is_array: false,
    },
    ConfigModule {
        name: "sso",
        key: "sso",
        category: "来源参数",
        scope: "kiro",
        description: "Kiro IAM Identity Center SSO 默认入口",
        preferred_command: "-",
        aliases: &[],
        is_array: false,
    },
    ConfigModule {
        name: "model-mapping",
        key: "model_mapping",
        category: "规则",
        scope: "provider",
        description: "按提供源、服务和 API key 生效的模型映射规则",
        preferred_command: "crosslane model-map",
        aliases: &["model-map"],
        is_array: true,
    },
    ConfigModule {
        name: "model-thinking-mode",
        key: "model_thinking_mode",
        category: "规则",
        scope: "kiro",
        description: "Kiro 模型级 thinking 默认开关",
        preferred_command: "-",
        aliases: &["thinking"],
        is_array: false,
    },
    ConfigModule {
        name: "webhook",
        key: "webhook",
        category: "告警",
        scope: "kiro",
        description: "Kiro 异常事件告警投递目标",
        preferred_command: "crosslane alert",
        aliases: &["webhooks"],
        is_array: true,
    },
    ConfigModule {
        name: "provider",
        key: "provider",
        category: "基础服务",
        scope: "provider",
        description: "Kiro、Copilot 实例及各自驱动设置、模型缓存和路由",
        preferred_command: "crosslane provider",
        aliases: &["providers"],
        is_array: true,
    },
    ConfigModule {
        name: "api-key",
        key: "api_key",
        category: "基础服务",
        scope: "provider",
        description: "客户端凭据、来源/模型权限与额度限制",
        preferred_command: "crosslane apikey",
        aliases: &["apikey"],
        is_array: true,
    },
    ConfigModule {
        name: "proxy-service",
        key: "proxy_service",
        category: "基础服务",
        scope: "provider",
        description: "代理监听实例、提供源范围及 API key 绑定",
        preferred_command: "crosslane service",
        aliases: &["service"],
        is_array: true,
    },
];

pub fn list_config_modules(json: bool) -> Result<()> {
    if json {
        let modules = CONFIG_MODULES
            .iter()
            .map(|module| {
                serde_json::json!({
                    "name":module.name,
                    "toml_key":module.key,
                    "category":module.category,
                    "scope":module.scope,
                    "description":module.description,
                    "preferred_command":(module.preferred_command != "-")
                        .then_some(module.preferred_command),
                    "aliases":module.aliases,
                    "resettable":module.resettable(),
                })
            })
            .collect::<Vec<_>>();
        return print_json(&modules);
    }

    let rows = CONFIG_MODULES
        .iter()
        .map(|module| {
            vec![
                module.name.to_string(),
                module.category.to_string(),
                module.scope_label().to_string(),
                module.description.to_string(),
                if module.resettable() { "是" } else { "否" }.to_string(),
                module.preferred_command.to_string(),
            ]
        })
        .collect::<Vec<_>>();
    print!(
        "{}",
        render_table(
            &["模块", "类型", "适用来源", "说明", "可重置", "推荐管理命令"],
            &rows
        )
    );
    Ok(())
}

pub async fn show_config(
    client: &mut AdminClient,
    module: Option<&str>,
    effective: bool,
    show_secrets: bool,
    json: bool,
) -> Result<()> {
    let module = module.map(resolve_config_module).transpose()?;
    let mut show: ConfigShowResult = client
        .call(method::CONFIG_SHOW, serde_json::json!({}))
        .await?;
    if !show_secrets {
        if effective {
            config_redaction::redact_effective(&mut show.effective_json);
        } else {
            config_redaction::redact(&mut show)?;
        }
    }
    let Some(module) = module else {
        if json {
            if effective {
                return print_json(&serde_json::json!({
                    "path": show.path,
                    "effective_json": show.effective_json,
                }));
            }
            return print_json(&show);
        }
        if effective {
            println!("{}", serde_json::to_string_pretty(&show.effective_json)?);
        } else {
            print!("{}", show.raw);
        }
        return Ok(());
    };

    if effective {
        let value = show
            .effective_json
            .get(module.key)
            .cloned()
            .ok_or_else(|| anyhow!("生效配置缺少模块 {}", module.name))?;
        if json {
            print_json(&serde_json::json!({
                "path":show.path,
                "module":module.name,
                "toml_key":module.key,
                "effective":value,
            }))?;
        } else {
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        return Ok(());
    }

    let raw = render_config_module_document(&show.raw, module)?;
    if json {
        print_json(&serde_json::json!({
            "path":show.path,
            "module":module.name,
            "toml_key":module.key,
            "raw":raw,
        }))?;
    } else {
        print!("{raw}");
    }
    Ok(())
}

fn resolve_config_module(name: &str) -> Result<&'static ConfigModule> {
    let normalized = name.trim().to_ascii_lowercase().replace('-', "_");
    CONFIG_MODULES
        .iter()
        .find(|module| {
            module.key == normalized
                || module.name.replace('-', "_") == normalized
                || module
                    .aliases
                    .iter()
                    .any(|alias| alias.replace('-', "_") == normalized)
        })
        .ok_or_else(|| anyhow!("未知配置模块 {name}；使用 `crosslane config list` 查看可用模块"))
}

fn render_config_module_document(raw: &str, module: &ConfigModule) -> Result<String> {
    let source = raw.parse::<DocumentMut>().context("配置文件 TOML 无效")?;
    let skeleton = if module.is_array {
        format!("{} = []\n", module.key)
    } else {
        format!("[{}]\n", module.key)
    };
    let mut output = skeleton
        .parse::<DocumentMut>()
        .context("无法生成配置模块模板")?;
    if let Some(item) = source.as_table().get(module.key) {
        output.as_table_mut().insert(module.key, item.clone());
    }
    Ok(output.to_string())
}

fn render_config_module_editor(raw: &str, module: &ConfigModule) -> Result<String> {
    let body = render_config_module_document(raw, module)?;
    Ok(format!(
        "# Crosslane 配置模块：{}（TOML key: {}）\n\
         # 只允许编辑本模块；保存后会合并回完整配置并进行整体校验。\n\
         # 删除整个模块不会保存；要恢复默认值，请保留模块 TOML key 并清空字段或数组。\n\n\
         {body}",
        module.name, module.key
    ))
}

fn merge_edited_config_module(
    original: &str,
    module: &ConfigModule,
    edited: &str,
) -> Result<String> {
    let before = original
        .parse::<toml::Value>()
        .context("原配置文件 TOML 无效")?;
    let edited = edited
        .parse::<toml::Value>()
        .with_context(|| format!("配置模块 {} 的 TOML 无效", module.name))?;
    let edited_table = edited
        .as_table()
        .ok_or_else(|| anyhow!("配置模块文件必须是 TOML table"))?;
    let extra = edited_table
        .keys()
        .filter(|key| key.as_str() != module.key)
        .cloned()
        .collect::<Vec<_>>();
    if !extra.is_empty() {
        return Err(anyhow!(
            "配置模块 {} 不能包含其他顶层模块：{}",
            module.name,
            extra.join(", ")
        ));
    }
    let value = edited_table.get(module.key).cloned().ok_or_else(|| {
        anyhow!(
            "配置模块 {} 缺少 TOML key `{}`；请保留该 key",
            module.name,
            module.key
        )
    })?;
    let mut after = before.clone();
    after
        .as_table_mut()
        .ok_or_else(|| anyhow!("config root must be a TOML table"))?
        .insert(module.key.into(), value);
    let output = crosslane_store::config_update::render_update_preserving_comments(
        original, &before, &after,
    )
    .context("合并配置模块失败")?;
    let config: crosslane_core::config::Config =
        toml::from_str(&output).context("合并后的配置无法解析")?;
    config.validate().context("合并后的配置校验失败")?;
    Ok(output)
}

/// Reloads the configuration while excluding concurrent `crosslane` and daemon
/// mutations of the same file.
pub async fn reload_config(client: &mut AdminClient) -> Result<ConfigReloadResult> {
    let paths: ConfigPathResult = client
        .call(method::CONFIG_PATH, serde_json::json!({}))
        .await?;
    let path = PathBuf::from(paths.config_file);
    let _config_lock = crosslane_store::atomic::lock_file_exclusive(&path)
        .await
        .with_context(|| format!("锁定配置文件 {} 失败", path.display()))?;
    reload_config_while_locked(client).await
}

async fn reload_config_while_locked(client: &mut AdminClient) -> Result<ConfigReloadResult> {
    client
        .call(method::CONFIG_RELOAD, serde_json::json!({}))
        .await
}

pub async fn validate_config(file: Option<&str>, json: bool) -> Result<()> {
    let path = file
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| Paths::from_env().config_file);
    validate_config_path(&path, file.is_some()).await?;
    if json {
        print_json(&serde_json::json!({
            "valid": true,
            "config_file": path
        }))
    } else {
        println!("配置有效：{}", path.display());
        Ok(())
    }
}

async fn validate_config_path(path: &Path, require_file: bool) -> Result<()> {
    if require_file {
        let metadata = tokio::fs::metadata(&path)
            .await
            .with_context(|| format!("配置文件不存在或无法访问：{}", path.display()))?;
        if !metadata.is_file() {
            return Err(anyhow!("配置路径不是文件：{}", path.display()));
        }
    }
    let config = crosslane_store::config_loader::load_config(path)
        .await
        .with_context(|| format!("读取或解析 {} 失败", path.display()))?;
    config
        .validate()
        .with_context(|| format!("{} 配置校验失败", path.display()))?;
    Ok(())
}

pub async fn edit_config(client: &mut AdminClient, module: Option<&str>, json: bool) -> Result<()> {
    if let Some(module) = module {
        return edit_config_module(client, resolve_config_module(module)?, json).await;
    }
    edit_full_config(client, json).await
}

async fn edit_full_config(client: &mut AdminClient, json: bool) -> Result<()> {
    let paths: ConfigPathResult = client
        .call(method::CONFIG_PATH, serde_json::json!({}))
        .await?;
    let path = std::path::PathBuf::from(paths.config_file);
    let _config_lock = crosslane_store::atomic::lock_file_exclusive(&path)
        .await
        .with_context(|| format!("锁定配置文件 {} 失败", path.display()))?;
    let original = tokio::fs::read(&path)
        .await
        .with_context(|| format!("读取 {} 失败", path.display()))?;
    run_editor(&path, json).await?;
    if let Err(error) = validate_config_path(&path, true).await {
        crosslane_store::atomic::write_bytes_atomically(&path, &original, Some(0o600)).await?;
        return Err(error.context("配置无效，磁盘文件已回滚"));
    }
    let changed = tokio::fs::read(&path).await? != original;
    let result = reload_edited_config(client, &path, &original).await?;
    if json {
        print_json(&serde_json::json!({
            "saved": true,
            "changed": changed,
            "config_file": path,
            "needs_restart": result.needs_restart,
        }))?;
    } else {
        println!("配置已保存并重载");
        print_restart_required(&result.needs_restart);
    }
    Ok(())
}

async fn edit_config_module(
    client: &mut AdminClient,
    module: &ConfigModule,
    json: bool,
) -> Result<()> {
    let paths: ConfigPathResult = client
        .call(method::CONFIG_PATH, serde_json::json!({}))
        .await?;
    let path = PathBuf::from(paths.config_file);
    let _config_lock = crosslane_store::atomic::lock_file_exclusive(&path)
        .await
        .with_context(|| format!("锁定配置文件 {} 失败", path.display()))?;
    let original = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("读取 {} 失败", path.display()))?;
    let editor_content = render_config_module_editor(&original, module)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(&format!("crosslane-config-{}-", module.name))
        .suffix(".toml")
        .tempfile()
        .context("创建配置模块临时文件失败")?;
    temporary
        .write_all(editor_content.as_bytes())
        .context("写入配置模块临时文件失败")?;
    temporary.flush().context("刷新配置模块临时文件失败")?;
    run_editor(temporary.path(), json).await?;
    let edited = tokio::fs::read_to_string(temporary.path())
        .await
        .context("读取编辑后的配置模块失败")?;
    let output = merge_edited_config_module(&original, module, &edited)?;
    if output == original {
        if json {
            print_json(&serde_json::json!({
                "saved": false,
                "changed": false,
                "module": module.name,
                "config_file": path,
                "needs_restart": [],
            }))?;
        } else {
            println!("配置模块 {} 未修改", module.name);
        }
        return Ok(());
    }

    crosslane_store::atomic::write_bytes_atomically(&path, output.as_bytes(), Some(0o600)).await?;
    let result = reload_edited_config(client, &path, original.as_bytes()).await?;
    if json {
        print_json(&serde_json::json!({
            "saved": true,
            "changed": true,
            "module": module.name,
            "config_file": path,
            "needs_restart": result.needs_restart,
        }))?;
    } else {
        println!("配置模块 {} 已保存并重载", module.name);
        print_restart_required(&result.needs_restart);
    }
    Ok(())
}

async fn run_editor(path: &Path, json: bool) -> Result<()> {
    let editor = resolve_editor()?;
    let mut command = tokio::process::Command::new(&editor.program);
    command.args(&editor.args).arg(path);
    if json {
        use std::os::fd::AsFd;
        command.stdout(std::process::Stdio::from(
            std::io::stderr().as_fd().try_clone_to_owned()?,
        ));
    }
    ensure_utf8_editor_locale(&mut command);
    let status = command.status().await.with_context(|| {
        format!(
            "启动编辑器 {} 失败；请确认命令已安装，或通过 $VISUAL/$EDITOR 指定编辑器",
            editor.program.display()
        )
    })?;
    if !status.success() {
        return Err(anyhow!("编辑器退出状态为 {status}"));
    }
    Ok(())
}

async fn reload_edited_config(
    client: &mut AdminClient,
    path: &Path,
    original: &[u8],
) -> Result<ConfigReloadResult> {
    let reload = reload_config_while_locked(client).await;
    if matches!(&reload, Ok(result) if result.applied) {
        return reload;
    }

    crosslane_store::atomic::write_bytes_atomically(path, original, Some(0o600)).await?;
    let rollback = reload_config_while_locked(client).await;
    match (reload, rollback) {
        (Ok(result), Ok(rollback)) if rollback.applied => Err(anyhow!(
            "配置重载失败，磁盘文件已回滚：{}",
            result.error.unwrap_or_else(|| "未知错误".into())
        )),
        (Err(error), Ok(rollback)) if rollback.applied => {
            Err(error.context("配置重载请求失败，磁盘文件已回滚"))
        }
        (Ok(result), Ok(rollback)) => Err(anyhow!(
            "配置重载失败，且回滚配置也无法重载：{}；回滚错误：{}",
            result.error.unwrap_or_else(|| "未知错误".into()),
            rollback.error.unwrap_or_else(|| "未知错误".into())
        )),
        (Err(error), Ok(rollback)) => Err(anyhow!(
            "配置重载请求失败，且回滚配置也无法重载：{error}；回滚错误：{}",
            rollback.error.unwrap_or_else(|| "未知错误".into())
        )),
        (Ok(result), Err(rollback_error)) => Err(anyhow!(
            "配置重载失败，且无法确认回滚配置已生效：{}；回滚错误：{rollback_error}",
            result.error.unwrap_or_else(|| "未知错误".into())
        )),
        (Err(error), Err(rollback_error)) => Err(anyhow!(
            "配置重载请求失败，且无法确认回滚配置已生效：{error}；回滚错误：{rollback_error}"
        )),
    }
}

fn print_restart_required(fields: &[String]) {
    for field in fields {
        println!("注意：{field} 需重启 crosslaned 才能生效");
    }
}

/// Successful `config reset` result.
pub struct ConfigResetResult {
    pub config_file: PathBuf,
    pub backup_file: PathBuf,
    pub module: Option<String>,
    pub needs_restart: Vec<String>,
}

/// Back up the configuration, reset one module or all resettable settings, and reload it.
pub async fn reset_config(
    client: &mut AdminClient,
    module: Option<&str>,
    yes: bool,
) -> Result<Option<ConfigResetResult>> {
    let module = module.map(resolve_config_module).transpose()?;
    if let Some(module) = module {
        if !module.resettable() {
            return Err(anyhow!(
                "配置模块 {} 属于基础服务资源，不能通过 config reset 清空；请使用 `{}` 显式管理，或执行完整 `crosslane uninstall`",
                module.name,
                module.preferred_command
            ));
        }
    }
    let paths: ConfigPathResult = client
        .call(method::CONFIG_PATH, serde_json::json!({}))
        .await?;
    let path = PathBuf::from(paths.config_file);
    let prompt = module.map_or_else(
        || {
            "确认将运行配置恢复为默认设置？Kiro 专项与全局参数会重置，提供源、API key、代理服务、模型禁用规则和告警配置会保留，模型映射会被清除"
                .to_string()
        },
        |module| {
            format!(
                "确认将配置模块 {} 恢复为默认设置？其他配置不会改动",
                module.name
            )
        },
    );
    if !crate::commands::confirm_unless(yes, &prompt).await? {
        return Ok(None);
    }

    let _config_lock = crosslane_store::atomic::lock_file_exclusive(&path)
        .await
        .with_context(|| format!("锁定配置文件 {} 失败", path.display()))?;
    let original = tokio::fs::read(&path)
        .await
        .with_context(|| format!("读取 {} 失败", path.display()))?;
    let raw = std::str::from_utf8(&original).context("当前配置不是有效的 UTF-8")?;
    let reset = match module {
        Some(module) => render_config_module_reset(raw, module)?,
        None => render_reset_config_preserving_resources_and_alerts(raw)?,
    };

    let backup_file = write_config_backup(&path, &original).await?;
    crosslane_store::atomic::write_bytes_atomically(&path, reset.as_bytes(), Some(0o600)).await?;
    let result: ConfigReloadResult = match reload_config_while_locked(client).await {
        Ok(result) => result,
        Err(reload_error) => {
            crosslane_store::atomic::write_bytes_atomically(&path, &original, Some(0o600)).await?;
            let rollback = reload_config_while_locked(client).await;
            return match rollback {
                Ok(rollback) if rollback.applied => Err(reload_error.context(format!(
                    "默认配置重载请求失败，磁盘文件已回滚；原配置备份位于 {}",
                    backup_file.display()
                ))),
                Ok(rollback) => Err(anyhow!(
                    "默认配置重载请求失败，且回滚后的配置也无法重载；原配置备份位于 {}：{}；回滚错误：{}",
                    backup_file.display(),
                    reload_error,
                    rollback.error.unwrap_or_else(|| "未知错误".into())
                )),
                Err(rollback_error) => Err(anyhow!(
                    "默认配置重载请求失败，且无法确认回滚配置已生效；原配置备份位于 {}：{}；回滚错误：{}",
                    backup_file.display(),
                    reload_error,
                    rollback_error
                )),
            };
        }
    };
    if result.applied {
        return Ok(Some(ConfigResetResult {
            config_file: path,
            backup_file,
            module: module.map(|module| module.name.to_string()),
            needs_restart: result.needs_restart,
        }));
    }

    crosslane_store::atomic::write_bytes_atomically(&path, &original, Some(0o600)).await?;
    let rollback = reload_config_while_locked(client).await?;
    if !rollback.applied {
        return Err(anyhow!(
            "默认配置重载失败，且回滚后的配置也无法重载；原配置备份位于 {}：{}",
            backup_file.display(),
            rollback.error.unwrap_or_else(|| "未知错误".into())
        ));
    }
    Err(anyhow!(
        "默认配置重载失败，磁盘文件已回滚；原配置备份位于 {}：{}",
        backup_file.display(),
        result.error.unwrap_or_else(|| "未知错误".into())
    ))
}

fn render_config_module_reset(raw: &str, module: &ConfigModule) -> Result<String> {
    if !module.resettable() {
        return Err(anyhow!(
            "配置模块 {} 属于基础服务资源，不能通过 config reset 清空",
            module.name
        ));
    }
    let before = raw.parse::<toml::Value>().context("当前配置 TOML 无效")?;
    let defaults = toml::Value::try_from(crosslane_core::config::Config::default())
        .context("内置默认配置无法序列化")?;
    let default_value = defaults
        .as_table()
        .and_then(|table| table.get(module.key))
        .cloned()
        .ok_or_else(|| anyhow!("内置默认配置缺少模块 {}", module.name))?;
    let mut after = before.clone();
    after
        .as_table_mut()
        .ok_or_else(|| anyhow!("config root must be a TOML table"))?
        .insert(module.key.into(), default_value);
    if module.key == "models" {
        preserve_model_denials(&before, &mut after);
    }
    let output =
        crosslane_store::config_update::render_update_preserving_comments(raw, &before, &after)
            .context("生成模块重置配置失败")?;
    let config: crosslane_core::config::Config =
        toml::from_str(&output).context("模块重置后的配置无法解析")?;
    config.validate().context("模块重置后的配置校验失败")?;
    Ok(output)
}

/// Renders defaults while retaining separately managed resources and alert settings.
fn render_reset_config_preserving_resources_and_alerts(raw: &str) -> Result<String> {
    const PRESERVED_SECTIONS: [&str; 5] =
        ["notify", "webhook", "provider", "api_key", "proxy_service"];

    let current = raw.parse::<toml::Value>().context("当前配置 TOML 无效")?;
    let current_table = current
        .as_table()
        .ok_or_else(|| anyhow!("config root must be a TOML table"))?;
    let defaults = crosslane_store::bootstrap::render_default_config(
        &crosslane_core::config::Config::default().admin.socket,
    );
    let before = defaults
        .parse::<toml::Value>()
        .context("内置默认配置无法解析")?;
    let mut after = before.clone();
    let after_table = after
        .as_table_mut()
        .ok_or_else(|| anyhow!("default config root must be a TOML table"))?;
    for section in PRESERVED_SECTIONS {
        if let Some(value) = current_table.get(section) {
            after_table.insert(section.into(), value.clone());
        }
    }
    preserve_model_denials(&current, &mut after);

    let output = crosslane_store::config_update::render_update_preserving_comments(
        &defaults, &before, &after,
    )
    .context("生成重置配置失败")?;
    let config: crosslane_core::config::Config =
        toml::from_str(&output).context("重置后的配置无法解析")?;
    config.validate().context("重置后的配置校验失败")?;
    Ok(output)
}

fn preserve_model_denials(before: &toml::Value, after: &mut toml::Value) {
    if let Some(disabled) = before
        .get("models")
        .and_then(|models| models.get("disabled"))
    {
        if let Some(models) = after.get_mut("models").and_then(toml::Value::as_table_mut) {
            models.insert("disabled".into(), disabled.clone());
        }
    }
}

async fn write_config_backup(path: &Path, contents: &[u8]) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("配置路径缺少文件名：{}", path.display()))?
        .to_string_lossy();
    for index in 0..10_000usize {
        let suffix = if index == 0 {
            ".bak".to_owned()
        } else {
            format!(".bak.{index}")
        };
        let candidate = path.with_file_name(format!("{file_name}{suffix}"));
        if crosslane_store::atomic::write_bytes_if_absent_atomically(
            &candidate,
            contents,
            Some(0o600),
        )
        .await?
        {
            return Ok(candidate);
        }
    }
    Err(anyhow!(
        "无法为 {} 分配备份文件名：已有备份数量过多",
        path.display()
    ))
}

const DEFAULT_EDITORS: [&str; 3] = ["vim", "vi", "nano"];
const UTF8_EDITOR_LOCALE: &str = "C.UTF-8";

#[derive(Debug, PartialEq, Eq)]
struct EditorCommand {
    program: PathBuf,
    args: Vec<String>,
}

fn resolve_editor() -> Result<EditorCommand> {
    if let Some(configured) = ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.trim().is_empty())
    {
        return parse_editor(&configured);
    }

    let path = std::env::var_os("PATH");
    let program = find_default_editor(path.as_deref()).ok_or_else(|| {
        anyhow!(
            "未找到可用编辑器（已尝试 {}）；请安装编辑器，或设置 $VISUAL/$EDITOR，例如 EDITOR=vim crosslane config edit",
            DEFAULT_EDITORS.join("、")
        )
    })?;
    Ok(EditorCommand {
        program,
        args: Vec::new(),
    })
}

fn parse_editor(configured: &str) -> Result<EditorCommand> {
    let mut parts = shlex::split(configured)
        .ok_or_else(|| anyhow!("$VISUAL/$EDITOR 存在未闭合的引号"))?
        .into_iter();
    let program = parts
        .next()
        .ok_or_else(|| anyhow!("$VISUAL/$EDITOR 不能为空"))?;
    Ok(EditorCommand {
        program: PathBuf::from(program),
        args: parts.collect(),
    })
}

fn ensure_utf8_editor_locale(command: &mut tokio::process::Command) {
    if editor_needs_utf8_locale(
        std::env::var_os("LC_ALL").as_deref(),
        std::env::var_os("LC_CTYPE").as_deref(),
        std::env::var_os("LANG").as_deref(),
    ) {
        command
            .env("LANG", UTF8_EDITOR_LOCALE)
            .env("LC_ALL", UTF8_EDITOR_LOCALE);
    }
}

fn editor_needs_utf8_locale(
    lc_all: Option<&std::ffi::OsStr>,
    lc_ctype: Option<&std::ffi::OsStr>,
    lang: Option<&std::ffi::OsStr>,
) -> bool {
    let effective = [lc_all, lc_ctype, lang]
        .into_iter()
        .flatten()
        .find(|value| !value.is_empty());
    effective.is_none_or(|value| {
        !value
            .to_string_lossy()
            .bytes()
            .filter(u8::is_ascii_alphanumeric)
            .map(|byte| byte.to_ascii_lowercase())
            .collect::<Vec<_>>()
            .windows(4)
            .any(|window| window == b"utf8")
    })
}

fn find_default_editor(path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let path = path?;
    DEFAULT_EDITORS.iter().find_map(|editor| {
        std::env::split_paths(path)
            .map(|directory| directory.join(editor))
            .find(|candidate| is_executable(candidate))
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

pub async fn set_models_enabled(
    client: &mut AdminClient,
    models: &[String],
    enabled: bool,
    json: bool,
) -> Result<()> {
    let capabilities: serde_json::Value = client
        .call(method::V2_CAPABILITIES, serde_json::json!({}))
        .await
        .context("读取 daemon 全局模型禁用能力失败，请确认 crosslane 和 crosslaned 均已更新")?;
    if !capabilities["features"].as_array().is_some_and(|features| {
        features.iter().any(|feature| {
            feature.as_str() == Some(crosslane_ipc::protocol::GLOBAL_MODEL_DISABLE_FEATURE)
        })
    }) {
        return Err(anyhow!(
            "运行中的 crosslaned 不支持全局模型禁用，请升级 daemon 后重试；未修改配置"
        ));
    }
    mutate_config(client, |root| {
        let mut validation: crosslane_core::config::Config =
            toml::Value::Table(root.clone()).try_into()?;
        validation.models.disabled = models.to_vec();
        validation.validate()?;
        let table = root
            .entry("models")
            .or_insert_with(|| toml::Value::Table(Default::default()))
            .as_table_mut()
            .ok_or_else(|| anyhow!("models must be a table"))?;
        let disabled = table
            .entry("disabled")
            .or_insert_with(|| toml::Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| anyhow!("models.disabled must be an array"))?;
        for model in models {
            let key = crosslane_core::model_policy::model_key(model);
            disabled.retain(|item| {
                item.as_str()
                    .is_none_or(|id| crosslane_core::model_policy::model_key(id) != key)
            });
            if !enabled {
                disabled.push(toml::Value::String(model.clone()));
            }
        }
        Ok(())
    })
    .await?;
    if json {
        print_json(
            &serde_json::json!({"models":models,"enabled":enabled,"scope":"all","applied":true}),
        )?;
    } else {
        println!(
            "{}模型（全部 provider，已生效）：{}",
            if enabled { "已启用" } else { "已禁用" },
            models.join(", ")
        );
    }
    Ok(())
}

pub async fn show_models(client: &mut AdminClient, mapped: bool, json: bool) -> Result<()> {
    let mut models: Vec<crosslane_kiro::ModelInfo> =
        client.call(method::MODELS, serde_json::json!({})).await?;
    let config = effective_config(client).await?;
    for id in &config.models.disabled {
        if !models.iter().any(|model| {
            crosslane_core::model_policy::model_key(&model.model_id)
                == crosslane_core::model_policy::model_key(id)
        }) {
            models.push(serde_json::from_value(serde_json::json!({"modelId":id}))?);
        }
    }
    if !mapped {
        if json {
            let mut value = serde_json::to_value(&models)?;
            for model in value.as_array_mut().into_iter().flatten() {
                let disabled = config
                    .models
                    .is_disabled(model["modelId"].as_str().unwrap_or_default());
                model["disabled"] = disabled.into();
                model["available"] = (!disabled).into();
            }
            return print_json(&value);
        }
        print!("{}", render_model_list(&models, &config.models));
        return Ok(());
    }
    let available_models = models
        .iter()
        .map(|model| model.model_id.clone())
        .collect::<Vec<_>>();
    let routes = models
        .iter()
        .map(|model| {
            let route = crosslane_translate::model::map_model(
                &model.model_id,
                &config.model_mapping,
                None,
                None,
                &config.features.default_model_id,
            );
            let token_limits =
                crosslane_translate::model::resolve_dynamic_model(&route.mapped, &available_models)
                    .and_then(|resolved| {
                        models
                            .iter()
                            .find(|candidate| candidate.model_id.eq_ignore_ascii_case(&resolved))
                    })
                    .map(model_token_limits)
                    .unwrap_or_default();
            ModelListRoute {
                disabled: config.models.is_disabled(&model.model_id)
                    || config.models.is_disabled(&route.mapped),
                input: model.model_id.clone(),
                mapped: route.mapped,
                rule: route.rule,
                max_input_tokens: token_limits.0,
                max_output_tokens: token_limits.1,
            }
        })
        .collect::<Vec<_>>();
    if json {
        print_json(
            &routes
                .iter()
                .map(|route| {
                    serde_json::json!({
                        "input":route.input,
                        "mapped":route.mapped,
                        "rule":route.rule,
                        "disabled":route.disabled,
                        "available":!route.disabled
                    })
                })
                .collect::<Vec<_>>(),
        )
    } else {
        print!("{}", render_mapped_model_list(&routes));
        Ok(())
    }
}

#[derive(Debug)]
struct ModelListRoute {
    disabled: bool,
    input: String,
    mapped: String,
    rule: Option<String>,
    max_input_tokens: Option<u32>,
    max_output_tokens: Option<u32>,
}

fn render_model_list(
    models: &[crosslane_kiro::ModelInfo],
    policy: &crosslane_core::config::ModelsConfig,
) -> String {
    if models.is_empty() {
        return "暂无可用模型。\n".into();
    }
    let rows = models
        .iter()
        .map(|model| {
            vec![
                model.model_id.clone(),
                if policy.is_disabled(&model.model_id) {
                    "不可用（已禁用）".into()
                } else {
                    "可用".into()
                },
                format_model_token_limit(model_token_limits(model).0),
                format_model_token_limit(model_token_limits(model).1),
                if model.model_name.trim().is_empty() {
                    "-".into()
                } else {
                    model.model_name.clone()
                },
            ]
        })
        .collect::<Vec<_>>();
    render_table(&["模型", "状态", "输入上下文", "输出上限", "名称"], &rows)
}

fn render_mapped_model_list(routes: &[ModelListRoute]) -> String {
    if routes.is_empty() {
        return "暂无可用模型。\n".into();
    }
    let rows = routes
        .iter()
        .map(|route| {
            vec![
                route.input.clone(),
                if route.disabled {
                    "不可用（已禁用）".into()
                } else {
                    "可用".into()
                },
                route.mapped.clone(),
                format_model_token_limit(route.max_input_tokens),
                format_model_token_limit(route.max_output_tokens),
                route.rule.clone().unwrap_or_else(|| "(无规则)".into()),
            ]
        })
        .collect::<Vec<_>>();
    render_table(
        &[
            "输入模型",
            "状态",
            "映射模型",
            "输入上下文",
            "输出上限",
            "规则",
        ],
        &rows,
    )
}

fn model_token_limits(model: &crosslane_kiro::ModelInfo) -> (Option<u32>, Option<u32>) {
    let Some(limits) = model.token_limits.as_ref() else {
        return (None, None);
    };
    (
        limits.max_input_tokens.filter(|tokens| *tokens > 0),
        limits.max_output_tokens.filter(|tokens| *tokens > 0),
    )
}

fn format_model_token_limit(tokens: Option<u32>) -> String {
    let Some(tokens) = tokens.filter(|tokens| *tokens > 0) else {
        return "-".into();
    };
    if tokens >= 1_000_000 {
        return format_scaled_token_count(tokens, 1_000_000, "M");
    }
    if tokens >= 1_000 {
        return format_scaled_token_count(tokens, 1_000, "K");
    }
    tokens.to_string()
}

fn format_scaled_token_count(tokens: u32, scale: u32, suffix: &str) -> String {
    if tokens.is_multiple_of(scale) {
        return format!("{}{suffix}", tokens / scale);
    }
    let scaled = f64::from(tokens) / f64::from(scale);
    let formatted = if scaled < 10.0 {
        format!("{scaled:.2}")
    } else {
        format!("{scaled:.1}")
    };
    format!(
        "{}{suffix}",
        formatted.trim_end_matches('0').trim_end_matches('.')
    )
}

pub async fn show_model_resolution(
    client: &mut AdminClient,
    model: &str,
    api_key: Option<&str>,
    json: bool,
) -> Result<()> {
    let result: ModelResolutionResult = client
        .call(
            method::MODEL_RESOLVE,
            serde_json::json!({"model":model,"api_key":api_key}),
        )
        .await?;
    if json {
        return print_json(&result);
    }

    println!("输入模型  {}", result.input_model);
    println!("显式映射  {}", result.mapped_model);
    println!(
        "映射规则  {}",
        result.mapping_rule.as_deref().unwrap_or("(无规则)")
    );
    if let Some(resolved) = &result.resolved_model {
        println!("最终模型  {resolved}");
    } else if result.possible_models.is_empty() {
        println!("最终模型  (无法解析)");
    } else {
        println!("候选模型  {}", result.possible_models.join(", "));
    }
    println!(
        "匹配账号  {}/{}",
        result.matched_accounts, result.total_accounts
    );
    let rows = result
        .accounts
        .iter()
        .map(|account| {
            vec![
                format!("{} ({})", account.account_name, account.account_id),
                account.health.clone(),
                account.mapped_model.clone(),
                account
                    .resolved_model
                    .clone()
                    .or_else(|| account.error.clone())
                    .unwrap_or_else(|| "-".into()),
                account.mapping_rule.clone().unwrap_or_else(|| "-".into()),
                if account.used_default {
                    format!("{}+default", account.model_source)
                } else {
                    account.model_source.clone()
                },
            ]
        })
        .collect::<Vec<_>>();
    println!(
        "\n{}",
        render_table(
            &[
                "账号",
                "状态",
                "映射模型",
                "最终模型 / 原因",
                "规则",
                "来源",
            ],
            &rows
        )
    );
    Ok(())
}

pub async fn run_model_map(
    client: &mut AdminClient,
    command: ModelMapCommand,
    json: bool,
) -> Result<()> {
    match command {
        ModelMapCommand::List { provider } => {
            let config = effective_config(client).await?;
            let mut rules = config.model_mapping;
            if let Some(provider) = provider.as_deref().filter(|id| *id != "all") {
                rules.retain(|rule| {
                    if rule.providers.is_empty() {
                        provider == "kiro"
                    } else {
                        rule.providers.iter().any(|item| item == provider)
                    }
                });
            }
            rules.sort_by_key(|rule| rule.priority);
            if json {
                print_json(&rules)
            } else {
                for rule in rules {
                    let credits = rule
                        .max_remaining_credit_percent
                        .map(|value| format!("剩余<{value}%"))
                        .unwrap_or_else(|| "无额度条件".into());
                    let schedule = rule
                        .schedule
                        .as_ref()
                        .map(|schedule| schedule.mode.as_str())
                        .unwrap_or("全天");
                    println!(
                        "[{:>3}] {:<24} {:<11} {} -> {}  provider={} service={}  {}  {}{}",
                        rule.priority,
                        rule.name,
                        rule.kind,
                        rule.source_models.join(","),
                        rule.target_models.join(","),
                        if rule.providers.is_empty() {
                            "kiro".into()
                        } else {
                            rule.providers.join(",")
                        },
                        if rule.service_ids.is_empty() {
                            "*".into()
                        } else {
                            rule.service_ids.join(",")
                        },
                        credits,
                        schedule,
                        if rule.enabled { "" } else { " [已停用]" }
                    );
                }
                Ok(())
            }
        }
        ModelMapCommand::Add {
            name,
            kind,
            source_models,
            target_models,
            priority,
            weights,
            below_credits_percent,
            api_key_ids,
            providers,
            service_ids,
            disabled,
        } => {
            mutate_config_array(client, "model_mapping", |array| {
                if array.iter().any(|value| named_value_matches(value, &name)) {
                    return Err(anyhow!("model mapping already exists: {name}"));
                }
                let mut table = toml::map::Map::new();
                table.insert("name".into(), toml::Value::String(name.clone()));
                table.insert("enabled".into(), toml::Value::Boolean(!disabled));
                table.insert("type".into(), toml::Value::String(kind.clone()));
                table.insert("source_models".into(), string_array_value(&source_models));
                table.insert("target_models".into(), string_array_value(&target_models));
                table.insert("priority".into(), toml::Value::Integer(i64::from(priority)));
                if !weights.is_empty() {
                    table.insert("weights".into(), integer_array_value(&weights));
                }
                if let Some(percent) = below_credits_percent {
                    table.insert(
                        "max_remaining_credit_percent".into(),
                        toml::Value::Float(percent),
                    );
                }
                if !api_key_ids.is_empty() {
                    table.insert("api_key_ids".into(), string_array_value(&api_key_ids));
                }
                if !providers.is_empty() {
                    table.insert("providers".into(), string_array_value(&providers));
                }
                if !service_ids.is_empty() {
                    table.insert("service_ids".into(), string_array_value(&service_ids));
                }
                // Missing schedule means always active. A credits threshold
                // naturally stops matching after the upstream monthly quota
                // refresh raises the remaining percentage again.
                array.push(toml::Value::Table(table));
                Ok(())
            })
            .await?;
            if json {
                print_json(&serde_json::json!({"name": name, "created": true}))
            } else {
                println!("已添加模型映射规则 {name}");
                Ok(())
            }
        }
        ModelMapCommand::Edit {
            name,
            rename,
            kind,
            source_models,
            target_models,
            priority,
            weights,
            clear_weights,
            below_credits_percent,
            clear_credits_threshold,
            api_key_ids,
            clear_api_keys,
            providers,
            clear_providers,
            service_ids,
            clear_services,
            enable,
            disable,
        } => {
            if rename.is_none()
                && kind.is_none()
                && source_models.is_empty()
                && target_models.is_empty()
                && priority.is_none()
                && weights.is_empty()
                && !clear_weights
                && below_credits_percent.is_none()
                && !clear_credits_threshold
                && api_key_ids.is_empty()
                && !clear_api_keys
                && providers.is_empty()
                && !clear_providers
                && service_ids.is_empty()
                && !clear_services
                && !enable
                && !disable
            {
                return Err(anyhow!("没有指定修改项；请至少提供一个 edit 选项"));
            }
            mutate_config_array(client, "model_mapping", |array| {
                let table = find_named_table_mut(array, &name, "model mapping")?;
                replace_optional_string(table, "name", rename.as_deref());
                replace_optional_string(table, "type", kind.as_deref());
                if !source_models.is_empty() {
                    table.insert("source_models".into(), string_array_value(&source_models));
                }
                if !target_models.is_empty() {
                    table.insert("target_models".into(), string_array_value(&target_models));
                }
                if let Some(priority) = priority {
                    table.insert("priority".into(), toml::Value::Integer(i64::from(priority)));
                }
                if clear_weights {
                    table.remove("weights");
                } else if !weights.is_empty() {
                    table.insert("weights".into(), integer_array_value(&weights));
                }
                if clear_credits_threshold {
                    table.remove("max_remaining_credit_percent");
                } else if let Some(percent) = below_credits_percent {
                    table.insert(
                        "max_remaining_credit_percent".into(),
                        toml::Value::Float(percent),
                    );
                }
                if clear_api_keys {
                    table.remove("api_key_ids");
                } else if !api_key_ids.is_empty() {
                    table.insert("api_key_ids".into(), string_array_value(&api_key_ids));
                }
                if clear_providers {
                    table.remove("providers");
                } else if !providers.is_empty() {
                    table.insert("providers".into(), string_array_value(&providers));
                }
                if clear_services {
                    table.remove("service_ids");
                } else if !service_ids.is_empty() {
                    table.insert("service_ids".into(), string_array_value(&service_ids));
                }
                if enable || disable {
                    table.insert("enabled".into(), toml::Value::Boolean(enable));
                }
                Ok(())
            })
            .await?;
            if json {
                print_json(&serde_json::json!({
                    "name": name,
                    "renamed_to": rename,
                    "updated": true
                }))
            } else {
                if let Some(rename) = rename {
                    println!("已更新模型映射规则 {name} -> {rename}");
                } else {
                    println!("已更新模型映射规则 {name}");
                }
                Ok(())
            }
        }
        ModelMapCommand::Delete { name, yes } => {
            if !crate::commands::confirm_unless(yes, &format!("确认删除模型映射规则 {name}？"))
                .await?
            {
                if json {
                    print_json(&serde_json::json!({
                        "name": name,
                        "deleted": false,
                        "cancelled": true
                    }))?;
                } else {
                    println!("已取消");
                }
                return Ok(());
            }
            mutate_config_array(client, "model_mapping", |array| {
                remove_named_value(array, &name, "model mapping")
            })
            .await?;
            if json {
                print_json(&serde_json::json!({"name": name, "deleted": true}))
            } else {
                println!("已删除模型映射规则 {name}");
                Ok(())
            }
        }
        ModelMapCommand::Test {
            model,
            remaining_credits_percent,
            api_key,
            provider,
            service,
        } => {
            let config = effective_config(client).await?;
            let route = crosslane_translate::model::map_model_for_provider(
                &model,
                &config.model_mapping,
                crosslane_translate::model::ModelMappingContext {
                    provider_id: &provider,
                    service_id: service.as_deref(),
                    api_key_id: api_key.as_deref(),
                    remaining_percent: remaining_credits_percent,
                },
                config
                    .effective_providers()
                    .iter()
                    .find(|item| item.id == provider)
                    .map(|item| item.routing.default_model_id.as_str())
                    .filter(|model| !model.is_empty())
                    .unwrap_or(&config.features.default_model_id),
            );
            if json {
                print_json(&serde_json::json!({
                    "provider":provider,"service":service,
                    "input":route.original,"matched_rule":route.rule,"result":route.mapped
                }))
            } else {
                println!("提供源    {provider}");
                if let Some(service) = service {
                    println!("代理服务  {service}");
                }
                println!("输入      {}", route.original);
                println!("命中      {}", route.rule.as_deref().unwrap_or("(无规则)"));
                println!("结果      {}", route.mapped);
                Ok(())
            }
        }
    }
}

pub(crate) async fn effective_config(
    client: &mut AdminClient,
) -> Result<crosslane_core::config::Config> {
    let show: ConfigShowResult = client
        .call(method::CONFIG_SHOW, serde_json::json!({}))
        .await?;
    serde_json::from_value(show.effective_json).context("daemon 返回的生效配置无效")
}

#[cfg(test)]
mod tests;
