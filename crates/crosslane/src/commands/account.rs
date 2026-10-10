//! `crosslane account` 子命令。

use anyhow::{anyhow, Context, Result};
use clap::Subcommand;
use crosslane_core::account::Account;
use crosslane_core::ids::{new_account_id, new_machine_id};
use crosslane_ipc::copilot::{enrich_account, CopilotQuotaSummary, CopilotQuotaUnit};
use crosslane_ipc::protocol::{
    method, AccountImportResult, AccountServiceBinding, AccountServicesResult, AccountSummary,
    AccountTagBatchResult, AccountTagResult, ConfigShowResult, CopilotBrowserCredentials,
    ProviderAccountListResult, ProviderAccountSummary,
};

use crate::client::AdminClient;
use crate::output::{format_timestamp, print_json, render_table};

mod copilot_input;

/// Kiro 账号池 overage 配置。
#[derive(Debug, Subcommand)]
pub enum AccountOverageCommand {
    /// 显示全局策略和各 Kiro 账号额度。
    #[command(
        after_help = "示例：\n  crosslane account overage show\n  crosslane account overage show --refresh"
    )]
    Show {
        /// 显示前立即向 Kiro 刷新账号额度。
        #[arg(long)]
        refresh: bool,
    },
    /// 开启 overage，并可同时设置每账号上限。
    #[command(
        after_help = "示例：\n  crosslane account overage enable --max-credits 500\n  crosslane account overage enable --kiro-limit\n\n未传额度参数时复用已有的每账号上限；尚未配置上限时必须显式选择其中一种方式。"
    )]
    Enable {
        /// 每个账号最多使用的 overage credits。
        #[arg(
            long,
            value_name = "CREDITS",
            value_parser = parse_non_negative_credits,
            conflicts_with = "kiro_limit"
        )]
        max_credits: Option<f64>,
        /// 不设置本地上限，完全遵循 Kiro 返回的 overage 上限。
        #[arg(long, conflicts_with = "max_credits")]
        kiro_limit: bool,
        /// 配置生效后不立即向 Kiro 刷新账号额度。
        #[arg(long)]
        no_refresh: bool,
    },
    /// 关闭代理侧 overage；保留已配置的每账号上限供下次启用。
    Disable,
}

/// 账号相关子命令。
#[derive(Debug, Subcommand)]
pub enum AccountCommand {
    /// 列出 Kiro/Copilot 账号、有效期、额度和本机累计 AIU。
    #[command(
        long_about = "列出 Kiro、GitHub Copilot 或全部账号，默认按邮箱排序。\n\n示例：\n  crosslane account list --provider kiro\n  crosslane account list --provider copilot\n  crosslane account list --provider all\n  crosslane account list --tag prod --enabled-only"
    )]
    List {
        /// 提供源实例 ID；默认聚合全部提供源。
        #[arg(long, default_value = "all")]
        provider: String,
        /// 按驱动类型筛选；不是实例 ID。
        #[arg(long, value_parser = ["kiro", "copilot"])]
        provider_kind: Option<String>,
        /// 只显示带该标签的账号。
        #[arg(long, value_name = "TAG")]
        tag: Option<String>,
        /// 只显示已启用账号。
        #[arg(long)]
        enabled_only: bool,
        /// 状态过滤；low_credit/exhausted/cooling/banned 等额度状态主要用于 Kiro。
        #[arg(
            long,
            value_parser = [
                "available",
                "low_credit",
                "disabled",
                "exhausted",
                "cooling",
                "banned",
                "refreshing",
                "unavailable"
            ]
        )]
        status: Option<String>,
        /// 排序字段：email（默认）/credit/id；credit 主要用于 Kiro。
        #[arg(long, value_parser = ["email", "credit", "id"])]
        sort: Option<String>,
    },
    /// 查看或配置整个 Kiro 账号池的 overage 策略。
    #[command(
        long_about = "查看或配置整个 Kiro 账号池的 overage 策略。修改操作会原子更新配置并自动重载。\n\n示例：\n  crosslane account overage\n  crosslane account overage show\n  crosslane account overage enable --max-credits 500\n  crosslane account overage disable"
    )]
    Overage {
        #[command(subcommand)]
        command: Option<AccountOverageCommand>,
    },
    /// 显示单账号详情。
    #[command(
        long_about = "显示 Kiro 或 GitHub Copilot 账号详情，不显示 token；同名账号可用 --provider 消歧。\n\n示例：\n  crosslane account show acc_7f3a --provider kiro\n  crosslane account show acc_8b2c --provider copilot"
    )]
    Show {
        /// 账号 ID 或邮箱。
        id: String,
        /// 提供源实例 ID；也可直接使用 provider/account_id。
        #[arg(long)]
        provider: Option<String>,
    },
    /// 统一添加 Kiro SSO/API key 或 Copilot Device Flow/token 账号。
    #[command(
        after_help = "--provider 填 provider add --id 创建的实例 ID。--headless 交互填写账号并隐藏输入密码；--batch 从 CSV 逐个登录。提供浏览器凭证时，由远端 daemon 的独立无痕 Chromium 完成 Device Flow；--sso-username 表示标准输入的密码仅用于 Azure SSO。未提供浏览器凭证时，仍可在本地无痕窗口手动授权。Kiro 使用 account add --provider kiro --auth sso --email EMAIL --password-stdin，或 --token-stdin 导入 API key；原 add-sso/add-api-key 命令继续可用。\n\n示例：\n  crosslane account add --provider copilot --auth device-flow --headless\n  crosslane account add --provider copilot --batch copilot.csv\n  crosslane account add --provider copilot --auth device-flow --username USER_SHORTCODE --sso-username user@example.com --password-stdin\n  crosslane account add --provider copilot --auth device-flow --credentials-stdin < login.json\n  crosslane account add --provider copilot --auth device-flow\n  printf '%s\\n' \"$GITHUB_TOKEN\" | crosslane account add --provider copilot --token-stdin\n\nCSV 表头：github_username,github_password,sso_username,sso_password,sso_start_url,label；仅 github_username 为必选列，密码需提供 GitHub 或 Azure SSO 其中一套。",
        group(clap::ArgGroup::new("browser_credentials").args(["username", "credentials_stdin", "headless", "batch"]).multiple(true))
    )]
    Add {
        /// 提供源实例 ID，见 provider list 的实例 ID 列；不是驱动 kind。
        // Clap checks leaf requirements before propagating parent globals.
        // Validate the resolved value after propagation, allowing root scope.
        #[arg(long, default_value = "", hide_default_value = true)]
        provider: String,
        /// Kiro 支持 sso/api-key；Copilot 支持 device-flow，省略时按来源选择。
        #[arg(long, value_parser = ["device-flow", "sso", "api-key"])]
        auth: Option<String>,
        /// 从标准输入读取 Kiro API key 或 GitHub token 并显式导入。
        #[arg(long, conflicts_with_all = ["credentials_stdin", "batch", "headless", "password_stdin", "capture_headers"])]
        token_stdin: bool,
        /// Kiro 邮箱（--email）或 GitHub 用户名/邮箱；Copilot SSO 必须用完整用户名。
        #[arg(long, visible_alias = "email", conflicts_with_all = ["credentials_stdin", "batch"])]
        username: Option<String>,
        /// 从标准输入读取一行密码；指定 --sso-username 时仅发送给 Azure。
        #[arg(long, requires = "username", conflicts_with_all = ["headless", "credentials_stdin", "batch"])]
        password_stdin: bool,
        /// Copilot：Azure SSO 登录名，可与 GitHub 用户名不同。
        #[arg(long, requires = "browser_credentials", conflicts_with_all = ["credentials_stdin", "batch"])]
        sso_username: Option<String>,
        /// Copilot：交互填写账号并隐藏输入密码，由远端无痕浏览器登录。
        #[arg(long, conflicts_with_all = ["password_stdin", "credentials_stdin", "batch"])]
        headless: bool,
        /// 批量添加：Kiro 用 email,password；Copilot 用下方 CSV 表头；- 表示标准输入。
        #[arg(long, value_name = "CSV", conflicts_with_all = ["username", "password_stdin", "sso_username", "credentials_stdin", "headless"])]
        batch: Option<String>,
        /// Kiro SSO 入口（--start-url）或 GitHub 组织/企业的 HTTPS SSO 入口。
        #[arg(long, visible_alias = "start-url", requires = "browser_credentials")]
        sso_start_url: Option<String>,
        /// Kiro AWS 区域。
        #[arg(long, default_value = "us-east-1")]
        region: String,
        /// 从标准输入读取 GitHub/Azure 浏览器凭证 JSON；仅用于本次登录。
        #[arg(long, conflicts_with_all = ["username", "password_stdin", "sso_username"])]
        credentials_stdin: bool,
        /// 在服务器受保护文件中记录完整请求/响应 header（含会话凭证）。
        #[arg(long, requires = "browser_credentials")]
        capture_headers: bool,
        /// 账号备注，便于在列表和详情中识别。
        #[arg(long)]
        label: Option<String>,
    },
    /// 向正在运行的远端 Copilot 浏览器提交 MFA 验证码。
    LoginCode {
        #[arg(long, default_value = "", hide_default_value = true)]
        provider: String,
        #[arg(long)]
        task: String,
        /// 从标准输入读取一行验证码。
        #[arg(long, required = true)]
        code_stdin: bool,
    },
    /// 查看 Kiro/Copilot 账号绑定的 API 代理服务。
    #[command(
        long_about = "查看 Kiro/Copilot 账号当前属于哪些 API 代理服务，并显示全局池、标签或手工绑定来源。\n\n示例：\n  crosslane account services acc_7f3a2b1c\n  crosslane account services alice@example.com"
    )]
    Services {
        /// 账号 ID 或邮箱。
        id: String,
        #[arg(long)]
        provider: Option<String>,
    },
    /// 从 JSON 导入已有 Kiro 凭证。
    #[command(
        long_about = "导入已有 Kiro 凭证。id、machine_id、created_at 缺失时自动生成；--tag 会合并到本次导入的全部账号。GitHub Copilot 请用 account add --provider copilot。\n\n示例：\n  crosslane account import --file accounts.json --tag team-a\n  cat accounts.json | crosslane account import --stdin --tag team-a --tag prod",
        group(clap::ArgGroup::new("import_source").required(true).multiple(false).args(["file", "stdin"]))
    )]
    Import {
        /// JSON 文件路径。
        #[arg(long, value_name = "PATH", conflicts_with = "stdin")]
        file: Option<String>,
        /// 从标准输入读取。
        #[arg(long)]
        stdin: bool,
        /// 合并到全部导入账号的标签；可重复或逗号分隔。
        #[arg(long = "tag", value_delimiter = ',', value_name = "TAG")]
        tags: Vec<String>,
    },
    /// 导入 Kiro headless API key，从 KIRO_API_KEY 环境变量或标准输入读取。
    AddApiKey {
        #[arg(long)]
        email: String,
        #[arg(long, default_value = "us-east-1")]
        region: String,
        /// 从标准输入读取 key，避免在命令行参数中暴露凭据。
        #[arg(long)]
        key_stdin: bool,
        /// 新账号标签；可重复或逗号分隔。
        #[arg(long = "tag", value_delimiter = ',', value_name = "TAG")]
        tags: Vec<String>,
    },
    /// 导出 Kiro/Copilot 账号 JSON；默认隐去凭证。
    #[command(
        after_help = "省略 --provider 时保持旧版 Kiro 导出格式；Copilot 请显式传入实例 ID。默认隐去敏感凭证；备份需要完整凭证时显式使用 --include-secrets，并写入受保护位置。--redact 作为兼容选项保留。\n\n示例：\n  crosslane account export --provider all\n  crosslane account export --provider all --include-secrets > accounts-backup.json"
    )]
    Export {
        /// 隐去 token 与 secret；兼容旧命令，现为默认行为。
        #[arg(long, conflicts_with = "include_secrets")]
        redact: bool,
        /// 导出完整凭证，用于受保护的备份。
        #[arg(long, conflicts_with = "redact")]
        include_secrets: bool,
        /// 提供源实例 ID，使用 all 导出全部；省略时保持旧版 Kiro 输出格式。
        #[arg(long)]
        provider: Option<String>,
    },
    /// 通过 IAM Identity Center SSO 登录并添加 Kiro 账号。
    #[command(
        after_help = "仅适用于 Kiro 的 IAM Identity Center SSO；GitHub Copilot 请使用 account add --provider copilot --auth device-flow。\n\n示例：\n  printf '%s\\n' \"$PASSWORD\" | crosslane account add-sso --email user@example.com --start-url https://example.awsapps.com/start --password-stdin\n  crosslane account add-sso --batch accounts.csv --start-url https://example.awsapps.com/start"
    )]
    AddSso {
        /// 单账号登录邮箱；需同时使用 --password-stdin。
        #[arg(long, required_unless_present = "batch", requires = "password_stdin")]
        email: Option<String>,
        /// IAM Identity Center start URL；未提供时读取 `[sso].start_url`。
        #[arg(long)]
        start_url: Option<String>,
        #[arg(long, default_value = "us-east-1")]
        region: String,
        /// 必须显式声明，从标准输入读取一行密码；密码不会进入命令行历史。
        #[arg(long, required_unless_present = "batch", requires = "email")]
        password_stdin: bool,
        /// 两列 CSV（email,password）批量登录；PATH 为 - 时从 stdin 读取。
        #[arg(long, value_name = "PATH", conflicts_with_all = ["email", "password_stdin"])]
        batch: Option<String>,
        /// 批量登录并发数，范围 1..8，默认 1。
        #[arg(
            short = 'c',
            long,
            requires = "batch",
            value_parser = parse_sso_concurrency
        )]
        concurrency: Option<usize>,
        /// 显示浏览器窗口，便于手工处理额外验证。
        #[arg(long)]
        headful: bool,
        /// 新账号标签；批量模式下应用到本批全部账号，可重复或逗号分隔。
        #[arg(long = "tag", value_delimiter = ',', value_name = "TAG")]
        tags: Vec<String>,
    },
    /// 删除 Kiro/Copilot 账号；可按来源消歧。
    #[command(
        visible_alias = "delete",
        long_about = "删除一个或多个 Kiro/Copilot 账号，整批执行前只需输入一次 y 或 yes 确认。同名账号用 --provider 或 provider/account_id 消歧。\n\n示例：\n  crosslane account rm acc_7f3a2b1c --provider kiro\n  crosslane account rm acc_8b2c --provider copilot"
    )]
    Rm {
        /// 一个或多个账号 ID/邮箱，以空格分隔。
        #[arg(required = true, num_args = 1.., value_name = "ID_OR_EMAIL")]
        ids: Vec<String>,
        /// 提供源实例 ID；也可使用 provider/account_id。
        #[arg(long)]
        provider: Option<String>,
        /// 跳过交互确认，用于自动化。
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// 启用 Kiro/Copilot 账号。
    #[command(
        after_help = "示例：\n  crosslane account enable acc_7f3a2b1c --provider kiro\n  crosslane account enable acc_8b2c --provider copilot"
    )]
    Enable {
        /// 账号 ID 或邮箱。
        id: String,
        #[arg(long)]
        provider: Option<String>,
    },
    /// 停用 Kiro/Copilot 账号。
    #[command(
        after_help = "示例：\n  crosslane account disable acc_7f3a2b1c --provider kiro\n  crosslane account disable acc_8b2c --provider copilot"
    )]
    Disable {
        /// 账号 ID 或邮箱。
        id: String,
        #[arg(long)]
        provider: Option<String>,
    },
    /// 为 Kiro/Copilot 账号增删标签，自动识别账号归属。
    #[command(
        long_about = "为 Kiro/Copilot 账号增删标签，可同时添加和移除。未指定 --provider 时按 ID/邮箱/登录名解析归属，重名需用 provider/account_id 消歧。整批先校验，每个 provider 内一次持久化；跨 provider 不提供分布式事务。服务 --account-tag 支持两种来源。\n\n示例：\n  crosslane account tag acc_7f3a --provider kiro --add prod\n  crosslane account tag acc_8b2c --provider copilot --add team-a",
        group(clap::ArgGroup::new("tag_change").required(true).multiple(true).args(["add", "remove"]))
    )]
    Tag {
        /// 一个或多个账号 ID/邮箱，以空格分隔。
        #[arg(required = true, num_args = 1.., value_name = "ID_OR_EMAIL")]
        ids: Vec<String>,
        /// 添加标签，可重复。
        #[arg(long = "add", value_name = "TAG")]
        add: Vec<String>,
        /// 移除标签，可重复。
        #[arg(long = "rm", value_name = "TAG")]
        remove: Vec<String>,
        /// 提供源实例 ID；也可使用 provider/account_id。
        #[arg(long)]
        provider: Option<String>,
    },
    /// 重新生成 Kiro 设备标识；Copilot 不使用 machine_id。
    #[command(
        long_about = "仅为 Kiro 账号重新生成 machine_id，在怀疑当前组合被标记时使用；Copilot 不使用该设备标识。\n\n示例：\n  crosslane account regen-machine-id acc_7f3a2b1c"
    )]
    RegenMachineId {
        /// 账号 ID 或邮箱。
        id: String,
    },
    /// 立即刷新 Kiro/Copilot 账号 token。
    #[command(
        after_help = "--all 省略 --provider 时覆盖全部来源；使用 --provider 限定实例。\n\n示例：\n  crosslane account refresh --provider kiro --all\n  crosslane account refresh --provider copilot --all",
        group(clap::ArgGroup::new("refresh_target").required(true).multiple(false).args(["id", "all"]))
    )]
    Refresh {
        id: Option<String>,
        #[arg(long, conflicts_with = "id")]
        all: bool,
        /// 限定提供源；--all 时默认聚合全部来源。
        #[arg(long)]
        provider: Option<String>,
    },
    /// 探测 Kiro/Copilot 账号可用端点与模型。
    #[command(
        after_help = "Kiro 会执行真实推理探测；Copilot 刷新账号 token/模型目录，不发起同样的推理请求。\n\n示例：\n  crosslane account probe --provider kiro <ACCOUNT_ID>\n  crosslane account probe --provider copilot <ACCOUNT_ID>\n  crosslane account probe --provider all --all",
        group(clap::ArgGroup::new("probe_target").required(true).multiple(false).args(["id", "all"]))
    )]
    Probe {
        id: Option<String>,
        #[arg(long, conflicts_with = "id")]
        all: bool,
        /// 限定提供源；批量操作可显式使用 all。
        #[arg(long)]
        provider: Option<String>,
    },
    /// 重置 Kiro 账号健康标记或 Copilot 运行错误。
    #[command(
        after_help = "Kiro 清除冷却、封禁与额度耗尽标记；Copilot 清除该账号的运行错误状态。\n\n示例：\n  crosslane account reset-health --provider kiro --all\n  crosslane account reset-health --provider copilot <ACCOUNT_ID>",
        group(clap::ArgGroup::new("reset_health_target").required(true).multiple(false).args(["id", "all"]))
    )]
    ResetHealth {
        id: Option<String>,
        #[arg(long, conflicts_with = "id")]
        all: bool,
        /// 限定提供源；批量操作可显式使用 all。
        #[arg(long)]
        provider: Option<String>,
    },
}

/// 构造账号列表表格行。
#[cfg(test)]
pub fn build_list_rows(accounts: &[AccountSummary]) -> Vec<Vec<String>> {
    accounts
        .iter()
        .map(|account| {
            let status = display_health(account);
            let credit = match (account.credit_current, account.credit_limit) {
                (Some(current), Some(limit)) => format!("{current:.2}/{limit:.2}"),
                _ => "-".into(),
            };
            vec![
                account.id.clone(),
                account.email.clone(),
                status,
                credit,
                account.subscription.clone().unwrap_or_else(|| "-".into()),
                if account.tags.is_empty() {
                    "-".into()
                } else {
                    account.tags.join(",")
                },
            ]
        })
        .collect()
}

fn build_provider_list_rows(accounts: &[ProviderAccountSummary]) -> Vec<Vec<String>> {
    accounts
        .iter()
        .map(|account| {
            let kiro_total = account.details["kiro_overage_total_limit"].as_f64();
            let quota = if account.provider_kind == "copilot" {
                copilot_quota_display(&account.details["info"])
            } else {
                match (account.quota_current, account.quota_limit, kiro_total) {
                    (Some(current), Some(limit), Some(kiro_total)) => {
                        format!("{current:.2}/{limit:.2}/{kiro_total:.2}")
                    }
                    (Some(current), Some(limit), None) => format!("{current:.2}/{limit:.2}"),
                    (Some(current), None, _) => format!("{current:.2}"),
                    _ => "未知".into(),
                }
            };
            let overage_enabled = account.details["overage_enabled"].as_bool();
            let configured_cap = account.details["max_overage_credits_per_account"].as_f64();
            let configured_overage = match overage_enabled {
                Some(false) => configured_cap
                    .map(|value| format!("关闭({value:.2})"))
                    .unwrap_or_else(|| "关闭".into()),
                Some(true) => account.details["max_overage_credits_per_account"]
                    .as_f64()
                    .map(|value| format!("{value:.2}"))
                    .unwrap_or_else(|| "Kiro上限".into()),
                None => "-".into(),
            };
            let kiro_overage = account.details["kiro_overage_cap"]
                .as_f64()
                .or_else(|| account.details["overage_cap"].as_f64())
                .map(|value| format!("{value:.2}"))
                .unwrap_or_else(|| "-".into());
            vec![
                account.provider_id.clone(),
                crate::cli::scope::kind_name(&account.provider_kind).to_owned(),
                account.id.clone(),
                account.display_name.clone(),
                if account.enabled {
                    display_provider_health(&account.health)
                } else {
                    "停用".into()
                },
                quota,
                if overage_enabled.is_some() {
                    format!("{configured_overage}/{kiro_overage}")
                } else {
                    "-".into()
                },
                if account.tags.is_empty() {
                    "-".into()
                } else {
                    account.tags.join(",")
                },
                if account.provider_kind == "copilot" {
                    account.details["info"]["plan"]
                        .as_str()
                        .unwrap_or("未知")
                        .into()
                } else {
                    account.details["subscription"]
                        .as_str()
                        .unwrap_or("-")
                        .into()
                },
                if account.provider_kind == "copilot" {
                    metadata_timestamp(&account.details["info"]["api_token_expires_at"])
                } else {
                    metadata_timestamp(&account.details["token_expires_at"])
                },
                if account.provider_kind == "copilot" {
                    account.details["local_aiu_usage"]["total_nano_aiu"]
                        .as_u64()
                        .map(format_nano_aiu)
                        .unwrap_or_else(|| "未知".into())
                } else {
                    "-".into()
                },
                if account.provider_kind == "copilot" {
                    account.details["info"]["quota_reset_date"]
                        .as_str()
                        .unwrap_or("未知")
                        .into()
                } else {
                    "-".into()
                },
                if account.provider_kind == "copilot" {
                    copilot_upstream_usage_display(&account.details["info"])
                } else {
                    "-".into()
                },
            ]
        })
        .collect()
}

fn format_nano_aiu(amount: u64) -> String {
    let whole = amount / 1_000_000_000;
    let fraction = amount % 1_000_000_000;
    if fraction == 0 {
        whole.to_string()
    } else {
        format!("{whole}.{:09}", fraction)
            .trim_end_matches('0')
            .into()
    }
}

fn metadata_timestamp(value: &serde_json::Value) -> String {
    value
        .as_i64()
        .filter(|value| *value > 0)
        .map(format_timestamp)
        .unwrap_or_else(|| "未知".into())
}

fn copilot_quota_display(info: &serde_json::Value) -> String {
    let quota = CopilotQuotaSummary::from_info(info);
    let display = match quota.unit {
        Some(CopilotQuotaUnit::AiCredits) => match quota.total {
            Some(total) => format!(
                "{}/{total} credits (余 {})",
                quota
                    .used
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "未知".into()),
                quota
                    .remaining
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "未知".into()),
            ),
            None => quota
                .percent_remaining
                .map(|percent| format!("总额/余额未知 (剩余 {percent:.2}%)"))
                .unwrap_or_else(|| "总额/余额未知".into()),
        },
        Some(CopilotQuotaUnit::PremiumRequests) if quota.unlimited == Some(true) => {
            "个人不限".into()
        }
        Some(CopilotQuotaUnit::PremiumRequests) => match (quota.used, quota.total) {
            (Some(used), Some(total)) => format!("{used:.2}/{total:.2} 请求"),
            _ => "未知".into(),
        },
        None => "未知".into(),
    };
    if quota.cached {
        format!("{display}(缓存)")
    } else {
        display
    }
}

fn copilot_upstream_usage_display(info: &serde_json::Value) -> String {
    let used = info["quota_snapshots"]["premium_interactions"]["credits_used"]
        .as_f64()
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| value.to_string())
        .unwrap_or_else(|| "未知".into());
    if info["quota_error"].as_str().is_some() {
        format!("{used}(缓存)")
    } else {
        used
    }
}

fn display_provider_health(health: &str) -> String {
    match health {
        "available" => "可用",
        "low_credit" => "低额度保护",
        "disabled" => "停用",
        "exhausted" => "额度耗尽",
        "cooling" => "冷却",
        "banned" => "已封禁",
        "refreshing" => "刷新中",
        "unavailable" => "不可用",
        other => other,
    }
    .into()
}

fn print_provider_detail(account: &ProviderAccountSummary) {
    println!(
        "{}/{}   {}",
        account.provider_id, account.id, account.display_name
    );
    println!("实例 ID   {}", account.provider_id);
    println!(
        "类型      {}",
        crate::cli::scope::kind_name(&account.provider_kind)
    );
    println!("状态      {}", display_provider_health(&account.health));
    println!("认证      {}", account.auth_state);
    if let Some(email) = &account.email {
        println!("邮箱      {email}");
    }
    if let Some(label) = &account.label {
        println!("备注      {label}");
    }
    if account.provider_kind == "copilot" {
        println!(
            "个人额度  {}",
            copilot_quota_display(&account.details["info"])
        );
        let info = &account.details["info"];
        println!("套餐      {}", info["plan"].as_str().unwrap_or("未知"));
        println!("SKU       {}", info["sku"].as_str().unwrap_or("未知"));
        println!(
            "上游月用量 {}（credits_used，按上游口径）",
            copilot_upstream_usage_display(info)
        );
        println!(
            "API Token到期 {}（自动续期）",
            metadata_timestamp(&info["api_token_expires_at"])
        );
        println!(
            "GitHub凭据到期 {}",
            metadata_timestamp(&info["github_token_expires_at"])
        );
        println!(
            "GitHub刷新凭据到期 {}",
            metadata_timestamp(&info["github_refresh_token_expires_at"])
        );
        println!(
            "额度重置  {}",
            info["quota_reset_date"].as_str().unwrap_or("未知")
        );
        println!(
            "额度查询  {}",
            metadata_timestamp(&info["quota_checked_at"])
        );
        if let Some(error) = info["quota_error"].as_str() {
            println!("额度错误  {error}（保留上次数据）");
        }
        let usage = &account.details["local_aiu_usage"];
        println!(
            "本机 AIU  {}",
            usage["total_nano_aiu"]
                .as_u64()
                .map(format_nano_aiu)
                .unwrap_or_else(|| "未知".into())
        );
        if let Some(requests) = usage["billed_requests"].as_u64() {
            println!("计费响应  {requests} 次（仅本机观察到的 copilot_usage）");
            println!(
                "统计开始  {}",
                metadata_timestamp(&usage["first_recorded_at"])
            );
            println!(
                "最近计费  {}",
                metadata_timestamp(&usage["last_recorded_at"])
            );
        }
    } else {
        match (
            account.quota_current,
            account.quota_limit,
            account.details["kiro_overage_total_limit"].as_f64(),
        ) {
            (Some(current), Some(limit), Some(kiro_total)) => println!(
            "额度      {current:.2}/{limit:.2}/{kiro_total:.2} {}（当前已用/配置总额/Kiro总额）",
            account.quota_unit.as_deref().unwrap_or("unknown")
        ),
            (Some(current), Some(limit), None) => println!(
                "额度      {current:.2}/{limit:.2} {}",
                account.quota_unit.as_deref().unwrap_or("unknown")
            ),
            _ => println!("额度      未知"),
        }
    }
    if let Some(enabled) = account.details["overage_enabled"].as_bool() {
        println!("超额开关  {}", if enabled { "开启" } else { "关闭" });
        let configured = account.details["max_overage_credits_per_account"]
            .as_f64()
            .map(|value| format!("{value:.2} credits/账号"))
            .unwrap_or_else(|| "遵循 Kiro 上限".into());
        println!("超额配置  {configured}");
        if let Some(value) = account.details["kiro_overage_cap"]
            .as_f64()
            .or_else(|| account.details["overage_cap"].as_f64())
        {
            println!("Kiro超额  {value:.2} credits");
        }
        if let Some(value) = account.details["effective_overage_cap"].as_f64() {
            println!("有效超额  {value:.2} credits");
        }
    }
    println!(
        "模型      {}",
        if account.supported_models.is_empty() {
            "未知".into()
        } else {
            account.supported_models.join(", ")
        }
    );
    println!(
        "标签      {}",
        if account.tags.is_empty() {
            "-".into()
        } else {
            account.tags.join(", ")
        }
    );
    if account.details != serde_json::Value::Null {
        println!("详情      {}", account.details);
    }
}

#[derive(Debug)]
struct ProviderLoginCancelled;

impl std::fmt::Display for ProviderLoginCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("登录已取消")
    }
}

impl std::error::Error for ProviderLoginCancelled {}

fn print_provider_login(state: &serde_json::Value, provider: &str, json: bool) -> Result<()> {
    if json {
        print_json(state)
    } else {
        println!(
            "授权完成，已添加 {}/{}",
            provider,
            state["account_id"].as_str().unwrap_or("-")
        );
        Ok(())
    }
}

async fn run_provider_login(
    client: &mut AdminClient,
    provider: &str,
    auth: &str,
    label: Option<&str>,
    browser: Option<CopilotBrowserCredentials>,
    capture_headers: bool,
    json: bool,
) -> Result<serde_json::Value> {
    let headless = browser.is_some();
    let auth = if headless {
        "device-flow-headless"
    } else {
        auth
    };
    let mut params = serde_json::json!({"provider":provider,"auth":auth,"label":label});
    if let Some(browser) = browser {
        params["browser"] = serde_json::to_value(browser)?;
        params["capture_headers"] = capture_headers.into();
    }
    let mut state: serde_json::Value = client.call(method::V2_LOGIN_START, params).await?;
    let task_id = state["id"]
        .as_str()
        .ok_or_else(|| anyhow!("daemon 返回的登录任务缺少 id"))?
        .to_owned();
    let verification_uri = state["verification_uri"].as_str().unwrap_or("-");
    let user_code = state["user_code"].as_str().unwrap_or("-");
    if headless {
        eprintln!("远端无痕 Chromium 正在登录 GitHub/Azure 并完成 Device Flow；任务 {task_id}");
        eprintln!("无需打开本地浏览器。请保持当前命令运行。");
    } else if json {
        eprintln!(
            "请在自己电脑的无痕/隐私窗口打开 {verification_uri}，确认目标 GitHub 账号并输入代码 {user_code}；保持当前终端会话"
        );
    } else {
        println!("请在自己电脑的无痕/隐私窗口打开 {verification_uri}");
        println!("确认登录的是要添加的 GitHub 账号，并输入代码 {user_code}");
        println!("等待 GitHub 授权……请保持当前终端/SSH 会话；服务器无需打开浏览器");
    }
    let mut last_browser_progress = String::new();
    loop {
        if let Some(progress) = state.get("browser") {
            let stage = progress["stage"].as_str().unwrap_or("");
            let progress_key = format!("{stage}:{}", progress["message"].as_str().unwrap_or(""));
            if progress_key != last_browser_progress {
                if let Some(message) = progress["message"].as_str() {
                    eprintln!("{message}");
                }
                if stage == "mfa_code" {
                    eprintln!("在另一终端提交验证码：crosslane account login-code --provider {provider} --task {task_id} --code-stdin");
                }
                last_browser_progress = progress_key;
            }
        }
        match state["status"].as_str().unwrap_or("failed") {
            "authorized" => return Ok(state),
            "pending" => {}
            status => {
                return Err(anyhow!(
                    "登录任务 {status}：{}",
                    state["error"].as_str().unwrap_or("未知错误")
                ));
            }
        }
        let interval = state["interval_secs"].as_u64().unwrap_or(5).clamp(1, 60);
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result?;
                let _ = client.call::<serde_json::Value>(
                    method::V2_LOGIN_CANCEL,
                    serde_json::json!({"provider":provider,"task_id":task_id}),
                ).await;
                return Err(ProviderLoginCancelled.into());
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(interval)) => {}
        }
        state = client
            .call(
                method::V2_LOGIN_STATUS,
                serde_json::json!({"provider":provider,"task_id":task_id}),
            )
            .await?;
    }
}

async fn run_copilot_batch(
    client: &mut AdminClient,
    provider: &str,
    rows: Vec<copilot_input::CsvLogin>,
    label: Option<&str>,
    sso_start_url: Option<&str>,
    capture_headers: bool,
    json: bool,
) -> Result<()> {
    let mut successes = Vec::new();
    let mut failures = Vec::new();
    let mut cancelled = false;
    let count = rows.len();
    for (index, mut row) in rows.into_iter().enumerate() {
        if let Some(url) = sso_start_url {
            row.credentials.sso_start_url = Some(url.to_owned());
        }
        let username = row.credentials.github_username.clone();
        eprintln!(
            "[{}/{}] 正在添加 GitHub Copilot 账号 {username}",
            index + 1,
            count
        );
        match run_provider_login(
            client,
            provider,
            "device-flow",
            row.label.as_deref().or(label),
            Some(row.credentials),
            capture_headers,
            json,
        )
        .await
        {
            Ok(state) => {
                if !json {
                    print_provider_login(&state, provider, false)?;
                }
                successes.push(state);
            }
            Err(error) => {
                cancelled = error.is::<ProviderLoginCancelled>();
                eprintln!("{username}: {error}");
                failures.push(serde_json::json!({"username":username,"error":error.to_string()}));
                if cancelled {
                    break;
                }
            }
        }
    }
    if json {
        print_json(&serde_json::json!({
            "accounts":successes,"errors":failures,
            "complete":failures.is_empty(),"cancelled":cancelled,
            "skipped":count - successes.len() - failures.len(),
        }))?;
    }
    if cancelled {
        return Err(ProviderLoginCancelled.into());
    }
    if !failures.is_empty() {
        return Err(anyhow!("{} 个 Copilot 账号登录失败", failures.len()));
    }
    Ok(())
}

#[cfg(test)]
fn display_health(account: &AccountSummary) -> String {
    if !account.enabled {
        return "停用".into();
    }
    if account.credit_exhausted {
        return "额度耗尽".into();
    }
    match account.health.as_deref() {
        Some("available") => "启用",
        Some("low_credit") => "低额度保护",
        Some("cooling") => "冷却",
        Some("exhausted") => "额度耗尽",
        Some("banned") => "已封禁",
        Some("refreshing") => "刷新中",
        Some("disabled") => "停用",
        Some(other) => other,
        None => "启用",
    }
    .into()
}

/// 解析导入载荷，接受单对象或数组并补全本地字段。
pub fn parse_import_payload(raw: &str) -> Result<Vec<Account>> {
    let value: serde_json::Value = serde_json::from_str(raw).context("导入内容不是合法 JSON")?;
    let items = match value {
        serde_json::Value::Array(items) => items,
        object @ serde_json::Value::Object(_) => vec![object],
        _ => return Err(anyhow!("导入内容应为 JSON 对象或对象数组")),
    };

    let now = now_secs();
    let mut accounts = Vec::with_capacity(items.len());
    for (index, mut item) in items.into_iter().enumerate() {
        let object = item
            .as_object_mut()
            .ok_or_else(|| anyhow!("第 {} 项不是 JSON 对象", index + 1))?;
        match object.get("email").and_then(serde_json::Value::as_str) {
            Some(email) if !email.trim().is_empty() => {}
            _ => return Err(anyhow!("第 {} 项缺少有效 email 字段", index + 1)),
        }
        object
            .entry("id")
            .or_insert_with(|| serde_json::Value::String(new_account_id()));
        object
            .entry("machine_id")
            .or_insert_with(|| serde_json::Value::String(new_machine_id()));
        object
            .entry("created_at")
            .or_insert_with(|| serde_json::Value::from(now));

        let account: Account = serde_json::from_value(item)
            .with_context(|| format!("第 {} 项字段不合法", index + 1))?;
        validate_imported_account(&account, index)?;
        accounts.push(account);
    }
    Ok(accounts)
}

fn validate_imported_account(account: &Account, index: usize) -> Result<()> {
    let id_hex = account.id.strip_prefix("acc_").unwrap_or_default();
    if id_hex.len() != 8
        || !id_hex
            .chars()
            .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase())
    {
        return Err(anyhow!("第 {} 项 id 格式无效", index + 1));
    }
    if account.machine_id.len() != 64
        || !account
            .machine_id
            .chars()
            .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase())
    {
        return Err(anyhow!("第 {} 项 machine_id 格式无效", index + 1));
    }
    if account.credentials.access_token.trim().is_empty() {
        return Err(anyhow!("第 {} 项 access_token 不能为空", index + 1));
    }
    Ok(())
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn parse_non_negative_credits(value: &str) -> std::result::Result<f64, String> {
    let credits = value
        .parse::<f64>()
        .map_err(|_| "额度必须是数字".to_owned())?;
    if credits.is_finite() && credits >= 0.0 {
        Ok(credits)
    } else {
        Err("额度必须是有限的非负数".to_owned())
    }
}

fn parse_sso_concurrency(value: &str) -> std::result::Result<usize, String> {
    let concurrency = value
        .parse::<usize>()
        .map_err(|_| "并发数必须是整数".to_owned())?;
    if (1..=8).contains(&concurrency) {
        Ok(concurrency)
    } else {
        Err("并发数必须在 1..=8 之间".to_owned())
    }
}

#[derive(Debug, serde::Serialize)]
struct OverageAccountView {
    id: String,
    account: String,
    enabled: bool,
    health: String,
    current: Option<f64>,
    proxy_total: Option<f64>,
    kiro_total: Option<f64>,
    effective_overage: Option<f64>,
    kiro_overage: Option<f64>,
}

#[derive(Debug, serde::Serialize)]
struct OverageView {
    enabled: bool,
    max_credits_per_account: Option<f64>,
    accounts: Vec<OverageAccountView>,
    complete: bool,
    errors: std::collections::BTreeMap<String, String>,
}

async fn fetch_overage_view(client: &mut AdminClient) -> Result<OverageView> {
    let config = crate::commands::runtime::effective_config(client).await?;
    let list: ProviderAccountListResult = client
        .call(
            method::V2_ACCOUNT_LIST,
            serde_json::json!({
                "provider":"kiro",
                "enabled_only":false,
            }),
        )
        .await?;
    let accounts = list
        .accounts
        .into_iter()
        .filter(|account| account.provider_kind == "kiro")
        .map(|account| OverageAccountView {
            id: account.id,
            account: account.display_name,
            enabled: account.enabled,
            health: account.health,
            current: account.quota_current,
            proxy_total: account.quota_limit,
            kiro_total: account.details["kiro_overage_total_limit"].as_f64(),
            effective_overage: account.details["effective_overage_cap"].as_f64(),
            kiro_overage: account.details["kiro_overage_cap"]
                .as_f64()
                .or_else(|| account.details["overage_cap"].as_f64()),
        })
        .collect();
    Ok(OverageView {
        enabled: config.pool.enable_overage,
        max_credits_per_account: config.pool.max_overage_credits_per_account,
        accounts,
        complete: list.complete,
        errors: list.errors,
    })
}

fn joined_credit_values(values: &[Option<f64>]) -> String {
    values
        .iter()
        .map(|value| {
            value
                .map(|value| format!("{value:.2}"))
                .unwrap_or_else(|| "-".into())
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn build_overage_rows(accounts: &[OverageAccountView]) -> Vec<Vec<String>> {
    accounts
        .iter()
        .map(|account| {
            vec![
                account.id.clone(),
                account.account.clone(),
                if account.enabled {
                    display_provider_health(&account.health)
                } else {
                    "停用".into()
                },
                joined_credit_values(&[account.current, account.proxy_total, account.kiro_total]),
                joined_credit_values(&[account.effective_overage, account.kiro_overage]),
            ]
        })
        .collect()
}

fn print_overage_view(
    view: &OverageView,
    action: Option<&str>,
    refresh: Option<&serde_json::Value>,
    json: bool,
) -> Result<()> {
    if json {
        let mut value = serde_json::to_value(view)?;
        let object = value
            .as_object_mut()
            .expect("serialized overage view is an object");
        if let Some(action) = action {
            object.insert("action".into(), serde_json::Value::String(action.into()));
        }
        if let Some(refresh) = refresh {
            object.insert("refresh".into(), refresh.clone());
        }
        return print_json(&value);
    }

    if let Some(action) = action {
        println!("{action}");
    }
    if let Some(refresh) = refresh {
        if refresh["ok"].as_bool() == Some(false) {
            println!(
                "额度刷新  失败：{}",
                refresh["error"].as_str().unwrap_or("unknown")
            );
        } else {
            println!(
                "额度刷新  {}",
                refresh["result"].as_str().unwrap_or("已完成")
            );
        }
    }
    println!("Overage  {}", if view.enabled { "开启" } else { "关闭" });
    println!(
        "每账号上限  {}",
        view.max_credits_per_account
            .map(|value| format!("{value:.2} credits"))
            .unwrap_or_else(|| "遵循 Kiro 上限".into())
    );
    println!("Kiro 账号  {}", view.accounts.len());
    if view.accounts.is_empty() {
        println!("暂无 Kiro 账号额度数据");
    } else {
        println!(
            "{}",
            render_table(
                &[
                    "ID",
                    "账号",
                    "状态",
                    "额度(已用/代理/Kiro)",
                    "Overage(有效/Kiro)",
                ],
                &build_overage_rows(&view.accounts),
            )
        );
    }
    for (provider, error) in &view.errors {
        eprintln!("{provider}: {error}");
    }
    Ok(())
}

async fn refresh_overage_usage(client: &mut AdminClient) -> serde_json::Value {
    match client
        .call::<serde_json::Value>(
            method::TASK_RUN,
            serde_json::json!({"name":"status_check","provider":null}),
        )
        .await
    {
        Ok(value) => serde_json::json!({"ok":true,"result":value["result"]}),
        Err(error) => serde_json::json!({
            "ok":false,
            "error":error.to_string(),
            "hint":"可稍后运行 crosslane tasks run status_check"
        }),
    }
}

async fn run_overage(
    client: &mut AdminClient,
    command: Option<AccountOverageCommand>,
    json: bool,
) -> Result<()> {
    let (action, refresh) = match command.unwrap_or(AccountOverageCommand::Show { refresh: false })
    {
        AccountOverageCommand::Show { refresh } => {
            let refresh = if refresh {
                Some(refresh_overage_usage(client).await)
            } else {
                None
            };
            (None, refresh)
        }
        AccountOverageCommand::Enable {
            max_credits,
            kiro_limit,
            no_refresh,
        } => {
            let limit = if let Some(value) = max_credits {
                crate::commands::runtime::OverageLimitUpdate::Set(value)
            } else if kiro_limit {
                crate::commands::runtime::OverageLimitUpdate::Clear
            } else {
                let current = crate::commands::runtime::effective_config(client).await?;
                if current.pool.max_overage_credits_per_account.is_some() {
                    crate::commands::runtime::OverageLimitUpdate::Preserve
                } else {
                    return Err(anyhow!(
                        "尚未配置每账号 overage 上限；请使用 --max-credits <CREDITS>，或显式使用 --kiro-limit"
                    ));
                }
            };
            crate::commands::runtime::update_pool_overage_config(client, true, limit).await?;
            let refresh = if no_refresh {
                None
            } else {
                Some(refresh_overage_usage(client).await)
            };
            (Some("enabled"), refresh)
        }
        AccountOverageCommand::Disable => {
            crate::commands::runtime::update_pool_overage_config(
                client,
                false,
                crate::commands::runtime::OverageLimitUpdate::Preserve,
            )
            .await?;
            (Some("disabled"), None)
        }
    };
    let view = fetch_overage_view(client).await.with_context(|| {
        if action.is_some() {
            "overage 配置已生效，但读取更新后的账号额度失败"
        } else {
            "读取 overage 配置和账号额度失败"
        }
    })?;
    let human_action = action.map(|action| match action {
        "enabled" => "Kiro overage 已开启，配置已重载",
        "disabled" => "Kiro overage 已关闭，配置已重载",
        _ => action,
    });
    print_overage_view(
        &view,
        if json { action } else { human_action },
        refresh.as_ref(),
        json,
    )?;
    if !view.complete {
        return Err(anyhow!("Kiro 账号额度查询不完整"));
    }
    if refresh
        .as_ref()
        .is_some_and(|result| result["ok"].as_bool() == Some(false))
    {
        if action.is_some() {
            eprintln!("额度刷新失败；配置已生效，可稍后运行 `crosslane tasks run status_check`");
        } else {
            eprintln!("额度刷新失败；可稍后运行 `crosslane tasks run status_check`");
        }
    }
    Ok(())
}

/// 执行账号子命令。
pub async fn run(client: &mut AdminClient, command: AccountCommand, json: bool) -> Result<()> {
    let command = normalize_add(command)?;
    match command {
        AccountCommand::List {
            provider,
            provider_kind,
            tag,
            enabled_only,
            status,
            sort,
        } => {
            let has_filters =
                tag.is_some() || status.is_some() || provider_kind.is_some() || enabled_only;
            let mut list: ProviderAccountListResult = client
                .call(
                    method::V2_ACCOUNT_LIST,
                    serde_json::json!({
                        "provider":provider,
                        "provider_kind":provider_kind,
                        "tag": tag,
                        "enabled_only": enabled_only,
                        "status":status,
                        "sort":sort,
                    }),
                )
                .await?;
            for account in &mut list.accounts {
                enrich_account(account);
            }
            if json {
                print_json(&list)?;
            } else if list.accounts.is_empty() {
                println!("{}", empty_account_hint(&provider, has_filters));
            } else {
                print!(
                    "{}",
                    render_table(
                        &[
                            "实例 ID",
                            "类型 (kind)",
                            "ID",
                            "账号",
                            "状态",
                            "额度(已用/代理/Kiro)",
                            "Overage(配置/Kiro)",
                            "标签",
                            "套餐",
                            "Token到期(UTC)",
                            "AIU(本机累计)",
                            "额度重置",
                            "上游月用量(credits_used)",
                        ],
                        &build_provider_list_rows(&list.accounts),
                    )
                );
                if list
                    .accounts
                    .iter()
                    .any(|account| account.provider_kind == "copilot")
                {
                    println!("Copilot 上游月用量取 credits_used；AIU 为本机累计，二者不混算。Business 的零上限/余额和 unlimited 不能用于推断总 credits 或组织余额；缺失额度显示未知。API Token 自动续期，其到期时间不是订阅到期时间。详情见 account show。");
                }
            }
            if !json {
                for (provider, error) in &list.errors {
                    eprintln!("{provider}: {error}");
                }
            }
            if !list.complete {
                return Err(anyhow!("部分提供源的账号查询失败"));
            }
        }
        AccountCommand::Overage { command } => run_overage(client, command, json).await?,
        AccountCommand::Show { id, provider } => {
            let mut detail: ProviderAccountSummary = client
                .call(
                    method::V2_ACCOUNT_SHOW,
                    serde_json::json!({"id": id,"provider":provider}),
                )
                .await?;
            enrich_account(&mut detail);
            if json {
                print_json(&detail)?;
            } else {
                print_provider_detail(&detail);
            }
        }
        AccountCommand::Add {
            provider,
            auth,
            token_stdin,
            username,
            password_stdin,
            sso_username,
            headless,
            batch,
            sso_start_url,
            region: _,
            credentials_stdin,
            capture_headers,
            label,
        } => {
            if token_stdin {
                let token = read_import_source(None, true).await?;
                let token = token.trim();
                if token.is_empty() || token.chars().any(char::is_whitespace) {
                    return Err(anyhow!("GitHub token 不能为空或包含空白字符"));
                }
                let result: serde_json::Value = client
                    .call(
                        method::V2_ACCOUNT_IMPORT_TOKEN,
                        serde_json::json!({"provider":provider,"token":token,"label":label}),
                    )
                    .await?;
                if json {
                    print_json(&result)?;
                } else {
                    println!(
                        "已添加 {}/{}（{}）",
                        result["provider_id"].as_str().unwrap_or("-"),
                        result["id"].as_str().unwrap_or("-"),
                        result["login"].as_str().unwrap_or("-")
                    );
                }
            } else if let Some(file) = batch {
                // Validate the entire input before starting any OAuth task.
                let raw = read_sso_batch_source(&file).await?;
                let rows = copilot_input::parse_csv(&raw)?;
                run_copilot_batch(
                    client,
                    &provider,
                    rows,
                    label.as_deref(),
                    sso_start_url.as_deref(),
                    capture_headers,
                    json,
                )
                .await?;
            } else {
                let browser = if credentials_stdin {
                    let raw = read_import_source(None, true).await?;
                    let mut credentials: CopilotBrowserCredentials = serde_json::from_str(&raw)
                        .map_err(|_| anyhow!("浏览器凭证 JSON 无效，请检查字段名和类型"))?;
                    if sso_start_url.is_some() {
                        credentials.sso_start_url = sso_start_url;
                    }
                    Some(credentials)
                } else if password_stdin {
                    let username =
                        username.ok_or_else(|| anyhow!("--password-stdin 需要 --username"))?;
                    let password = read_password_line().await?;
                    let (github_password, sso_password) = if sso_username.is_some() {
                        (None, Some(password))
                    } else {
                        (Some(password), None)
                    };
                    Some(CopilotBrowserCredentials {
                        github_username: username,
                        github_password,
                        sso_username,
                        sso_password,
                        sso_start_url,
                    })
                } else if headless || username.is_some() {
                    Some(copilot_input::prompt(username, sso_username, sso_start_url).await?)
                } else {
                    None
                };
                let state = run_provider_login(
                    client,
                    &provider,
                    auth.as_deref().unwrap_or("device-flow"),
                    label.as_deref(),
                    browser,
                    capture_headers,
                    json,
                )
                .await?;
                print_provider_login(&state, &provider, json)?;
            }
        }
        AccountCommand::LoginCode {
            provider,
            task,
            code_stdin: _,
        } => {
            let code = read_password_line().await?;
            let result: serde_json::Value = client
                .call(
                    method::V2_LOGIN_SUBMIT_CODE,
                    serde_json::json!({"provider":provider,"task_id":task,"code":code}),
                )
                .await?;
            if json {
                print_json(&result)?;
            } else {
                println!("验证码已交给远端浏览器；请在原登录命令查看结果");
            }
        }
        AccountCommand::Services { id, provider } => {
            let result: AccountServicesResult = client
                .call(
                    method::ACCOUNT_SERVICES,
                    serde_json::json!({"id":id,"provider":provider}),
                )
                .await?;
            print_account_services(&result, json)?;
        }
        AccountCommand::Import { file, stdin, tags } => {
            let tags = normalize_cli_tags(tags)?;
            let raw = read_import_source(file.as_deref(), stdin).await?;
            let accounts = parse_import_payload(&raw)?;
            let result: AccountImportResult = client
                .call(
                    method::ACCOUNT_IMPORT,
                    serde_json::json!({"accounts": accounts, "tags":tags}),
                )
                .await?;
            if json {
                print_json(&result)?;
            } else {
                println!("已导入 {} 个账号", result.imported);
                if !result.skipped.is_empty() {
                    println!(
                        "跳过 {} 个（ID 或邮箱已存在）：{}",
                        result.skipped.len(),
                        result.skipped.join(", ")
                    );
                }
            }
        }
        AccountCommand::AddApiKey {
            email,
            region,
            key_stdin,
            tags,
        } => {
            let tags = normalize_cli_tags(tags)?;
            let key = if key_stdin {
                read_import_source(None, true).await?
            } else {
                std::env::var("KIRO_API_KEY").context("请设置 KIRO_API_KEY 或使用 --key-stdin")?
            };
            let key = key.trim();
            if !key.starts_with("ksk_") || key.len() <= 4 || key.chars().any(char::is_whitespace) {
                return Err(anyhow!("Kiro API key 必须是非空的 ksk_... 凭据"));
            }
            let accounts = parse_import_payload(
                &serde_json::json!({
                    "email":email, "credentials":{
                        "access_token":key, "region":region, "expires_at":0, "auth_method":"api_key"
                    }
                })
                .to_string(),
            )?;
            let result: AccountImportResult = client
                .call(
                    method::ACCOUNT_IMPORT,
                    serde_json::json!({"accounts":accounts,"tags":tags}),
                )
                .await?;
            if json {
                print_json(&result)?;
            } else {
                println!(
                    "已导入 {} 个 API key 账号，跳过 {} 个",
                    result.imported,
                    result.skipped.len()
                );
            }
        }
        AccountCommand::Export {
            redact: _,
            include_secrets,
            provider,
        } => {
            let accounts: serde_json::Value = client
                .call(
                    if provider.is_some() {
                        method::V2_ACCOUNT_EXPORT
                    } else {
                        method::ACCOUNT_EXPORT
                    },
                    serde_json::json!({"redact":!include_secrets,"provider":provider}),
                )
                .await?;
            print_json(&accounts)?;
        }
        AccountCommand::AddSso {
            email,
            start_url,
            region,
            password_stdin,
            batch,
            concurrency,
            headful,
            tags,
        } => {
            let tags = normalize_cli_tags(tags)?;
            let start_url = resolve_start_url(client, start_url.as_deref()).await?;
            if let Some(file) = batch {
                let concurrency = concurrency.unwrap_or(1);
                run_sso_batch(
                    client,
                    &file,
                    SsoBatchOptions {
                        start_url: &start_url,
                        region: &region,
                        concurrency,
                        headful,
                        tags: &tags,
                        json,
                    },
                )
                .await?;
                return Ok(());
            }
            if !password_stdin {
                return Err(anyhow!("SSO 密码只能通过 --password-stdin 提供"));
            }
            let email = email.ok_or_else(|| anyhow!("单账号登录需提供 --email"))?;
            let password = read_password_line().await?;
            let result: AccountSummary = client
                .call(
                    method::ACCOUNT_ADD_SSO,
                    serde_json::json!({
                        "email":email,
                        "password":password,
                        "start_url":start_url,
                        "region":region,
                        "headful":headful,
                        "tags":tags
                    }),
                )
                .await?;
            if json {
                print_json(&result)?;
            } else {
                println!("已添加 {}（{}）", result.email, result.id);
            }
        }
        AccountCommand::Rm { ids, provider, yes } => {
            if !crate::commands::confirm_unless(yes, &remove_confirmation_prompt(&ids)).await? {
                if json {
                    print_json(&serde_json::json!({
                        "removed": false,
                        "cancelled": true,
                        "accounts": ids
                    }))?;
                } else {
                    println!("已取消");
                }
                return Ok(());
            }
            remove_accounts(client, &ids, provider.as_deref(), json).await?;
        }
        AccountCommand::Enable { id, provider } => {
            set_enabled(client, &id, provider.as_deref(), true, json).await?
        }
        AccountCommand::Disable { id, provider } => {
            set_enabled(client, &id, provider.as_deref(), false, json).await?
        }
        AccountCommand::Tag {
            ids,
            add,
            remove,
            provider,
        } => {
            tag_accounts(client, &ids, &add, &remove, provider.as_deref(), json).await?;
        }
        AccountCommand::RegenMachineId { id } => {
            let result: serde_json::Value = client
                .call(
                    method::ACCOUNT_REGEN_MACHINE_ID,
                    serde_json::json!({"id": id}),
                )
                .await?;
            if json {
                print_json(&result)?;
            } else {
                println!(
                    "{id} 新设备标识：{}",
                    result["machine_id"].as_str().unwrap_or("-")
                );
            }
        }
        AccountCommand::Refresh { id, all, provider } => {
            if all {
                let provider = provider.as_deref().unwrap_or("all");
                let list: ProviderAccountListResult = client
                    .call(
                        method::V2_ACCOUNT_LIST,
                        serde_json::json!({"provider":provider}),
                    )
                    .await?;
                let mut failures = list
                    .errors
                    .iter()
                    .map(|(provider, error)| format!("{provider}: {error}"))
                    .collect::<Vec<_>>();
                let mut results = Vec::new();
                for account in list.accounts {
                    match client
                        .call::<serde_json::Value>(
                            method::V2_ACCOUNT_REFRESH,
                            serde_json::json!({
                                "provider":account.provider_id,
                                "id":account.id
                            }),
                        )
                        .await
                    {
                        Ok(result) => results.push(result),
                        Err(error) => failures.push(error.to_string()),
                    }
                }
                if json {
                    print_json(&serde_json::json!({"results":results,"errors":failures}))?;
                } else {
                    println!("已刷新 {} 个账号", results.len());
                }
                if !failures.is_empty() {
                    return Err(anyhow!(
                        "{} 个账号刷新失败：{}",
                        failures.len(),
                        failures.join("; ")
                    ));
                }
            } else {
                let result: serde_json::Value = client
                    .call(
                        method::V2_ACCOUNT_REFRESH,
                        serde_json::json!({"id":id,"provider":provider}),
                    )
                    .await?;
                print_result(&result, json, "账号 token 已刷新")?;
            }
        }
        AccountCommand::Probe { id, all, provider } => {
            let result = if all {
                run_provider_account_batch(
                    client,
                    method::V2_ACCOUNT_PROBE,
                    provider.as_deref().unwrap_or("all"),
                )
                .await?
            } else {
                client
                    .call(
                        method::V2_ACCOUNT_PROBE,
                        serde_json::json!({"id":id,"provider":provider}),
                    )
                    .await?
            };
            if json {
                print_json(&result)?;
            } else {
                let model_count = result["models"].as_array().map_or_else(
                    || {
                        result["results"].as_array().map_or(0, |results| {
                            results
                                .iter()
                                .map(|entry| entry["models"].as_array().map_or(0, Vec::len))
                                .sum()
                        })
                    },
                    Vec::len,
                );
                println!(
                    "账号 {} 探测成功，可用模型 {} 个",
                    id.as_deref().unwrap_or("all"),
                    model_count
                );
            }
            fail_provider_account_batch(&result, "探测")?;
        }
        AccountCommand::ResetHealth { id, all, provider } => {
            let result = if all {
                run_provider_account_batch(
                    client,
                    method::V2_ACCOUNT_RESET_HEALTH,
                    provider.as_deref().unwrap_or("all"),
                )
                .await?
            } else {
                client
                    .call(
                        method::V2_ACCOUNT_RESET_HEALTH,
                        serde_json::json!({"id":id,"provider":provider}),
                    )
                    .await?
            };
            print_result(&result, json, "账号健康状态已重置")?;
            fail_provider_account_batch(&result, "重置健康状态")?;
        }
    }
    Ok(())
}

/// Keep one entry point while reusing the established provider authentication
/// implementations. Legacy add-sso/add-api-key commands remain compatible.
fn normalize_add(command: AccountCommand) -> Result<AccountCommand> {
    match command {
        AccountCommand::Add {
            provider,
            auth,
            token_stdin,
            username,
            password_stdin,
            sso_username,
            headless,
            batch,
            sso_start_url,
            region,
            credentials_stdin,
            capture_headers,
            label,
        } if provider == "kiro" => {
            if sso_username.is_some()
                || credentials_stdin
                || capture_headers
                || label.is_some()
                || headless
            {
                return Err(anyhow!("该参数用于 Copilot；Kiro 使用 --email、--start-url、--password-stdin 或 --batch"));
            }
            match auth
                .as_deref()
                .unwrap_or(if token_stdin { "api-key" } else { "sso" })
            {
                "api-key" => {
                    if batch.is_some() || password_stdin || sso_start_url.is_some() {
                        return Err(anyhow!("api-key 不支持 SSO 参数"));
                    }
                    Ok(AccountCommand::AddApiKey {
                        email: username.ok_or_else(|| anyhow!("Kiro API key 需要 --email"))?,
                        region,
                        key_stdin: token_stdin,
                        tags: vec![],
                    })
                }
                "sso" if !token_stdin => {
                    if batch.is_none() && (username.is_none() || !password_stdin) {
                        return Err(anyhow!(
                            "Kiro SSO 需要 --email 与 --password-stdin，或 --batch"
                        ));
                    }
                    Ok(AccountCommand::AddSso {
                        email: username,
                        start_url: sso_start_url,
                        region,
                        password_stdin,
                        batch,
                        concurrency: None,
                        headful: false,
                        tags: vec![],
                    })
                }
                _ => Err(anyhow!("Kiro 支持 --auth sso 或 --auth api-key")),
            }
        }
        AccountCommand::Add {
            ref provider,
            ref auth,
            token_stdin,
            ref region,
            ref username,
            ref sso_username,
            ref sso_start_url,
            ..
        } => {
            if provider == "all" {
                return Err(anyhow!("添加账号需要明确的 --provider 实例 ID"));
            }
            if auth.as_deref().is_some_and(|auth| auth != "device-flow")
                || (token_stdin && auth.is_some())
            {
                return Err(anyhow!("Copilot 使用 --auth device-flow 或 --token-stdin"));
            }
            if token_stdin
                && (username.is_some() || sso_username.is_some() || sso_start_url.is_some())
            {
                return Err(anyhow!("Copilot token 导入不能同时提供浏览器登录参数"));
            }
            if region != "us-east-1" {
                return Err(anyhow!("--region 仅适用于 Kiro"));
            }
            Ok(command)
        }
        _ => Ok(command),
    }
}

fn empty_account_hint(provider: &str, filtered: bool) -> String {
    if filtered {
        return "没有符合筛选条件的账号。".into();
    }
    match provider {
        "all" => "暂无账号。Kiro 接入见 `crosslane guide kiro`；GitHub Copilot 接入见 `crosslane guide copilot`。".into(),
        "kiro" => "暂无 Kiro 账号。使用 `crosslane account add --provider kiro --auth sso --email <EMAIL> --password-stdin`，或 --auth api-key；详情见 `crosslane guide kiro`。".into(),
        _ => format!("实例 {provider} 暂无账号。使用 `crosslane account add --provider {provider}` 自动选择认证方式；查看 `crosslane provider show {provider}` 获取该实例的接入命令。"),
    }
}

async fn run_provider_account_batch(
    client: &mut AdminClient,
    method_name: &str,
    provider: &str,
) -> Result<serde_json::Value> {
    let list: ProviderAccountListResult = client
        .call(
            method::V2_ACCOUNT_LIST,
            serde_json::json!({"provider":provider}),
        )
        .await?;
    let mut results = Vec::new();
    let mut errors = list
        .errors
        .iter()
        .map(|(provider, error)| format!("{provider}: {error}"))
        .collect::<Vec<_>>();
    for account in list.accounts {
        match client
            .call::<serde_json::Value>(
                method_name,
                serde_json::json!({
                    "provider":account.provider_id,
                    "id":account.id
                }),
            )
            .await
        {
            Ok(result) => results.push(result),
            Err(error) => errors.push(error.to_string()),
        }
    }
    Ok(serde_json::json!({
        "scope":{"provider":provider},
        "results":results,
        "errors":errors,
        "complete":errors.is_empty()
    }))
}

fn fail_provider_account_batch(value: &serde_json::Value, operation: &str) -> Result<()> {
    let errors = value["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect::<Vec<_>>();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "{} 个账号{}失败：{}",
            errors.len(),
            operation,
            errors.join("; ")
        ))
    }
}

async fn resolve_start_url(client: &mut AdminClient, explicit: Option<&str>) -> Result<String> {
    if let Some(url) = explicit.map(str::trim).filter(|url| !url.is_empty()) {
        return validate_start_url(url);
    }
    let show: ConfigShowResult = client
        .call(method::CONFIG_SHOW, serde_json::json!({}))
        .await?;
    let config: crosslane_core::config::Config =
        serde_json::from_value(show.effective_json).context("daemon 返回的生效配置无效")?;
    let configured = config.sso.start_url.trim();
    if configured.is_empty() {
        return Err(anyhow!(
            "需提供 --start-url，或在配置文件中设置 [sso].start_url"
        ));
    }
    validate_start_url(configured)
}

fn normalize_cli_tags(tags: Vec<String>) -> Result<Vec<String>> {
    let mut normalized = Vec::new();
    for tag in tags {
        let tag = tag.trim();
        if tag.is_empty() {
            return Err(anyhow!("账号标签不能为空"));
        }
        if !normalized.iter().any(|existing| existing == tag) {
            normalized.push(tag.to_owned());
        }
    }
    normalized.sort();
    Ok(normalized)
}

fn validate_start_url(url: &str) -> Result<String> {
    if !url.starts_with("https://") {
        return Err(anyhow!("SSO start URL 必须使用 https://"));
    }
    Ok(url.to_owned())
}

fn print_result(value: &serde_json::Value, json: bool, message: &str) -> Result<()> {
    if json {
        print_json(value)?;
    } else {
        println!("{message}");
    }
    Ok(())
}

fn remove_confirmation_prompt(ids: &[String]) -> String {
    if let [id] = ids {
        format!("确认删除账号 {id}？")
    } else {
        format!("确认删除以下 {} 个账号：{}？", ids.len(), ids.join("、"))
    }
}

async fn remove_accounts(
    client: &mut AdminClient,
    ids: &[String],
    provider: Option<&str>,
    json: bool,
) -> Result<()> {
    if let [id] = ids {
        let result: serde_json::Value = client
            .call(
                method::V2_ACCOUNT_REMOVE,
                serde_json::json!({"id": id,"provider":provider}),
            )
            .await?;
        if json {
            print_json(&result)?;
        } else {
            println!("已删除 {id}");
        }
        return Ok(());
    }

    let mut failures = Vec::new();
    let mut results = Vec::new();
    for id in ids {
        match client
            .call::<serde_json::Value>(
                method::V2_ACCOUNT_REMOVE,
                serde_json::json!({"id": id,"provider":provider}),
            )
            .await
        {
            Ok(result) => {
                if !json {
                    println!("已删除 {id}");
                }
                results.push(result);
            }
            Err(error) => failures.push(format!("{id}: {error}")),
        }
    }
    if json {
        print_json(&serde_json::json!({
            "results": results,
            "errors": failures,
            "complete": failures.is_empty()
        }))?;
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "{} 个账号删除失败：\n{}",
            failures.len(),
            failures.join("\n")
        ))
    }
}

/// Tag one or more accounts.
///
/// Resolve every selector on the daemon before applying a provider-local batch.
async fn tag_accounts(
    client: &mut AdminClient,
    ids: &[String],
    add: &[String],
    remove: &[String],
    provider: Option<&str>,
    json: bool,
) -> Result<()> {
    if let [id] = ids {
        let result: AccountTagResult = client
            .call(
                method::V2_ACCOUNT_TAG,
                serde_json::json!({"id": id, "add": add, "remove": remove, "provider": provider}),
            )
            .await?;
        if json {
            print_json(&result)?;
        } else {
            print_tag_result(&result);
        }
    } else {
        let result: AccountTagBatchResult = client
            .call(
                method::V2_ACCOUNT_TAG,
                serde_json::json!({"ids": ids, "add": add, "remove": remove, "provider": provider}),
            )
            .await?;
        if json {
            print_json(&result)?;
        } else {
            for account in &result.accounts {
                print_tag_result(account);
            }
        }
    }
    Ok(())
}

fn print_tag_result(result: &AccountTagResult) {
    let tags = result.tags.join(",");
    println!(
        "{} 当前标签：{}",
        result.id,
        if tags.is_empty() { "-" } else { &tags }
    );
}

async fn set_enabled(
    client: &mut AdminClient,
    id: &str,
    provider: Option<&str>,
    enabled: bool,
    json: bool,
) -> Result<()> {
    let result: serde_json::Value = client
        .call(
            method::V2_ACCOUNT_SET_ENABLED,
            serde_json::json!({"id": id, "provider":provider, "enabled": enabled}),
        )
        .await?;
    if json {
        print_json(&result)?;
    } else {
        println!("{id} 已{}", if enabled { "启用" } else { "停用" });
    }
    Ok(())
}

async fn read_import_source(file: Option<&str>, stdin: bool) -> Result<String> {
    if stdin {
        use tokio::io::AsyncReadExt;
        let mut buffer = String::new();
        tokio::io::stdin()
            .read_to_string(&mut buffer)
            .await
            .context("读取标准输入失败")?;
        return Ok(buffer);
    }
    let path = file.ok_or_else(|| anyhow!("需指定 --file <PATH> 或 --stdin"))?;
    tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("读取 {path} 失败"))
}

async fn read_password_line() -> Result<String> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut password = String::new();
    BufReader::new(tokio::io::stdin())
        .read_line(&mut password)
        .await
        .context("读取标准输入密码失败")?;
    while matches!(password.chars().last(), Some('\n' | '\r')) {
        password.pop();
    }
    if password.is_empty() {
        return Err(anyhow!("密码不能为空"));
    }
    Ok(password)
}

struct SsoBatchOptions<'a> {
    start_url: &'a str,
    region: &'a str,
    concurrency: usize,
    headful: bool,
    tags: &'a [String],
    json: bool,
}

async fn run_sso_batch(
    client: &AdminClient,
    file: &str,
    options: SsoBatchOptions<'_>,
) -> Result<()> {
    use futures::{stream, StreamExt};

    if !(1..=8).contains(&options.concurrency) {
        return Err(anyhow!("并发数必须在 1..=8 之间"));
    }
    let raw = read_sso_batch_source(file).await?;
    let rows = parse_sso_csv(&raw)?;
    let socket = client.socket_path();
    let start_url = options.start_url.to_string();
    let region = options.region.to_string();
    let tags = options.tags.to_vec();
    let headful = options.headful;
    let results = stream::iter(rows.into_iter().map(|(email, password)| {
        let socket = socket.clone();
        let start_url = start_url.clone();
        let region = region.clone();
        let tags = tags.clone();
        async move {
            let mut client = AdminClient::connect(socket);
            let result = client
                .call::<AccountSummary>(
                    method::ACCOUNT_ADD_SSO,
                    serde_json::json!({
                        "email":email,
                        "password":password,
                        "start_url":start_url,
                        "region":region,
                        "headful":headful,
                        "tags":tags
                    }),
                )
                .await;
            (email, result)
        }
    }))
    .buffer_unordered(options.concurrency)
    .collect::<Vec<_>>()
    .await;
    let mut failures = Vec::new();
    let mut successes = Vec::new();
    for (email, result) in results {
        match result {
            Ok(summary) => {
                if !options.json {
                    println!("已添加 {}（{}）", summary.email, summary.id);
                }
                successes.push(summary);
            }
            Err(error) => failures.push(format!("{email}: {error}")),
        }
    }
    if options.json {
        print_json(&serde_json::json!({
            "accounts": successes,
            "errors": failures,
            "complete": failures.is_empty()
        }))?;
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "{} 个账号登录失败：\n{}",
            failures.len(),
            failures.join("\n")
        ))
    }
}

const WRAPPER_BATCH_STDIN_ENV: &str = "CROSSLANE_WRAPPER_BATCH_STDIN";

fn sso_batch_reads_stdin(file: &str, wrapper_override: Option<&std::ffi::OsStr>) -> bool {
    file == "-" || wrapper_override == Some(std::ffi::OsStr::new("1"))
}

async fn read_sso_batch_source(file: &str) -> Result<String> {
    if sso_batch_reads_stdin(file, std::env::var_os(WRAPPER_BATCH_STDIN_ENV).as_deref()) {
        use tokio::io::AsyncReadExt;

        let mut raw = String::new();
        tokio::io::stdin()
            .read_to_string(&mut raw)
            .await
            .context("读取批量 CSV 标准输入失败")?;
        return Ok(raw);
    }
    tokio::fs::read_to_string(file)
        .await
        .with_context(|| format!("读取 {file} 失败"))
}

fn parse_sso_csv(raw: &str) -> Result<Vec<(String, String)>> {
    let mut rows = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let fields =
            parse_csv_line(line).with_context(|| format!("CSV 第 {} 行无效", index + 1))?;
        if index == 0
            && fields
                .first()
                .map(|field| field.eq_ignore_ascii_case("email"))
                == Some(true)
        {
            continue;
        }
        if fields.len() != 2 || fields[0].trim().is_empty() || fields[1].is_empty() {
            return Err(anyhow!(
                "CSV 第 {} 行必须只有 email,password 两列",
                index + 1
            ));
        }
        rows.push((fields[0].trim().to_ascii_lowercase(), fields[1].clone()));
    }
    if rows.is_empty() {
        return Err(anyhow!("CSV 中没有账号"));
    }
    Ok(rows)
}

fn parse_csv_line(line: &str) -> Result<Vec<String>> {
    let mut fields = vec![String::new()];
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '"' if quoted && chars.peek() == Some(&'"') => {
                fields
                    .last_mut()
                    .ok_or_else(|| anyhow!("CSV 解析状态无效"))?
                    .push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(String::new()),
            other => fields
                .last_mut()
                .ok_or_else(|| anyhow!("CSV 解析状态无效"))?
                .push(other),
        }
    }
    if quoted {
        return Err(anyhow!("未闭合的引号"));
    }
    Ok(fields)
}

fn print_account_services(result: &AccountServicesResult, json: bool) -> Result<()> {
    if json {
        return print_json(result);
    }
    if result.services.is_empty() {
        println!(
            "账号 {}（{}）未绑定任何 API 代理服务。",
            result.account_email, result.account_id
        );
        return Ok(());
    }
    let rows = result
        .services
        .iter()
        .map(|service| {
            vec![
                service.service_id.clone(),
                service.service_name.clone(),
                format!("{}:{}", service.host, service.port),
                if service.running {
                    "运行中".into()
                } else if service.enabled {
                    "未运行".into()
                } else {
                    "已停用".into()
                },
                display_binding_sources(service),
            ]
        })
        .collect::<Vec<_>>();
    println!(
        "账号      {}（{}）",
        result.account_email, result.account_id
    );
    println!(
        "{}",
        render_table(&["服务 ID", "名称", "监听", "状态", "绑定来源"], &rows)
    );
    Ok(())
}

fn display_binding_sources(service: &AccountServiceBinding) -> String {
    service
        .binding_sources
        .iter()
        .map(|source| {
            source
                .strip_prefix("tag:")
                .map(|tag| format!("标签:{tag}"))
                .unwrap_or_else(|| match source.as_str() {
                    "global" => "全局池".into(),
                    "manual" => "手工".into(),
                    other => other.to_owned(),
                })
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unified_add_dispatches_kiro_auth_and_rejects_wrong_provider_modes() {
        use clap::Parser;
        let parse = |args: &[&str]| {
            let cli = crate::Cli::try_parse_from(args).unwrap();
            let Some(crate::Command::Account {
                command: Some(command),
            }) = cli.command
            else {
                panic!("account command");
            };
            normalize_add(command)
        };
        let sso = parse(&[
            "crosslane",
            "--provider",
            "kiro",
            "account",
            "add",
            "--auth",
            "sso",
            "--email",
            "test@example.com",
            "--password-stdin",
            "--start-url",
            "https://example.awsapps.com/start",
        ])
        .unwrap();
        assert!(
            matches!(sso,AccountCommand::AddSso { email:Some(email), password_stdin:true, .. } if email == "test@example.com")
        );
        let key = parse(&[
            "crosslane",
            "account",
            "add",
            "--provider",
            "kiro",
            "--email",
            "test@example.com",
            "--token-stdin",
        ])
        .unwrap();
        assert!(matches!(
            key,
            AccountCommand::AddApiKey {
                key_stdin: true,
                ..
            }
        ));
        assert!(parse(&[
            "crosslane",
            "--provider",
            "copilot",
            "account",
            "add",
            "--auth",
            "sso"
        ])
        .is_err());
        assert!(parse(&[
            "crosslane",
            "--provider",
            "all",
            "account",
            "add",
            "--auth",
            "device-flow"
        ])
        .is_err());
        assert!(matches!(
            parse(&[
                "crosslane",
                "--provider",
                "copilot",
                "account",
                "add",
                "--auth",
                "device-flow"
            ])
            .unwrap(),
            AccountCommand::Add { .. }
        ));
    }

    #[test]
    fn empty_account_hints_match_provider_auth_flow() {
        let all = empty_account_hint("all", false);
        assert!(all.contains("guide kiro"));
        assert!(all.contains("guide copilot"));

        let kiro = empty_account_hint("kiro", false);
        assert!(kiro.contains("account add --provider kiro --auth sso"));
        assert!(!kiro.contains("device-flow"));

        let copilot = empty_account_hint("copilot", false);
        assert!(copilot.contains("account add --provider copilot`"));
        let custom = empty_account_hint("work-cp", false);
        assert!(custom.contains("account add --provider work-cp`"));
        assert!(custom.contains("provider show work-cp"));
        assert!(!custom.contains("--provider copilot"));

        assert_eq!(
            empty_account_hint("copilot", true),
            "没有符合筛选条件的账号。"
        );
    }

    fn summary(enabled: bool) -> AccountSummary {
        AccountSummary {
            id: "acc_00000001".into(),
            email: "a@example.com".into(),
            label: None,
            enabled,
            health: Some(if enabled { "available" } else { "disabled" }.into()),
            tags: vec!["prod".into()],
            subscription: Some("Pro".into()),
            credit_current: Some(120.126),
            credit_limit: Some(500.5),
            token_expires_at: 1_767_225_600,
            credit_exhausted: false,
        }
    }

    #[test]
    fn list_rows_cover_status_usage_and_tags() {
        let mut enabled = summary(true);
        enabled.tags.push("pro".into());
        let disabled = summary(false);
        let rows = build_list_rows(&[enabled, disabled]);
        assert_eq!(rows[0][2], "启用");
        assert_eq!(rows[1][2], "停用");
        assert_eq!(rows[0][3], "120.13/500.50");
        assert_eq!(rows[0][5], "prod,pro");

        let mut exhausted = summary(true);
        exhausted.credit_exhausted = true;
        assert_eq!(build_list_rows(&[exhausted])[0][2], "额度耗尽");

        let mut protected = summary(true);
        protected.health = Some("low_credit".into());
        assert_eq!(build_list_rows(&[protected])[0][2], "低额度保护");
    }

    #[test]
    fn copilot_list_rows_show_token_expiry_native_quota_and_exact_local_aiu() {
        let account = |info, usage| ProviderAccountSummary {
            provider_id: "copilot".into(),
            provider_kind: "copilot".into(),
            id: "acc_test".into(),
            display_name: "octocat".into(),
            email: None,
            label: None,
            enabled: true,
            health: "available".into(),
            auth_state: "ready".into(),
            tags: Vec::new(),
            quota_current: None,
            quota_limit: None,
            quota_unit: None,
            supported_models: Vec::new(),
            details: serde_json::json!({"info":info,"local_aiu_usage":usage}),
        };
        let info = serde_json::json!({
            "plan":"business","api_token_expires_at":1_767_225_600,
            "token_based_billing":true,"quota_reset_date":"2026-11-01T00:00:00.000Z",
            "quota_snapshots":{"premium_interactions":{"unlimited":true,"entitlement":0,"remaining":0,"credits_used":0}}
        });
        let rows = build_provider_list_rows(&[
            account(
                info.clone(),
                serde_json::json!({"total_nano_aiu":17_000_000}),
            ),
            account(serde_json::Value::Null, serde_json::Value::Null),
            account(info, serde_json::json!({"total_nano_aiu":0})),
        ]);
        assert_eq!(rows[0][5], "总额/余额未知");
        assert_eq!(rows[0][8], "business");
        assert_eq!(rows[0][9], "2026-01-01T00:00:00Z");
        assert_eq!(rows[0][10], "0.017");
        assert_eq!(rows[0][11], "2026-11-01T00:00:00.000Z");
        assert_eq!(rows[0][12], "0");
        assert_eq!(rows[1][5], "未知");
        assert_eq!(rows[1][9], "未知");
        assert_eq!(rows[1][10], "未知");
        assert_eq!(rows[1][12], "未知");
        assert_eq!(rows[2][10], "0");
        assert_eq!(format_nano_aiu(u64::MAX), "18446744073.709551615");
        assert_eq!(format_nano_aiu(1), "0.000000001");
        assert_eq!(format_nano_aiu(2_000_000_000), "2");
        assert_eq!(
            copilot_upstream_usage_display(&serde_json::json!({
                "quota_snapshots":{"premium_interactions":{"credits_used":1}}
            })),
            "1"
        );
        assert_eq!(
            copilot_quota_display(&serde_json::json!({
                "quota_snapshots":{"premium_interactions":{"entitlement":300,"remaining":240}},
                "quota_error":"HTTP 429"
            })),
            "60.00/300.00 请求(缓存)"
        );
        assert_eq!(
            copilot_quota_display(&serde_json::json!({"token_based_billing":true,
                "quota_snapshots":{"premium_interactions":{
                    "unlimited":false,"entitlement":1500,"remaining":1498,
                    "quota_remaining":1498.75,"credits_used":1}}
            })),
            "1/1500 credits (余 1498.75)"
        );
        assert_eq!(
            copilot_quota_display(&serde_json::json!({"token_based_billing":true,
                "quota_error":"HTTP 429","quota_snapshots":{"premium_interactions":{
                    "unlimited":true,"entitlement":0,"remaining":0,
                    "percent_remaining":100,"credits_used":1}}
            })),
            "总额/余额未知(缓存)"
        );
        assert_eq!(
            copilot_upstream_usage_display(&serde_json::json!({
                "quota_snapshots":{"premium_interactions":{"credits_used":-1}}
            })),
            "未知"
        );
    }

    #[test]
    fn provider_list_rows_show_configured_and_kiro_overage_caps() {
        let account = ProviderAccountSummary {
            provider_id: "kiro".into(),
            provider_kind: "kiro".into(),
            id: "acc_00000001".into(),
            display_name: "a@example.com".into(),
            email: Some("a@example.com".into()),
            label: None,
            enabled: true,
            health: "available".into(),
            auth_state: "ready".into(),
            tags: vec!["prod".into()],
            quota_current: Some(10_000.01),
            quota_limit: Some(10_500.0),
            quota_unit: Some("kiro_credits".into()),
            supported_models: Vec::new(),
            details: serde_json::json!({
                "overage_enabled": true,
                "max_overage_credits_per_account": 500.0,
                "kiro_overage_cap": 10_000.0,
                "kiro_overage_total_limit": 20_000.0,
                "effective_overage_cap": 500.0
            }),
        };
        let rows = build_provider_list_rows(&[account]);
        assert_eq!(rows[0][1], "Kiro (kiro)");
        assert_eq!(rows[0][4], "可用");
        assert_eq!(rows[0][5], "10000.01/10500.00/20000.00");
        assert_eq!(rows[0][6], "500.00/10000.00");
        assert_eq!(rows[0][7], "prod");
    }

    #[test]
    fn provider_list_rows_distinguish_disabled_and_uncapped_overage() {
        let account = |enabled, cap| ProviderAccountSummary {
            provider_id: "kiro".into(),
            provider_kind: "kiro".into(),
            id: "acc_00000001".into(),
            display_name: "a@example.com".into(),
            email: Some("a@example.com".into()),
            label: None,
            enabled: true,
            health: "available".into(),
            auth_state: "ready".into(),
            tags: Vec::new(),
            quota_current: Some(10_000.0),
            quota_limit: Some(20_000.0),
            quota_unit: Some("kiro_credits".into()),
            supported_models: Vec::new(),
            details: serde_json::json!({
                "overage_enabled": enabled,
                "max_overage_credits_per_account": cap,
                "kiro_overage_cap": 10_000.0
            }),
        };
        let rows = build_provider_list_rows(&[
            account(false, Some(500.0)),
            account(true, Option::<f64>::None),
        ]);
        assert_eq!(rows[0][6], "关闭(500.00)/10000.00");
        assert_eq!(rows[1][6], "Kiro上限/10000.00");
    }

    #[test]
    fn overage_credit_parser_rejects_negative_and_non_finite_values() {
        assert_eq!(parse_non_negative_credits("0").expect("zero"), 0.0);
        assert_eq!(
            parse_non_negative_credits("500.25").expect("credits"),
            500.25
        );
        assert!(parse_non_negative_credits("-0.01").is_err());
        assert!(parse_non_negative_credits("NaN").is_err());
        assert!(parse_non_negative_credits("inf").is_err());
        assert!(parse_non_negative_credits("lots").is_err());
    }

    #[test]
    fn overage_rows_keep_machine_values_raw_and_human_status_localized() {
        let account = OverageAccountView {
            id: "acc_00000001".into(),
            account: "a@example.com".into(),
            enabled: true,
            health: "available".into(),
            current: Some(10_000.01),
            proxy_total: Some(10_500.0),
            kiro_total: Some(20_000.0),
            effective_overage: Some(500.0),
            kiro_overage: Some(10_000.0),
        };
        let rows = build_overage_rows(&[account]);
        assert_eq!(rows[0][2], "可用");
        assert_eq!(rows[0][3], "10000.01/10500.00/20000.00");
        assert_eq!(rows[0][4], "500.00/10000.00");
    }

    #[test]
    fn import_accepts_object_and_array_and_fills_ids() {
        let object = r#"{
            "email": "a@example.com",
            "credentials": {
                "access_token": "at",
                "region": "us-east-1",
                "expires_at": 0,
                "auth_method": "idc"
            }
        }"#;
        let account = parse_import_payload(object).expect("object");
        assert_eq!(account.len(), 1);
        assert!(account[0].id.starts_with("acc_"));
        assert_eq!(account[0].machine_id.len(), 64);
        assert!(account[0].created_at > 0);
        let array = format!("[{object}]");
        assert_eq!(parse_import_payload(&array).expect("array").len(), 1);
    }

    #[test]
    fn import_rejects_missing_email_and_malformed_ids() {
        assert!(parse_import_payload(
            r#"{"credentials":{"access_token":"at","region":"us-east-1","expires_at":0,"auth_method":"idc"}}"#
        )
        .is_err());
        assert!(parse_import_payload("{not json").is_err());
        assert!(parse_import_payload(
            r#"{"id":"bad","email":"a@example.com","credentials":{"access_token":"at","region":"us-east-1","expires_at":0,"auth_method":"idc"}}"#
        )
        .is_err());
    }

    #[test]
    fn account_creation_tags_are_trimmed_sorted_and_deduplicated() {
        assert_eq!(
            normalize_cli_tags(vec![" prod ".into(), "team-a".into(), "prod".into()])
                .expect("tags"),
            ["prod", "team-a"]
        );
        assert!(normalize_cli_tags(vec![" ".into()]).is_err());

        let binding = AccountServiceBinding {
            service_id: "svc_1".into(),
            service_name: "team".into(),
            host: "127.0.0.1".into(),
            port: 5581,
            enabled: true,
            running: true,
            binding_sources: vec!["tag:team-a".into(), "manual".into()],
        };
        assert_eq!(display_binding_sources(&binding), "标签:team-a,手工");
    }

    #[test]
    fn sso_csv_supports_headers_quotes_and_commas() {
        let rows =
            parse_sso_csv("email,password\na@example.com,secret\n\"b@example.com\",\"p,a\"\n")
                .expect("csv");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1], ("b@example.com".into(), "p,a".into()));
        assert!(parse_sso_csv("a@example.com,\"unterminated").is_err());
    }

    #[test]
    fn sso_batch_accepts_explicit_and_wrapper_managed_stdin() {
        assert!(sso_batch_reads_stdin("-", None));
        assert!(sso_batch_reads_stdin(
            "accounts.csv",
            Some(std::ffi::OsStr::new("1"))
        ));
        assert!(!sso_batch_reads_stdin(
            "accounts.csv",
            Some(std::ffi::OsStr::new("0"))
        ));
        assert!(!sso_batch_reads_stdin("accounts.csv", None));
    }

    #[test]
    fn remove_confirmation_describes_single_and_multiple_accounts() {
        assert_eq!(
            remove_confirmation_prompt(&["acc_00000001".into()]),
            "确认删除账号 acc_00000001？"
        );
        assert_eq!(
            remove_confirmation_prompt(&["acc_00000001".into(), "a@example.com".into()]),
            "确认删除以下 2 个账号：acc_00000001、a@example.com？"
        );
    }

    #[tokio::test]
    async fn batch_remove_continues_after_an_account_fails() {
        use crosslane_ipc::protocol::{decode_line, encode_line, Request, Response, RpcError};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let directory = tempfile::tempdir().expect("tempdir");
        let socket = directory.path().join("admin.sock");
        let listener = UnixListener::bind(&socket).expect("bind socket");
        let server = tokio::spawn(async move {
            let mut received = Vec::new();
            for index in 0..2 {
                let (stream, _) = listener.accept().await.expect("accept client");
                let (read_half, mut write_half) = stream.into_split();
                let raw = BufReader::new(read_half)
                    .lines()
                    .next_line()
                    .await
                    .expect("read request")
                    .expect("request line");
                let request: Request = decode_line(&raw).expect("decode request");
                received.push(
                    request.params["id"]
                        .as_str()
                        .expect("account id")
                        .to_owned(),
                );
                let response = if index == 0 {
                    Response::err(request.id, RpcError::bad_params("account not found"))
                } else {
                    Response::ok(request.id, serde_json::json!({"removed": received[index]}))
                };
                write_half
                    .write_all(encode_line(&response).expect("encode response").as_bytes())
                    .await
                    .expect("write response");
            }
            received
        });

        let mut client = AdminClient::connect(socket);
        let error = remove_accounts(
            &mut client,
            &["missing@example.com".into(), "acc_00000002".into()],
            None,
            false,
        )
        .await
        .expect_err("first removal should fail");
        assert!(error.to_string().contains("missing@example.com"));
        assert_eq!(
            server.await.expect("server task"),
            ["missing@example.com", "acc_00000002"]
        );
    }

    #[tokio::test]
    async fn batch_tag_sends_one_request_with_all_account_ids() {
        use crosslane_ipc::protocol::{decode_line, encode_line, Request, Response};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let directory = tempfile::tempdir().expect("tempdir");
        let socket = directory.path().join("admin.sock");
        let listener = UnixListener::bind(&socket).expect("bind socket");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept client");
            let (read_half, mut write_half) = stream.into_split();
            let raw = BufReader::new(read_half)
                .lines()
                .next_line()
                .await
                .expect("read request")
                .expect("request line");
            let request: Request = decode_line(&raw).expect("decode request");
            let response = Response::ok(
                request.id,
                serde_json::json!({
                    "accounts": [
                        {"id": "acc_00000001", "tags": ["test"]},
                        {"id": "acc_00000002", "tags": ["test"]}
                    ]
                }),
            );
            write_half
                .write_all(encode_line(&response).expect("encode response").as_bytes())
                .await
                .expect("write response");
            request
        });

        let mut client = AdminClient::connect(socket);
        tag_accounts(
            &mut client,
            &["acc_00000001".into(), "acc_00000002".into()],
            &["test".into()],
            &[],
            None,
            false,
        )
        .await
        .expect("batch tag");
        let request = server.await.expect("server task");
        assert_eq!(request.method, method::V2_ACCOUNT_TAG);
        assert_eq!(
            request.params,
            serde_json::json!({
                "ids": ["acc_00000001", "acc_00000002"],
                "add": ["test"],
                "remove": [],
                "provider": null
            })
        );
    }
}
