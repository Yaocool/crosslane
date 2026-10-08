//! crosslane 命令行客户端。

mod cli;
mod client;
mod commands;
mod output;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use crosslane_ipc::protocol::{method, ConfigPathResult, ConfigReloadResult, StatusResult};
use std::ffi::OsString;
use std::io::{IsTerminal, Write};

use crate::client::{resolve_socket, AdminClient};
use crate::output::{format_relative, format_timestamp, print_json, render_table};

#[derive(Debug, Parser)]
#[command(
    name = "crosslane",
    version,
    disable_help_subcommand = true,
    about = "Kiro 与 GitHub Copilot 模型代理管理工具",
    long_about = "查看 Kiro、GitHub Copilot 提供源及服务状态，管理账号、模型、API key 与配置。\n\n示例：\n  crosslane provider list\n  crosslane account list --provider kiro\n  crosslane account list --provider copilot\n  crosslane models list --provider all\n  crosslane guide kiro\n  crosslane guide copilot",
    after_help = "来源与命令适用范围：guide provider、guide kiro、guide copilot\n常用查询：status、health、ready、stats、pool、subscriptions\n命令组：provider、account、config、apikey、service、alert、logs、models、model-map、tasks、diagnose\n宿主机操作：restart、stop、uninstall\n帮助与补全：help、guide、completions"
)]
struct Cli {
    /// 提供源实例 ID（见 provider list）；可放在任意子命令前后，也可用 CROSSLANE_PROVIDER。
    #[arg(
        short = 'p',
        long,
        global = true,
        env = "CROSSLANE_PROVIDER",
        value_name = "ID",
        value_delimiter = ','
    )]
    provider: Vec<String>,
    /// 管理面 socket 路径，默认读取配置文件。
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        env = "CROSSLANE_ADMIN_SOCKET"
    )]
    socket: Option<String>,
    /// 以 JSON 输出业务数据。
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Clone, Default, Args)]
struct TimeRangeArgs {
    /// 相对当前时间的窗口，例如 30m、1h、7d。
    #[arg(long, value_name = "DURATION", conflicts_with_all = ["start", "end"])]
    since: Option<String>,
    /// 起始时间（含）；使用 Unix 秒或带时区的 RFC 3339 时间。
    #[arg(long, value_name = "TIME")]
    start: Option<String>,
    /// 结束时间（含）；使用 Unix 秒或带时区的 RFC 3339 时间。
    #[arg(long, value_name = "TIME")]
    end: Option<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Kiro/Copilot 模型提供源实例管理。
    #[command(
        after_help = "示例：\n  crosslane provider list\n  crosslane provider show kiro\n  crosslane provider show copilot\n  crosslane provider add --id copilot --kind copilot\n\n操作说明：crosslane guide provider"
    )]
    Provider {
        #[command(subcommand)]
        command: Option<ProviderCommand>,
    },
    /// Kiro/Copilot 服务总览；可按提供源过滤。
    #[command(
        after_help = "--json 与 --watch 同时使用时，每次刷新输出一行 JSON（NDJSON）。\n\n示例：\n  crosslane status\n  crosslane status --provider kiro\n  crosslane status --provider copilot\n  crosslane status --since 30m --watch"
    )]
    Status {
        /// 每 2 秒刷新。
        #[arg(long)]
        watch: bool,
        #[command(flatten)]
        range: TimeRangeArgs,
        /// 仅显示一个提供源实例；省略时聚合全部。
        #[arg(long)]
        provider: Option<String>,
    },
    /// 全局 daemon 健康检查；不代表任一来源的账号可用。
    #[command(after_help = "示例：\n  crosslane health\n  crosslane --json health")]
    Health,
    /// 检查业务代理就绪状态；可按 Kiro/Copilot 提供源过滤。
    #[command(
        after_help = "示例：\n  crosslane ready\n  crosslane ready --provider copilot\n  crosslane --json ready"
    )]
    Ready {
        /// 只检查引用该提供源的服务及账号。
        #[arg(long)]
        provider: Option<String>,
    },
    /// 显示版本与 Kiro 默认上游端点；Copilot API 地址按账号动态选择。
    #[command(after_help = "示例：\n  crosslane version\n  crosslane --json version")]
    Version,
    /// 重启 crosslaned（Docker 宿主机命令）。
    #[command(after_help = "示例：\n  crosslane restart")]
    Restart,
    /// 停止 crosslaned（Docker 宿主机命令）。
    #[command(
        after_help = "示例：\n  crosslane stop\n\n停止后可用 `crosslane restart` 重新启动。"
    )]
    Stop,
    /// 完全卸载 Docker 服务、数据卷、专用镜像和宿主机 wrapper。
    #[command(
        after_help = "示例：\n  crosslane uninstall\n  crosslane uninstall --backup-dir /srv/backups\n  crosslane uninstall --yes --keep-backup\n  crosslane uninstall --yes --delete-backup\n\n卸载前必定先备份数据。默认备份目录为 ~/.crosslane/backups；会永久删除配置、账号、统计和日志，但不删除源码目录。"
    )]
    Uninstall {
        /// 跳过交互确认，用于自动化。
        #[arg(short = 'y', long)]
        yes: bool,
        /// 宿主机备份根目录；默认 ~/.crosslane/backups。
        #[arg(long, value_name = "PATH", env = "CROSSLANE_BACKUP_DIR")]
        backup_dir: Option<String>,
        /// 卸载成功后保留备份。
        #[arg(long, conflicts_with = "delete_backup")]
        keep_backup: bool,
        /// 卸载成功后删除备份。
        #[arg(long, conflicts_with = "keep_backup")]
        delete_backup: bool,
    },
    /// 配置查看、编辑、重载与重置；模块可按适用来源查看。
    #[command(
        after_help = "`config list` 标明模块的适用来源；Kiro 专项配置与 Copilot 的 [[provider]] 设置不要混用，全局配置仍共用。config show 默认脱敏；只有显式使用 --show-secrets 才显示完整配置。\n\n示例：\n  crosslane config list\n  crosslane config show pool --effective\n  crosslane config validate\n\n操作说明：crosslane guide config"
    )]
    Config {
        #[command(subcommand)]
        command: Option<ConfigCommand>,
    },
    /// 账号管理。
    #[command(
        after_help = "统一入口：account add --provider <ID>；Kiro 使用 --auth sso/api-key，Copilot 使用 --auth device-flow。兼容命令 account add-sso / add-api-key 仅适用于 Kiro。\n\n示例：\n  crosslane account list --provider kiro\n  crosslane account list --provider copilot\n  crosslane account probe --provider copilot <ACCOUNT_ID>\n  crosslane account overage\n\n操作说明：crosslane guide kiro、crosslane guide copilot、crosslane guide account"
    )]
    Account {
        #[command(subcommand)]
        command: Option<crate::commands::account::AccountCommand>,
    },
    /// Kiro 查看调度评分；Copilot 查看账号及模型支持。
    #[command(
        after_help = "Kiro 显示排队、额度与选号评分；Copilot 显示账号认证和模型支持，--explain 的评分明细不适用于 Copilot。省略来源显示全部账号，省略模型展示已发现模型；按单个模型检查时使用 --model。--json 与 --watch 同时使用时，每次刷新输出一行 JSON（NDJSON）。\n\n示例：\n  crosslane pool --provider kiro --model claude-sonnet-4 --explain\n  crosslane pool --provider copilot --model <已授权模型ID> --watch"
    )]
    Pool {
        /// 查看指定提供源的账号池；默认全部来源。
        #[arg(long, default_value = "all")]
        provider: String,
        /// 按该模型检查账号是否可调度。
        #[arg(long, default_value = "")]
        model: String,
        /// 每 2 秒刷新；交互终端中原地更新。
        #[arg(long)]
        watch: bool,
        /// 显示 Kiro 账号不可调度原因和三因子评分；Copilot 不提供评分明细。
        #[arg(long)]
        explain: bool,
    },
    /// Kiro 上游网络与真实推理诊断；Copilot 使用 account probe。
    #[command(
        after_help = "本组为 Kiro 专项诊断。Copilot 请使用 `crosslane account probe --provider copilot <ACCOUNT_ID>`。\n\n示例：\n  crosslane diagnose all\n  crosslane diagnose endpoints\n  crosslane diagnose account --all\n\n操作说明：crosslane guide diagnose"
    )]
    Diagnose {
        #[command(subcommand)]
        command: Option<DiagnoseCommand>,
    },
    /// 查询 Kiro 订阅计划；Copilot 不提供权威订阅数据。
    #[command(
        after_help = "仅 Kiro 返回权威订阅计划；Copilot 返回 supported=false。Copilot 账号授权与可用模型请用 account probe/models list 检查。\n\n示例：\n  crosslane subscriptions --provider kiro\n  crosslane subscriptions --provider all\n  crosslane account probe --provider copilot <ACCOUNT_ID>"
    )]
    Subscriptions {
        id: Option<String>,
        /// 查询指定提供源；`all` 汇总并标明不支持订阅查询的来源。
        #[arg(long, default_value = "kiro")]
        provider: String,
    },
    /// 查看或运行 Kiro、Copilot 与全局周期任务。
    #[command(
        after_help = "token_refresh/status_check/health_recheck 为 Kiro 专项；model_cache_refresh 可用 --provider <ID> 限定；stats_persist 等为全局任务。\n\n示例：\n  crosslane tasks list\n  crosslane tasks run status_check\n  crosslane tasks run model_cache_refresh --provider copilot\n\n操作说明：crosslane guide tasks"
    )]
    Tasks {
        #[command(subcommand)]
        command: Option<TaskCommand>,
    },
    /// 显示 Kiro/Copilot 请求统计；可按提供源过滤。
    #[command(
        after_help = "省略 --provider 时聚合 Kiro 与 Copilot；--by provider 可查看来源分组。\n\n示例：\n  crosslane stats --since 1h --provider kiro\n  crosslane stats --since 1h --provider copilot\n  crosslane stats --detail --by provider --recent 20"
    )]
    Stats {
        /// 显示分组和最近请求明细。
        #[arg(long)]
        detail: bool,
        /// 最近请求条数，范围 0..=1000；0 表示不显示最近请求，默认 20。
        #[arg(long, requires = "detail", value_parser = parse_stats_recent)]
        recent: Option<usize>,
        #[command(flatten)]
        range: TimeRangeArgs,
        /// 分组维度：provider/model/account/apikey/endpoint。
        #[arg(long, requires = "detail", value_enum)]
        by: Option<StatsGroup>,
        /// 仅统计一个提供源实例；`all` 等同于不筛选。
        #[arg(long)]
        provider: Option<String>,
    },
    /// 查看 Kiro/Copilot 请求日志和全局日志文件。
    #[command(
        after_help = "show/follow 可用 --provider 过滤 Kiro 或 Copilot 请求；trace/files/path 是全局日志操作。\n\n示例：\n  crosslane logs show --provider kiro --tail 100\n  crosslane logs follow --provider copilot --level error\n  crosslane logs files\n\n操作说明：crosslane guide logs"
    )]
    Logs {
        #[command(subcommand)]
        command: Option<LogsCommand>,
    },
    /// Kiro/Copilot API key 及来源权限管理。
    #[command(
        name = "apikey",
        after_help = "示例：\n  crosslane apikey add --name kiro-key --provider kiro\n  crosslane apikey add --name copilot-key --provider copilot\n  crosslane apikey list --provider copilot\n\n操作说明：crosslane guide apikey"
    )]
    ApiKey {
        #[command(subcommand)]
        command: Option<crate::commands::runtime::ApiKeyCommand>,
    },
    /// Kiro/Copilot API 代理服务管理。
    #[command(
        name = "service",
        after_help = "示例：\n  crosslane service create --name kiro --port 5580 --provider kiro --default-provider kiro\n  crosslane service create --name copilot --port 5581 --provider copilot --default-provider copilot\n  crosslane service list\n\n操作说明：crosslane guide service"
    )]
    Service {
        #[command(subcommand)]
        command: Option<crate::commands::runtime::ServiceCommand>,
    },
    /// Kiro 额度和 token 刷新告警及通知目标管理。
    #[command(
        name = "alert",
        after_help = "当前四类账号额度/token 刷新事件来自 Kiro 账号池，不代表 Copilot 授权、席位或额度告警。\n\n示例：\n  crosslane alert events\n  crosslane alert list\n  crosslane alert logs\n\n操作说明：crosslane guide alert"
    )]
    Alert {
        #[command(subcommand)]
        command: Option<crate::commands::runtime::AlertCommand>,
    },
    /// 查看、解析及全局禁用/启用 Kiro/Copilot 模型。
    #[command(
        visible_alias = "model",
        after_help = "示例：\n  crosslane models list --provider kiro --refresh\n  crosslane models list --provider copilot --refresh\n  crosslane models list --provider all --mapped\n  crosslane models resolve <MODEL_ID> --provider copilot\n\n操作说明：crosslane guide models"
    )]
    Models {
        #[command(subcommand)]
        command: Option<ModelsCommand>,
    },
    /// 按提供源管理 Kiro/Copilot 模型映射规则。
    #[command(
        name = "model-map",
        after_help = "add 省略 --provider 时仅作用于 Kiro；edit 省略时保留规则原有范围。--below-credits-percent 仅能用于 Kiro。\n\n示例：\n  crosslane model-map list --provider kiro\n  crosslane model-map list --provider copilot\n  crosslane model-map test <MODEL_ID> --provider copilot\n\n操作说明：crosslane guide model-map"
    )]
    ModelMap {
        #[command(subcommand)]
        command: Option<ModelMapCommand>,
    },
    /// 查看命令帮助。
    #[command(
        after_help = "示例：\n  crosslane help\n  crosslane help logs\n  crosslane help logs trace\n  crosslane help --all"
    )]
    Help {
        /// 递归列出全部公开命令。
        #[arg(long, conflicts_with = "path")]
        all: bool,
        /// 要查看的命令路径，例如 logs trace。
        #[arg(value_name = "COMMAND", num_args = 0..)]
        path: Vec<String>,
    },
    /// 查看原理与操作指南。
    #[command(
        after_help = "示例：\n  crosslane guide\n  crosslane guide kiro\n  crosslane guide copilot\n  crosslane guide provider"
    )]
    Guide {
        #[arg(value_enum)]
        topic: Option<crate::cli::guide::Topic>,
    },
    /// 生成 Shell 补全脚本。
    #[command(
        after_help = "示例：\n  crosslane completions bash\n  crosslane completions zsh\n  crosslane completions fish"
    )]
    Completions { shell: CompletionShell },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "lowercase")]
enum CompletionShell {
    Bash,
    Zsh,
    Fish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
enum StatsGroup {
    Provider,
    Model,
    Account,
    #[value(name = "apikey")]
    ApiKey,
    Endpoint,
}

impl StatsGroup {
    fn as_str(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Model => "model",
            Self::Account => "account",
            Self::ApiKey => "apikey",
            Self::Endpoint => "endpoint",
        }
    }
}

#[derive(Debug, Subcommand)]
enum ProviderCommand {
    /// 列出 Kiro/Copilot 实例及运行状态。
    List {
        /// 按实例 ID 筛选；省略或 all 显示全部。
        #[arg(long)]
        provider: Option<String>,
        /// 按驱动类型筛选，与实例 ID 无关。
        #[arg(long, value_parser = ["kiro", "copilot"])]
        provider_kind: Option<String>,
    },
    /// 显示 Kiro/Copilot 实例的能力和运行状态（不显示凭证设置）。
    Show { id: String },
    /// 添加一个提供源实例。
    #[command(
        visible_alias = "create",
        after_help = "未显式配置 provider 时内置 Kiro；首次添加 Copilot 会保留 Kiro 实例。新建 Copilot 实例会将默认公开 Client ID 写入 [provider.settings]，可在配置文件中修改。\n\n示例：\n  crosslane provider add --id copilot --kind copilot\n  crosslane config path\n  crosslane provider show copilot"
    )]
    Add {
        /// 自定义实例 ID，后续 --provider 使用此值；例如 copilot、team-a。
        #[arg(long)]
        id: String,
        /// 驱动类型；不是实例 ID。
        #[arg(long, value_parser = ["kiro", "copilot"])]
        kind: String,
        /// 驱动设置，格式为 KEY=TOML_VALUE，可重复。
        #[arg(long = "setting", value_name = "KEY=VALUE")]
        settings: Vec<String>,
        /// 单账号并发上限；0 表示继承全局账号池上限。
        #[arg(long)]
        max_concurrent_per_account: Option<usize>,
        #[arg(long)]
        default_model: Option<String>,
        #[arg(long)]
        disabled: bool,
    },
    /// 修改提供源实例的驱动设置和路由默认值。
    #[command(
        after_help = "--setting 接受 KEY=TOML_VALUE；Copilot OAuth/endpoint 设置属于该 Copilot 实例，不写入 Kiro 的 [sso]/[upstream]。修改 client_id 后，既有账号可能需要重新授权。\n\n示例：\n  crosslane provider edit copilot --setting 'client_id=\"Iv1.your-app\"'\n  crosslane provider edit copilot --setting 'api_endpoint_fallback=\"https://api.enterprise.githubcopilot.com\"'"
    )]
    Edit {
        id: String,
        /// 设置或覆盖驱动设置，格式为 KEY=TOML_VALUE，可重复。
        #[arg(long = "setting", value_name = "KEY=VALUE")]
        settings: Vec<String>,
        /// 删除驱动设置，可重复或逗号分隔。
        #[arg(long = "remove-setting", value_delimiter = ',')]
        remove_settings: Vec<String>,
        /// 单账号并发上限；0 表示继承全局账号池上限。
        #[arg(long)]
        max_concurrent_per_account: Option<usize>,
        #[arg(long)]
        default_model: Option<String>,
        #[arg(long, value_name = "BOOL", action = clap::ArgAction::Set)]
        enable_model_fallback: Option<bool>,
        #[arg(long, value_name = "BOOL", action = clap::ArgAction::Set)]
        allow_cross_provider_fallback: Option<bool>,
    },
    /// 启用提供源实例。
    Enable { id: String },
    /// 停用提供源实例，但保留配置和账号。
    Disable { id: String },
    /// 删除 Kiro/Copilot 实例配置；账号文件保留在数据目录中。
    #[command(name = "delete", visible_alias = "rm")]
    Delete {
        id: String,
        /// 跳过交互确认，用于自动化。
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ModelsCommand {
    /// 全局禁用模型：所有提供源、协议、回退和内部探测均禁止调用。
    #[command(
        after_help = "支持一次指定多个模型 ID。如设置了 CROSSLANE_PROVIDER，请用 --provider all 明确执行全局操作。\n\n示例：\n  crosslane model disable gpt-4.1 --provider all"
    )]
    Disable {
        /// 不带 provider 前缀的模型 ID；支持一次指定多个。
        #[arg(required = true, num_args = 1..)]
        models: Vec<String>,
    },
    /// 解除模型的全局禁用规则，立即生效。
    #[command(
        after_help = "如设置了 CROSSLANE_PROVIDER，请用 --provider all 明确执行全局操作。\n\n示例：\n  crosslane model enable gpt-4.1 --provider all"
    )]
    Enable {
        #[arg(required = true, num_args = 1..)]
        models: Vec<String>,
    },
    /// 列出一个或全部提供源发现的模型、输入上下文与输出上限。
    #[command(
        after_help = "示例：\n  crosslane models list --provider kiro --refresh\n  crosslane models list --provider copilot --refresh\n  crosslane models list --provider all --mapped"
    )]
    List {
        /// 同时显示每个模型经过映射规则后的结果。
        #[arg(long)]
        mapped: bool,
        /// 先立即执行一次上游模型发现，再显示结果。
        #[arg(long)]
        refresh: bool,
        /// 仅显示一个提供源实例；默认聚合全部来源。
        #[arg(long, default_value = "all")]
        provider: String,
        /// 按驱动类型筛选，与实例 ID 无关。
        #[arg(long, value_parser = ["kiro", "copilot"])]
        provider_kind: Option<String>,
    },
    /// 查询客户端 model ID 在指定提供源中的映射与最终模型。
    #[command(
        after_help = "省略 --provider 时保持 Kiro 解析语义；Copilot 请显式指定实例 ID。\n\n示例：\n  crosslane models resolve opus5 --provider kiro\n  crosslane models resolve team-fast --provider copilot --refresh\n  crosslane --json models resolve opus5 --api-key production"
    )]
    Resolve {
        /// 客户端传入的 model ID。
        model: String,
        /// 按指定 API key ID 或名称应用条件映射规则。
        #[arg(long, value_name = "ID_OR_NAME")]
        api_key: Option<String>,
        /// 查询前先刷新账号模型缓存。
        #[arg(long)]
        refresh: bool,
        /// 指定提供源实例；默认保持 Kiro v1 解析语义。
        #[arg(long)]
        provider: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
enum RequestLogLevel {
    /// 显示全部请求；`info` 是兼容别名。
    #[value(alias = "info")]
    All,
    /// 只显示 HTTP 4xx/5xx 请求；`warning` 是兼容别名。
    #[value(alias = "warning")]
    Warn,
    /// 只显示 HTTP 5xx 请求。
    Error,
}

impl RequestLogLevel {
    fn as_filter(self) -> Option<&'static str> {
        match self {
            Self::All => None,
            Self::Warn => Some("warn"),
            Self::Error => Some("error"),
        }
    }
}

#[derive(Debug, Args)]
struct RequestLogArgs {
    /// 最多显示多少条最近请求（1～1000）。
    #[arg(long, default_value_t = 50, value_parser = parse_log_tail)]
    tail: usize,
    /// 请求范围：all/info 显示全部，warn 显示 4xx/5xx，error 显示 5xx。
    #[arg(long, value_enum)]
    level: Option<RequestLogLevel>,
    /// 按账号 ID、邮箱或名称过滤。
    #[arg(long)]
    account: Option<String>,
    /// 按提供源实例 ID 过滤。
    #[arg(long)]
    provider: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "lowercase")]
enum LogFileLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogFileLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Subcommand)]
enum LogsCommand {
    /// 显示 Kiro/Copilot 的结构化请求日志。
    #[command(
        after_help = "省略 --provider 时显示全部来源；可与 --account 组合过滤。\n\n示例：\n  crosslane logs show --provider kiro --tail 200\n  crosslane logs show --provider copilot --tail 200"
    )]
    Show(RequestLogArgs),
    /// 持续跟踪 Kiro/Copilot 的结构化请求日志。
    #[command(
        after_help = "示例：\n  crosslane logs follow --provider kiro\n  crosslane logs follow --provider copilot --level error"
    )]
    Follow(RequestLogArgs),
    /// 按 trace ID 跨日期、跨级别查询完整请求链路。
    #[command(
        after_help = "默认查询 info/warn/error/debug/trace 的所有物理分片。\n\n示例：\n  crosslane logs trace trace_0123456789abcdef0123456789abcdef\n  crosslane logs trace trace_0123456789abcdef0123456789abcdef --level error --tail 500"
    )]
    Trace {
        /// 响应头 x-trace-id 或 request-id 中的 trace ID。
        trace_id: String,
        /// 最多显示最近多少条匹配记录（1～1000）。
        #[arg(long, default_value_t = 200, value_parser = parse_log_tail)]
        tail: usize,
        /// 只查询一个精确级别的物理分片。
        #[arg(long, value_enum)]
        level: Option<LogFileLevel>,
    },
    /// 列出所有实际 daemon 日志文件、大小、级别和完整路径。
    #[command(
        after_help = "示例：\n  crosslane logs files\n  crosslane logs files --level error\n  crosslane --json logs files"
    )]
    Files {
        /// 只列出指定级别的日志文件。
        #[arg(long, value_enum)]
        level: Option<LogFileLevel>,
    },
    /// 显示日志目录、基础路径、格式和过滤级别。
    #[command(after_help = "示例：\n  crosslane logs path\n  crosslane --json logs path")]
    Path,
}

#[derive(Debug, Subcommand)]
enum ModelMapCommand {
    /// 列出 Kiro/Copilot 模型映射规则。
    #[command(
        after_help = "示例：\n  crosslane model-map list\n  crosslane model-map list --provider copilot\n  crosslane --json model-map list"
    )]
    List {
        /// 仅显示会作用于该提供源的规则；未指定时显示全部。
        #[arg(long)]
        provider: Option<String>,
    },
    /// 添加模型映射规则。
    #[command(
        after_help = "省略 --provider 时仅作用于 Kiro；--below-credits-percent 只支持 Kiro，不能用于 Copilot。Copilot 目标模型请从 `models list --provider copilot` 的结果选择。\n\n示例：\n  crosslane model-map add --name kiro-low-credit --source 'claude-opus-*' --target claude-sonnet-4 --provider kiro --below-credits-percent 10\n  crosslane model-map add --name copilot-fast --source team-fast --target MODEL_ID_FROM_COPILOT_LIST --provider copilot"
    )]
    Add {
        #[arg(long)]
        name: String,
        #[arg(
            long,
            default_value = "replace",
            value_parser = ["replace", "alias", "loadbalance"]
        )]
        kind: String,
        #[arg(long = "source", value_delimiter = ',', required = true)]
        source_models: Vec<String>,
        #[arg(long = "target", value_delimiter = ',', required = true)]
        target_models: Vec<String>,
        #[arg(long, default_value_t = 0)]
        priority: i32,
        #[arg(long = "weight", value_delimiter = ',')]
        weights: Vec<u32>,
        /// Kiro 账号剩余 credits 百分比低于此值时生效；Copilot 不支持。
        #[arg(long, value_parser = parse_percent)]
        below_credits_percent: Option<f64>,
        #[arg(long = "api-key", value_delimiter = ',')]
        api_key_ids: Vec<String>,
        /// 规则作用的提供源，可重复或逗号分隔；省略时保持旧版 Kiro 范围。
        #[arg(id = "provider", long = "provider", value_delimiter = ',')]
        providers: Vec<String>,
        /// 规则作用的代理服务，可重复或逗号分隔。
        #[arg(long = "service", value_delimiter = ',')]
        service_ids: Vec<String>,
        #[arg(long)]
        disabled: bool,
    },
    /// 编辑模型映射规则；可用 --provider 替换适用来源。
    #[command(
        after_help = "省略 --provider 时保留规则现有来源；--clear-providers 恢复 Kiro-only。额度阈值规则只能作用于 Kiro。\n\n示例：\n  crosslane model-map edit copilot-fast --provider copilot\n  crosslane model-map edit kiro-low-credit --below-credits-percent 8"
    )]
    Edit {
        /// 当前规则名。
        name: String,
        #[arg(long)]
        rename: Option<String>,
        #[arg(long, value_parser = ["replace", "alias", "loadbalance"])]
        kind: Option<String>,
        #[arg(long = "source", value_delimiter = ',')]
        source_models: Vec<String>,
        #[arg(long = "target", value_delimiter = ',')]
        target_models: Vec<String>,
        #[arg(long)]
        priority: Option<i32>,
        #[arg(long = "weight", value_delimiter = ',')]
        weights: Vec<u32>,
        #[arg(long)]
        clear_weights: bool,
        #[arg(long, value_parser = parse_percent)]
        below_credits_percent: Option<f64>,
        #[arg(long)]
        clear_credits_threshold: bool,
        #[arg(long = "api-key", value_delimiter = ',')]
        api_key_ids: Vec<String>,
        #[arg(long)]
        clear_api_keys: bool,
        #[arg(id = "provider", long = "provider", value_delimiter = ',')]
        providers: Vec<String>,
        #[arg(long)]
        clear_providers: bool,
        #[arg(long = "service", value_delimiter = ',')]
        service_ids: Vec<String>,
        #[arg(long)]
        clear_services: bool,
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        #[arg(long, conflicts_with = "enable")]
        disable: bool,
    },
    /// 删除模型映射规则，执行前需输入 y 或 yes 确认。
    #[command(name = "delete", visible_alias = "rm")]
    Delete {
        name: String,
        /// 跳过交互确认，用于自动化。
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// 测试客户端模型名会命中的规则。
    #[command(
        after_help = "默认测试 Kiro 规则；Copilot 必须显式传入 --provider copilot，额度百分比条件仅适用于 Kiro。\n\n示例：\n  crosslane model-map test claude-opus-4 --provider kiro --remaining-credits-percent 8\n  crosslane model-map test team-fast --provider copilot"
    )]
    Test {
        model: String,
        #[arg(long, value_parser = parse_percent)]
        remaining_credits_percent: Option<f64>,
        #[arg(long)]
        api_key: Option<String>,
        /// 按该提供源测试；默认保持旧版 Kiro 语义。
        #[arg(long, default_value = "kiro")]
        provider: String,
        /// 按该代理服务测试服务范围规则。
        #[arg(long)]
        service: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum TaskCommand {
    /// 显示 Kiro、跨来源与全局周期任务及其运行状态。
    #[command(
        after_help = "展示全部周期任务，包含不能手工触发的 account_file_watcher 和 adaptive_admission；不接受 --provider。\n\n示例：\n  crosslane tasks list\n  crosslane --json tasks list"
    )]
    List,
    /// 立即运行一个任务。
    #[command(
        after_help = "可手工运行：token_refresh/status_check/health_recheck（仅 Kiro）、model_cache_refresh（可按来源）、stats_persist/daily_reset/proxy_service_reconcile（全局）。account_file_watcher 和 adaptive_admission 仅由调度器运行。Kiro 任务只接受 --provider kiro；全局任务不接受 --provider。\n\n示例：\n  crosslane tasks run status_check\n  crosslane tasks run model_cache_refresh --provider copilot\n  crosslane tasks run proxy_service_reconcile"
    )]
    Run {
        name: String,
        /// 限定模型缓存刷新提供源；Kiro 任务只接受 kiro，全局任务不接受该参数。
        #[arg(long)]
        provider: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum DiagnoseCommand {
    /// 检查 Kiro 上游端点，并对全部 Kiro 账号发起真实推理。
    #[command(
        after_help = "仅针对 Kiro 端点和账号，会发起真实推理；Copilot 请用 account probe --provider copilot。\n\n示例：\n  crosslane diagnose all\n  crosslane diagnose all --region us-west-2 --timeout 30s --concurrency 4"
    )]
    All {
        /// 上游端点所在区域。
        #[arg(long, default_value = "us-east-1")]
        region: String,
        /// 单账号真实推理探测超时，范围 1s..5m。
        #[arg(long, default_value = "45s", value_parser = parse_diagnose_timeout)]
        timeout: u64,
        /// 账号探测并发数，范围 1..8。
        #[arg(short = 'c', long, default_value_t = 1, value_parser = parse_diagnose_concurrency)]
        concurrency: usize,
    },
    /// 探测 CodeWhisperer、AmazonQ 与 OIDC 网络连通性。
    #[command(
        after_help = "示例：\n  crosslane diagnose endpoints\n  crosslane diagnose endpoints --region us-west-2"
    )]
    Endpoints {
        #[arg(long, default_value = "us-east-1")]
        region: String,
    },
    /// 拉取模型并发起一次真实推理，验证 Kiro 账号存活。
    #[command(
        after_help = "示例：\n  crosslane diagnose account acc_7f3a2b1c\n  crosslane diagnose account --all --concurrency 4"
    )]
    Account {
        #[command(flatten)]
        target: DiagnoseAccountTarget,
        /// 单账号真实推理探测超时，范围 1s..5m。
        #[arg(long, default_value = "45s", value_parser = parse_diagnose_timeout)]
        timeout: u64,
        /// `--all` 时的探测并发数，范围 1..8。
        #[arg(short = 'c', long, default_value_t = 1, value_parser = parse_diagnose_concurrency)]
        concurrency: usize,
    },
}

#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
struct DiagnoseAccountTarget {
    /// 账号 ID。
    id: Option<String>,
    /// 检查全部账号。
    #[arg(long)]
    all: bool,
}

fn parse_diagnose_timeout(value: &str) -> std::result::Result<u64, String> {
    let seconds =
        crate::commands::runtime::parse_duration(value).map_err(|error| error.to_string())?;
    if (1..=300).contains(&seconds) {
        Ok(seconds)
    } else {
        Err("超时必须在 1 秒到 300 秒（5 分钟）之间".to_owned())
    }
}

fn parse_diagnose_concurrency(value: &str) -> std::result::Result<usize, String> {
    let concurrency = value
        .parse::<usize>()
        .map_err(|_| "并发数必须是整数".to_owned())?;
    if (1..=8).contains(&concurrency) {
        Ok(concurrency)
    } else {
        Err("并发数必须在 1..=8 之间".to_owned())
    }
}

fn parse_log_tail(value: &str) -> std::result::Result<usize, String> {
    let tail = value
        .parse::<usize>()
        .map_err(|_| "日志条数必须是整数".to_owned())?;
    if (1..=1_000).contains(&tail) {
        Ok(tail)
    } else {
        Err("日志条数必须在 1..=1000 之间".to_owned())
    }
}

fn parse_stats_recent(value: &str) -> std::result::Result<usize, String> {
    let recent = value
        .parse::<usize>()
        .map_err(|_| "最近请求条数必须是整数".to_owned())?;
    if recent <= 1_000 {
        Ok(recent)
    } else {
        Err("最近请求条数必须在 0..=1000 之间".to_owned())
    }
}

fn parse_percent(value: &str) -> std::result::Result<f64, String> {
    let percent = value
        .parse::<f64>()
        .map_err(|_| "百分比必须是数字".to_owned())?;
    if percent.is_finite() && (0.0..=100.0).contains(&percent) {
        Ok(percent)
    } else {
        Err("百分比必须是 0..=100 的有限数值".to_owned())
    }
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// 列出配置模块及 Kiro/Copilot/全局适用范围。
    #[command(
        after_help = "列表中的适用来源用于区分 Kiro 专项、按来源配置和全局配置。\n\n示例：\n  crosslane config list\n  crosslane --json config list"
    )]
    List,
    /// 显示配置。
    #[command(
        after_help = "Kiro 专项：upstream/pool/features/context/storage/sso 等；models 包含全局禁用规则和 Kiro 发现配置；Copilot 驱动配置：provider；server/log/admin 等为全局。默认脱敏展示；使用 --show-secrets 才输出原始凭证和注释。\n\n示例：\n  crosslane config list\n  crosslane config show pool --effective\n  crosslane provider show copilot"
    )]
    Show {
        /// 只显示指定模块；使用 `crosslane config list` 查看模块名。
        module: Option<String>,
        /// 显示合并默认值后的生效配置。
        #[arg(long)]
        effective: bool,
        /// 显示包含凭证的完整配置；默认脱敏。
        #[arg(long)]
        show_secrets: bool,
    },
    /// 打印全部文件路径。
    #[command(after_help = "示例：\n  crosslane config path\n  crosslane --json config path")]
    Path,
    /// 手动触发重载。
    #[command(after_help = "示例：\n  crosslane config reload\n  crosslane --json config reload")]
    Reload,
    /// 用 $VISUAL/$EDITOR 编辑完整配置或指定模块，保存后校验并重载。
    #[command(
        after_help = "Kiro 上游与账号池使用 upstream/pool 等模块；Copilot OAuth、endpoint、缓存优先用 provider edit <ID> 修改对应实例。不指定模块时编辑完整配置。\n\n示例：\n  crosslane config edit upstream\n  crosslane config edit pool\n  crosslane provider edit copilot --setting 'api_endpoint_fallback=\"https://api.enterprise.githubcopilot.com\"'"
    )]
    Edit {
        /// 只编辑指定模块；使用 `crosslane config list` 查看模块名。
        module: Option<String>,
    },
    /// 备份配置并重置指定模块或全部可重置的运行配置。
    #[command(
        after_help = "指定模块时仅重置该模块；pool/features/sso 等属于 Kiro 专项，server/log 等为全局。provider、api-key、proxy-service 不可按模块重置；不指定模块时重置 Kiro 与全局运行参数，保留基础服务和告警配置，但清除模型映射。原配置自动备份。\n\n示例：\n  crosslane config reset pool\n  crosslane config reset sso\n  crosslane config reset"
    )]
    Reset {
        /// 只重置指定模块；使用 `crosslane config list` 查看模块名。
        module: Option<String>,
        /// 跳过交互确认，用于自动化。
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// 只校验配置，不应用。
    #[command(
        after_help = "示例：\n  crosslane config validate\n  crosslane config validate ./config.toml"
    )]
    Validate { file: Option<String> },
}

#[tokio::main]
async fn main() -> Result<()> {
    let raw_args: Vec<OsString> = std::env::args_os().collect();
    let preliminary = crate::cli::parse_navigation_or_exit(raw_args.clone());
    if crate::cli::handle_local(&preliminary).await? {
        return Ok(());
    }

    crosslane_store::environment::load_dotenv()?;
    let cli = crate::cli::parse_or_exit(raw_args);
    let Some(command) = cli.command else {
        unreachable!("local navigation returned before runtime setup")
    };
    if matches!(
        &command,
        Command::Restart | Command::Stop | Command::Uninstall { .. }
    ) {
        anyhow::bail!(
            "该命令需在 Docker 宿主机通过 crosslane wrapper 执行。\n\
             请在仓库根目录运行 `./deploy/docker-setup.sh` 安装或更新 wrapper；\n\
             原生 systemd 部署请使用 `sudo systemctl restart|stop crosslaned`。"
        );
    }
    let socket = resolve_socket(cli.socket.clone()).await;
    let mut client = AdminClient::connect(socket);

    match command {
        Command::Status {
            watch,
            range,
            provider,
        } => {
            let (since_secs, start_secs, end_secs) = parse_time_range_args(&range)?;
            loop {
                let status: StatusResult = client
                    .call(
                        method::STATUS,
                        serde_json::json!({
                            "since_secs":since_secs,
                            "start_secs":start_secs,
                            "end_secs":end_secs,
                            "provider":provider
                        }),
                    )
                    .await?;
                if cli.json {
                    if watch {
                        crate::output::print_json_line(&status)?;
                    } else {
                        print_json(&status)?;
                    }
                } else {
                    print_status(&status);
                }
                if !watch {
                    break;
                }
                tokio::select! {
                    result = tokio::signal::ctrl_c() => { result?; break; }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
                }
            }
        }
        Command::Health => {
            let status: StatusResult = client.call(method::STATUS, serde_json::json!({})).await?;
            if cli.json {
                print_json(&serde_json::json!({"healthy":true,"status":status}))?;
            } else {
                println!("ok");
            }
        }
        Command::Ready { provider } => {
            let status: StatusResult = client
                .call(method::STATUS, serde_json::json!({"provider":provider}))
                .await?;
            if cli.json {
                print_json(&serde_json::json!({
                    "ready":status.ready,
                    "reasons":status.readiness_reasons.clone(),
                    "status":status
                }))?;
            } else if status.ready {
                println!("ready");
            } else {
                anyhow::bail!("not ready: {}", status.readiness_reasons.join("; "));
            }
            if !status.ready {
                anyhow::bail!("business proxy is not ready");
            }
        }
        Command::Version => unreachable!("version returned before runtime setup"),
        Command::Restart | Command::Stop | Command::Uninstall { .. } => {
            unreachable!("host lifecycle commands returned before connecting to the daemon")
        }
        Command::Provider {
            command:
                Some(ProviderCommand::List {
                    provider,
                    provider_kind,
                }),
        } => {
            show_providers(
                &mut client,
                provider.as_deref(),
                provider_kind.as_deref(),
                false,
                cli.json,
            )
            .await?;
        }
        Command::Provider {
            command: Some(ProviderCommand::Show { id }),
        } => {
            show_providers(&mut client, Some(&id), None, true, cli.json).await?;
        }
        Command::Provider {
            command:
                Some(ProviderCommand::Add {
                    id,
                    kind,
                    settings,
                    max_concurrent_per_account,
                    default_model,
                    disabled,
                }),
        } => {
            crate::commands::runtime::add_provider(
                &mut client,
                &id,
                &kind,
                &settings,
                max_concurrent_per_account,
                default_model.as_deref(),
                disabled,
                cli.json,
            )
            .await?;
        }
        Command::Provider {
            command:
                Some(ProviderCommand::Edit {
                    id,
                    settings,
                    remove_settings,
                    max_concurrent_per_account,
                    default_model,
                    enable_model_fallback,
                    allow_cross_provider_fallback,
                }),
        } => {
            crate::commands::runtime::edit_provider(
                &mut client,
                &id,
                &settings,
                &remove_settings,
                max_concurrent_per_account,
                default_model.as_deref(),
                enable_model_fallback,
                allow_cross_provider_fallback,
                cli.json,
            )
            .await?;
        }
        Command::Provider {
            command: Some(ProviderCommand::Enable { id }),
        } => {
            crate::commands::runtime::set_provider_enabled(&mut client, &id, true, cli.json)
                .await?;
        }
        Command::Provider {
            command: Some(ProviderCommand::Disable { id }),
        } => {
            crate::commands::runtime::set_provider_enabled(&mut client, &id, false, cli.json)
                .await?;
        }
        Command::Provider {
            command: Some(ProviderCommand::Delete { id, yes }),
        } => {
            crate::commands::runtime::delete_provider(&mut client, &id, yes, cli.json).await?;
        }
        Command::Config {
            command: Some(ConfigCommand::List),
        } => {
            crate::commands::runtime::list_config_modules(cli.json)?;
        }
        Command::Config {
            command:
                Some(ConfigCommand::Show {
                    module,
                    effective,
                    show_secrets,
                }),
        } => {
            crate::commands::runtime::show_config(
                &mut client,
                module.as_deref(),
                effective,
                show_secrets,
                cli.json,
            )
            .await?;
        }
        Command::Config {
            command: Some(ConfigCommand::Path),
        } => {
            let paths: ConfigPathResult = client
                .call(method::CONFIG_PATH, serde_json::json!({}))
                .await?;
            if cli.json {
                print_json(&paths)?;
            } else {
                println!("配置文件    {}", paths.config_file);
                println!("账号库      {}", paths.accounts_file);
                println!("日用量      {}", paths.daily_file);
                println!("统计        {}", paths.stats_file);
                println!("日志目录    {}", paths.log_directory);
                println!("日志基础路径 {}", paths.log_base_path);
                println!("管理 socket {}", paths.admin_socket);
            }
        }
        Command::Config {
            command: Some(ConfigCommand::Reload),
        } => {
            let result: ConfigReloadResult =
                crate::commands::runtime::reload_config(&mut client).await?;
            if cli.json {
                print_json(&result)?;
            }
            if !result.applied {
                anyhow::bail!(
                    "配置重载失败，已保留原配置：{}",
                    result.error.as_deref().unwrap_or("未知原因")
                );
            }
            if !cli.json {
                println!("配置已重载");
                for field in result.needs_restart {
                    println!("注意：{field} 需重启 crosslaned 才能生效");
                }
            }
        }
        Command::Config {
            command: Some(ConfigCommand::Edit { module }),
        } => {
            crate::commands::runtime::edit_config(&mut client, module.as_deref(), cli.json).await?;
        }
        Command::Config {
            command: Some(ConfigCommand::Reset { module, yes }),
        } => {
            if let Some(result) =
                crate::commands::runtime::reset_config(&mut client, module.as_deref(), yes).await?
            {
                if cli.json {
                    print_json(&serde_json::json!({
                        "config_file": result.config_file,
                        "backup_file": result.backup_file,
                        "module": result.module,
                        "applied": true,
                        "needs_restart": result.needs_restart,
                    }))?;
                } else {
                    if let Some(module) = result.module.as_deref() {
                        println!("配置模块 {module} 已恢复为默认设置并重载；其他配置未改动");
                    } else {
                        println!(
                            "运行配置已恢复默认并重载；提供源、API key、代理服务和告警配置已保留，模型映射已清除"
                        );
                    }
                    println!("原配置备份 {}", result.backup_file.display());
                    for field in result.needs_restart {
                        println!("注意：{field} 需重启 crosslaned 才能生效");
                    }
                }
            } else if cli.json {
                print_json(&serde_json::json!({"cancelled": true}))?;
            } else {
                println!("已取消");
            }
        }
        Command::Config {
            command: Some(ConfigCommand::Validate { file }),
        } => {
            crate::commands::runtime::validate_config(file.as_deref(), cli.json).await?;
        }
        Command::Account {
            command: Some(command),
        } => {
            crate::commands::account::run(&mut client, command, cli.json).await?;
        }
        Command::Pool {
            provider,
            model,
            watch,
            explain,
        } => {
            let refresh_in_place = watch && !cli.json && std::io::stdout().is_terminal();
            loop {
                if refresh_in_place {
                    print!("\x1b[2J\x1b[H");
                    std::io::stdout().flush()?;
                }
                crate::commands::runtime::show_pool(
                    &mut client,
                    &provider,
                    &model,
                    explain,
                    cli.json,
                    watch,
                )
                .await?;
                if !watch {
                    break;
                }
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
                }
            }
        }
        Command::Diagnose { command } => match command {
            Some(DiagnoseCommand::All {
                region,
                timeout,
                concurrency,
            }) => {
                let endpoints: serde_json::Value = client
                    .call(
                        method::DIAGNOSE_ENDPOINTS,
                        serde_json::json!({"region":region}),
                    )
                    .await?;
                let accounts: serde_json::Value = client
                    .call(
                        method::DIAGNOSE_ACCOUNT,
                        serde_json::json!({
                            "all":true,
                            "timeout_secs":timeout,
                            "concurrency":concurrency
                        }),
                    )
                    .await?;
                crate::commands::runtime::print_diagnose_all(&endpoints, &accounts, cli.json)?;
            }
            Some(DiagnoseCommand::Endpoints { region }) => {
                crate::commands::runtime::simple_rpc(
                    &mut client,
                    method::DIAGNOSE_ENDPOINTS,
                    serde_json::json!({"region":region}),
                    cli.json,
                )
                .await?;
            }
            Some(DiagnoseCommand::Account {
                target: DiagnoseAccountTarget { id, all },
                timeout,
                concurrency,
            }) => {
                crate::commands::runtime::simple_rpc(
                    &mut client,
                    method::DIAGNOSE_ACCOUNT,
                    serde_json::json!({
                        "id":id,
                        "all":all,
                        "timeout_secs":timeout,
                        "concurrency":concurrency
                    }),
                    cli.json,
                )
                .await?;
            }
            None => unreachable!("empty diagnose group returned before runtime setup"),
        },
        Command::Subscriptions { id, provider } => {
            crate::commands::runtime::simple_rpc(
                &mut client,
                method::SUBSCRIPTIONS,
                serde_json::json!({"id":id,"provider":provider}),
                cli.json,
            )
            .await?;
        }
        Command::Tasks { command } => {
            let (method_name, params) = match command {
                Some(TaskCommand::List) => (method::TASKS, serde_json::json!({})),
                Some(TaskCommand::Run { name, provider }) => (
                    method::TASK_RUN,
                    serde_json::json!({"name":name,"provider":provider}),
                ),
                None => unreachable!("empty tasks group returned before runtime setup"),
            };
            crate::commands::runtime::simple_rpc(&mut client, method_name, params, cli.json)
                .await?;
        }
        Command::Stats {
            detail,
            recent,
            range,
            by,
            provider,
        } => {
            let range = parse_time_range_args(&range)?;
            crate::commands::runtime::show_stats(
                &mut client,
                detail,
                recent,
                range,
                by.map(StatsGroup::as_str),
                provider.as_deref(),
                cli.json,
            )
            .await?;
        }
        Command::Logs { command } => match command {
            Some(LogsCommand::Show(query)) => {
                crate::commands::runtime::show_logs(
                    &mut client,
                    query.tail,
                    false,
                    query.level.and_then(RequestLogLevel::as_filter),
                    query.account.as_deref(),
                    query.provider.as_deref(),
                    cli.json,
                )
                .await?;
            }
            Some(LogsCommand::Follow(query)) => {
                crate::commands::runtime::show_logs(
                    &mut client,
                    query.tail,
                    true,
                    query.level.and_then(RequestLogLevel::as_filter),
                    query.account.as_deref(),
                    query.provider.as_deref(),
                    cli.json,
                )
                .await?;
            }
            Some(LogsCommand::Trace {
                trace_id,
                tail,
                level,
            }) => {
                crate::commands::runtime::show_trace_logs(
                    &mut client,
                    &trace_id,
                    tail,
                    level.map(LogFileLevel::as_str),
                    cli.json,
                )
                .await?;
            }
            Some(LogsCommand::Files { level }) => {
                crate::commands::runtime::show_log_files(
                    &mut client,
                    level.map(LogFileLevel::as_str),
                    false,
                    cli.json,
                )
                .await?;
            }
            Some(LogsCommand::Path) => {
                crate::commands::runtime::show_log_files(&mut client, None, true, cli.json).await?;
            }
            None => unreachable!("empty logs group returned before runtime setup"),
        },
        Command::ApiKey {
            command: Some(command),
        } => {
            crate::commands::runtime::run_apikey(&mut client, command, cli.json).await?;
        }
        Command::Service {
            command: Some(command),
        } => {
            crate::commands::runtime::run_service(&mut client, command, cli.json).await?;
        }
        Command::Alert {
            command: Some(command),
        } => {
            crate::commands::runtime::run_alert(&mut client, command, cli.json).await?;
        }
        Command::Models { command } => match command {
            Some(ModelsCommand::Disable { models }) => {
                crate::commands::runtime::set_models_enabled(&mut client, &models, false, cli.json)
                    .await?;
            }
            Some(ModelsCommand::Enable { models }) => {
                crate::commands::runtime::set_models_enabled(&mut client, &models, true, cli.json)
                    .await?;
            }
            Some(ModelsCommand::List {
                mapped,
                refresh,
                provider,
                provider_kind,
            }) => {
                if provider == "kiro" && provider_kind.is_none() {
                    if refresh {
                        refresh_kiro_models(&mut client).await?;
                    }
                    crate::commands::runtime::show_models(&mut client, mapped, cli.json).await?;
                } else {
                    show_provider_models(
                        &mut client,
                        mapped,
                        refresh,
                        &provider,
                        provider_kind.as_deref(),
                        cli.json,
                    )
                    .await?;
                }
            }
            Some(ModelsCommand::Resolve {
                model,
                api_key,
                refresh,
                provider,
            }) => {
                if let Some(provider) = provider {
                    show_provider_model_resolution(
                        &mut client,
                        &provider,
                        &model,
                        api_key.as_deref(),
                        refresh,
                        cli.json,
                    )
                    .await?;
                } else {
                    if refresh {
                        refresh_kiro_models(&mut client).await?;
                    }
                    crate::commands::runtime::show_model_resolution(
                        &mut client,
                        &model,
                        api_key.as_deref(),
                        cli.json,
                    )
                    .await?;
                }
            }
            None => unreachable!("empty models group returned before runtime setup"),
        },
        Command::ModelMap {
            command: Some(command),
        } => {
            crate::commands::runtime::run_model_map(&mut client, command, cli.json).await?;
        }
        Command::Config { command: None }
        | Command::Provider { command: None }
        | Command::Account { command: None }
        | Command::ApiKey { command: None }
        | Command::Service { command: None }
        | Command::Alert { command: None }
        | Command::ModelMap { command: None }
        | Command::Help { .. }
        | Command::Guide { .. }
        | Command::Completions { .. } => {
            unreachable!("local navigation returned before runtime setup")
        }
    }
    Ok(())
}

async fn show_providers(
    client: &mut AdminClient,
    provider: Option<&str>,
    provider_kind: Option<&str>,
    detail: bool,
    json: bool,
) -> Result<()> {
    if detail {
        let provider = provider.expect("provider show has an ID");
        let value: crosslane_core::provider::ProviderDescriptor = client
            .call(
                method::V2_PROVIDER_SHOW,
                serde_json::json!({"provider":provider}),
            )
            .await?;
        if json {
            return print_json(&value);
        }
        println!(
            "{}   {}   {}",
            value.id,
            cli::scope::kind_name(&value.kind),
            display_runtime_status(&value.status)
        );
        println!("启用      {}", if value.enabled { "是" } else { "否" });
        println!(
            "协议      {}",
            value
                .capabilities
                .protocols
                .iter()
                .map(|protocol| protocol.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!(
            "认证      device-flow={} token-import={}",
            value.capabilities.device_flow, value.capabilities.token_import
        );
        if let Some(error) = value.error {
            println!("错误      {error}");
        }
        cli::scope::print_next_steps(value.id.as_str(), &value.kind);
        return Ok(());
    }
    let value: serde_json::Value = client
        .call(
            method::V2_PROVIDER_LIST,
            serde_json::json!({"provider":provider,"provider_kind":provider_kind}),
        )
        .await?;
    if json {
        return print_json(&value);
    }
    let providers = value["providers"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("daemon 返回的 provider 列表无效"))?;
    let rows = providers
        .iter()
        .map(|provider| {
            let protocols = provider["capabilities"]["protocols"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>()
                .join(",");
            vec![
                provider["id"].as_str().unwrap_or("-").into(),
                cli::scope::kind_name(provider["kind"].as_str().unwrap_or("-")).into(),
                if provider["enabled"].as_bool().unwrap_or(false) {
                    "是".into()
                } else {
                    "否".into()
                },
                display_runtime_status(provider["status"].as_str().unwrap_or("unknown")).into(),
                protocols,
                provider["error"].as_str().unwrap_or("").into(),
            ]
        })
        .collect::<Vec<_>>();
    println!(
        "{}",
        render_table(
            &[
                "实例 ID (--provider)",
                "类型 (kind)",
                "启用",
                "状态",
                "协议",
                "错误"
            ],
            &rows
        )
    );
    println!(
        "--provider/-p 使用第一列实例 ID；用 `crosslane provider show <ID>` 查看该实例的接入命令。"
    );
    Ok(())
}

fn display_runtime_status(status: &str) -> &str {
    match status {
        "ready" | "available" | "running" => "可用",
        "disabled" => "已停用",
        "unavailable" | "stopped" => "不可用",
        "unsupported" => "不支持",
        "not_ready" => "未就绪",
        "error" => "异常",
        "unknown" => "未知",
        other => other,
    }
}

async fn show_provider_models(
    client: &mut AdminClient,
    mapped: bool,
    refresh: bool,
    provider: &str,
    provider_kind: Option<&str>,
    json: bool,
) -> Result<()> {
    let result: crosslane_ipc::protocol::ProviderModelListResult = client
        .call(
            method::V2_MODELS,
            serde_json::json!({
                "provider":provider,
                "provider_kind":provider_kind,
                "refresh":refresh
            }),
        )
        .await?;
    let config = if mapped {
        let show: crosslane_ipc::protocol::ConfigShowResult = client
            .call(method::CONFIG_SHOW, serde_json::json!({}))
            .await?;
        Some(
            serde_json::from_value::<crosslane_core::config::Config>(show.effective_json)
                .context("daemon 返回的生效配置无效")?,
        )
    } else {
        None
    };
    let policy = crosslane_core::config::ModelsConfig {
        disabled: result.disabled_models.clone(),
        ..Default::default()
    };
    let mut enriched = result
        .models
        .iter()
        .map(|model| {
            let route = config.as_ref().map(|config| {
                let default_model = config
                    .effective_providers()
                    .into_iter()
                    .find(|provider| provider.id == model.provider_id.as_str())
                    .map(|provider| provider.routing.default_model_id)
                    .unwrap_or_default();
                crosslane_translate::model::map_model_for_provider(
                    &model.id,
                    &config.model_mapping,
                    crosslane_translate::model::ModelMappingContext {
                        provider_id: model.provider_id.as_str(),
                        service_id: None,
                        api_key_id: None,
                        remaining_percent: None,
                    },
                    &default_model,
                )
            });
            serde_json::json!({
                "provider_id":model.provider_id,
                "id":model.id,
                "display_name":model.display_name,
                "vendor":model.vendor,
                "max_input_tokens":model.max_input_tokens,
                "max_output_tokens":model.max_output_tokens,
                "protocols":model.protocols,
                "capabilities":model.capabilities,
                "disabled":policy.is_disabled(&model.id),
                "available":!policy.is_disabled(&model.id),
                "mapped_model":route.as_ref().map(|route| route.mapped.as_str()),
                "mapping_rule":route.and_then(|route| route.rule),
            })
        })
        .collect::<Vec<_>>();
    for id in &policy.disabled {
        if !result.models.iter().any(|model| {
            crosslane_core::model_policy::model_key(&model.id)
                == crosslane_core::model_policy::model_key(id)
        }) {
            enriched.push(
                serde_json::json!({"provider_id":"*", "id":id, "disabled":true, "available":false}),
            );
        }
    }
    if json {
        print_json(&serde_json::json!({
            "schema_version":result.schema_version,
            "scope":result.scope,
            "models":enriched,
            "disabled_models":result.disabled_models,
            "errors":result.errors,
            "complete":result.complete
        }))?;
    } else {
        let rows = enriched
            .iter()
            .map(|model| {
                vec![
                    model["provider_id"].as_str().unwrap_or("-").into(),
                    model["id"].as_str().unwrap_or("-").into(),
                    if model["disabled"] == true {
                        "不可用（已禁用）".into()
                    } else {
                        "可用".into()
                    },
                    format_token_limit(model["max_input_tokens"].as_u64()),
                    format_token_limit(model["max_output_tokens"].as_u64()),
                    model["protocols"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(serde_json::Value::as_str)
                        .collect::<Vec<_>>()
                        .join(","),
                    model["mapped_model"].as_str().unwrap_or("-").into(),
                ]
            })
            .collect::<Vec<_>>();
        println!(
            "{}",
            render_table(
                &[
                    "PROVIDER",
                    "MODEL",
                    "状态",
                    "输入上限",
                    "输出上限",
                    "协议",
                    "映射后"
                ],
                &rows,
            )
        );
        for (provider, error) in &result.errors {
            eprintln!("{provider}: {error}");
        }
    }
    if result.complete {
        Ok(())
    } else {
        anyhow::bail!("部分提供源的模型发现失败")
    }
}

async fn refresh_kiro_models(client: &mut AdminClient) -> Result<()> {
    let result: crosslane_ipc::protocol::ProviderModelListResult = client
        .call(
            method::V2_MODELS,
            serde_json::json!({"provider":"kiro","refresh":true}),
        )
        .await?;
    if result.complete {
        return Ok(());
    }
    let message = result
        .errors
        .get("kiro")
        .cloned()
        .unwrap_or_else(|| "Kiro 模型刷新失败".into());
    Err(anyhow::anyhow!(message))
}

async fn show_provider_model_resolution(
    client: &mut AdminClient,
    provider: &str,
    model: &str,
    api_key: Option<&str>,
    refresh: bool,
    json: bool,
) -> Result<()> {
    let show: crosslane_ipc::protocol::ConfigShowResult = client
        .call(method::CONFIG_SHOW, serde_json::json!({}))
        .await?;
    let config: crosslane_core::config::Config =
        serde_json::from_value(show.effective_json).context("daemon 返回的生效配置无效")?;
    let api_key_id = api_key
        .map(|selector| {
            config
                .api_key
                .iter()
                .find(|key| key.id.as_deref() == Some(selector) || key.name == selector)
                .and_then(|key| key.id.clone())
                .ok_or_else(|| anyhow::anyhow!("API key 不存在：{selector}"))
        })
        .transpose()?;
    let providers = config.effective_providers();
    let provider_config = providers
        .iter()
        .find(|item| item.id == provider)
        .ok_or_else(|| anyhow::anyhow!("provider 不存在：{provider}"))?;
    let source_model = model.strip_prefix(&format!("{provider}/")).unwrap_or(model);
    let route = crosslane_translate::model::map_model_for_provider(
        source_model,
        &config.model_mapping,
        crosslane_translate::model::ModelMappingContext {
            provider_id: provider,
            service_id: None,
            api_key_id: api_key_id.as_deref(),
            remaining_percent: None,
        },
        &provider_config.routing.default_model_id,
    );
    let (resolved_provider, target) = mapped_provider_target(&providers, provider, &route.mapped);
    let resolved_provider_config = providers
        .iter()
        .find(|item| item.id == resolved_provider)
        .ok_or_else(|| anyhow::anyhow!("映射目标 provider 不存在：{resolved_provider}"))?;
    let catalog: crosslane_ipc::protocol::ProviderModelListResult = client
        .call(
            method::V2_MODELS,
            serde_json::json!({"provider":resolved_provider,"refresh":refresh}),
        )
        .await?;
    let resolved_model = if resolved_provider_config.kind == "kiro" {
        let available = catalog
            .models
            .iter()
            .map(|candidate| candidate.id.clone())
            .collect::<Vec<_>>();
        crosslane_translate::model::resolve_dynamic_model(target, &available)
    } else {
        catalog
            .models
            .iter()
            .find(|candidate| candidate.id == target)
            .map(|candidate| candidate.id.clone())
    };
    let disabled = [
        Some(model),
        Some(route.mapped.as_str()),
        resolved_model.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|model| config.is_model_disabled(model));
    let available = !disabled
        && provider_config.enabled
        && resolved_provider_config.enabled
        && resolved_model.is_some();
    let value = serde_json::json!({
        "provider_id":provider,
        "input_model":model,
        "mapped_model":route.mapped,
        "mapping_rule":route.rule,
        "resolved_provider_id":resolved_provider,
        "resolved_model":available.then_some(resolved_model).flatten(),
        "available":available,
        "disabled":disabled,
        "provider_enabled":provider_config.enabled,
        "resolved_provider_enabled":resolved_provider_config.enabled,
        "catalog_size":catalog.models.len(),
        "catalog_complete":catalog.complete,
        "errors":catalog.errors,
    });
    if json {
        print_json(&value)
    } else {
        println!("提供源    {provider}");
        println!("输入模型  {model}");
        println!(
            "映射结果  {}",
            value["mapped_model"].as_str().unwrap_or("-")
        );
        println!(
            "命中规则  {}",
            value["mapping_rule"].as_str().unwrap_or("-")
        );
        println!(
            "实际源    {}",
            value["resolved_provider_id"].as_str().unwrap_or("-")
        );
        println!(
            "实际模型  {}",
            value["resolved_model"].as_str().unwrap_or("不可用")
        );
        Ok(())
    }
}

fn mapped_provider_target<'a>(
    providers: &'a [crosslane_core::config::ProviderConfig],
    source_provider: &'a str,
    mapped_model: &'a str,
) -> (&'a str, &'a str) {
    mapped_model
        .split_once('/')
        .filter(|(provider, _)| providers.iter().any(|candidate| candidate.id == *provider))
        .unwrap_or((source_provider, mapped_model))
}

fn format_token_limit(value: Option<u64>) -> String {
    let Some(value) = value else {
        return "-".into();
    };
    if value >= 1_000_000 && value.is_multiple_of(1_000_000) {
        format!("{}M", value / 1_000_000)
    } else if value >= 1_000 && value.is_multiple_of(1_000) {
        format!("{}K", value / 1_000)
    } else {
        value.to_string()
    }
}

fn parse_time_range_args(range: &TimeRangeArgs) -> Result<(Option<u64>, Option<i64>, Option<i64>)> {
    let since_secs = range
        .since
        .as_deref()
        .map(crate::commands::runtime::parse_duration)
        .transpose()?;
    let start_secs = range
        .start
        .as_deref()
        .map(crate::commands::runtime::parse_timestamp)
        .transpose()?;
    let end_secs = range
        .end
        .as_deref()
        .map(crate::commands::runtime::parse_timestamp)
        .transpose()?;
    if start_secs
        .zip(end_secs)
        .is_some_and(|(start, end)| start > end)
    {
        anyhow::bail!("起始时间不能晚于结束时间");
    }
    Ok((since_secs, start_secs, end_secs))
}

fn print_status(status: &StatusResult) {
    println!(
        "crosslaned {}   运行 {}   PID {}",
        status.version,
        format_relative(status.uptime_secs as i64),
        status.pid
    );
    println!(
        "监听    {}        管理 {}",
        status.listen, status.admin_socket
    );
    println!(
        "代理    {} 个（{} 运行）",
        status.proxy_service_total, status.proxy_service_running
    );
    if !status.providers.is_empty() {
        let rows = status
            .providers
            .iter()
            .map(|provider| {
                vec![
                    provider.id.clone(),
                    cli::scope::kind_name(&provider.kind).to_owned(),
                    provider.status.clone(),
                    format!("{}/{}", provider.account_available, provider.account_total),
                    provider.error.clone().unwrap_or_default(),
                ]
            })
            .collect::<Vec<_>>();
        println!(
            "{}",
            render_table(
                &["实例 ID", "类型 (kind)", "状态", "可用/账号", "错误"],
                &rows
            )
        );
    }
    println!(
        "账号    {} 个（{} 可调度 / {} 额度保护 / {} 冷却 / {} 额度耗尽 / {} 封禁 / {} 刷新中 / {} 停用）",
        status.account_total,
        status.account_available,
        status.account_protected,
        status.account_cooling,
        status.account_exhausted,
        status.account_banned,
        status.account_refreshing,
        status.account_total.saturating_sub(status.account_enabled)
    );
    if status.ready {
        println!("就绪    是");
    } else {
        println!("就绪    否（{}）", status.readiness_reasons.join("；"));
    }
    println!(
        "并发    {} 进行中 / 上限 {}     队列 {} 等待",
        status.active_requests, status.max_concurrent_requests, status.queued_requests
    );
    let stats_scope = if status.stats_scope == "session" {
        "本次启动"
    } else {
        "累计"
    };
    println!(
        "统计    {}   {} 请求   {:.1}% 成功   均值 {}ms   credits {:.2}",
        stats_scope,
        status.request_count,
        status.success_rate,
        status.average_latency_ms,
        status.credits
    );
    if let (Some(start), Some(end)) = (status.stats_start, status.stats_end) {
        println!(
            "范围    {} ～ {}（分钟级聚合）",
            format_timestamp(start),
            format_timestamp(end)
        );
    }
    if status.stats_truncated {
        println!("提示    指定范围早于本次服务启动时间，已从启动时间起统计");
    }
    if status.daily_credit_limit > 0.0 {
        println!(
            "日额度  {}   {:.2} 已用 + {:.2} 在途 / {:.2}",
            status.daily_credit_day,
            status.daily_credit_used,
            status.daily_credit_reserved,
            status.daily_credit_limit
        );
    }
    match status.config_reloaded_at {
        Some(at) => println!(
            "配置    {}（{} 重载）",
            status.config_path,
            format_timestamp(at)
        ),
        None => println!("配置    {}", status.config_path),
    }
    if let Some(hint) = &status.hint {
        println!("提示    {hint}");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn provider_selector_is_shared_at_every_command_depth() {
        for args in [
            vec![
                "crosslane",
                "--provider",
                "copilot",
                "account",
                "tag",
                "acc_test",
                "--add",
                "prod",
            ],
            vec![
                "crosslane",
                "account",
                "--provider",
                "copilot",
                "tag",
                "acc_test",
                "--add",
                "prod",
            ],
            vec![
                "crosslane",
                "account",
                "tag",
                "acc_test",
                "--add",
                "prod",
                "--provider",
                "copilot",
            ],
        ] {
            let cli = super::Cli::try_parse_from(args).unwrap();
            let Some(super::Command::Account {
                command: Some(crate::commands::account::AccountCommand::Tag { provider, .. }),
            }) = cli.command
            else {
                panic!("tag");
            };
            assert_eq!(provider.as_deref(), Some("copilot"));
        }
        let cli =
            super::Cli::try_parse_from(["crosslane", "--provider", "copilot", "models", "list"])
                .unwrap();
        let Some(super::Command::Models {
            command: Some(super::ModelsCommand::List { provider, .. }),
        }) = cli.command
        else {
            panic!("models");
        };
        assert_eq!(provider, "copilot");
        let cli = super::Cli::try_parse_from([
            "crosslane",
            "--provider",
            "copilot",
            "service",
            "create",
            "--name",
            "test",
            "--port",
            "5581",
        ])
        .unwrap();
        let Some(super::Command::Service {
            command: Some(crate::commands::runtime::ServiceCommand::Create { providers, .. }),
        }) = cli.command
        else {
            panic!("service");
        };
        assert_eq!(providers, ["copilot"]);
        let cli = super::Cli::try_parse_from([
            "crosslane",
            "--provider",
            "kiro,copilot",
            "service",
            "create",
            "--name",
            "both",
            "--port",
            "5582",
        ])
        .unwrap();
        let Some(super::Command::Service {
            command: Some(crate::commands::runtime::ServiceCommand::Create { providers, .. }),
        }) = cli.command
        else {
            panic!("service");
        };
        assert_eq!(providers, ["kiro", "copilot"]);
    }

    use super::*;

    #[test]
    fn model_resolution_checks_the_mapped_provider_catalog() {
        let providers = vec![
            crosslane_core::config::ProviderConfig::default(),
            crosslane_core::config::ProviderConfig {
                id: "copilot".into(),
                kind: "copilot".into(),
                ..crosslane_core::config::ProviderConfig::default()
            },
        ];

        assert_eq!(
            mapped_provider_target(&providers, "kiro", "copilot/gpt-test"),
            ("copilot", "gpt-test")
        );
        assert_eq!(
            mapped_provider_target(&providers, "kiro", "claude-sonnet-test"),
            ("kiro", "claude-sonnet-test")
        );
        assert_eq!(
            mapped_provider_target(&providers, "kiro", "vendor/model"),
            ("kiro", "vendor/model")
        );
    }

    #[test]
    fn config_reset_is_available_as_a_subcommand() {
        let cli = Cli::try_parse_from(["crosslane", "config", "reset"]).expect("config reset");
        assert!(matches!(
            cli.command,
            Some(Command::Config {
                command: Some(ConfigCommand::Reset {
                    module: None,
                    yes: false,
                })
            })
        ));

        let scoped = Cli::try_parse_from(["crosslane", "config", "reset", "pool"])
            .expect("scoped config reset");
        assert!(matches!(
            scoped.command,
            Some(Command::Config {
                command: Some(ConfigCommand::Reset {
                    module: Some(module),
                    yes: false,
                })
            }) if module == "pool"
        ));
    }

    #[test]
    fn provider_concurrency_zero_keeps_the_inherit_semantics() {
        let cli = Cli::try_parse_from([
            "crosslane",
            "provider",
            "edit",
            "kiro",
            "--max-concurrent-per-account",
            "0",
        ])
        .expect("zero inherits global limit");
        assert!(matches!(
            cli.command,
            Some(Command::Provider {
                command: Some(ProviderCommand::Edit {
                    max_concurrent_per_account: Some(0),
                    ..
                })
            })
        ));
    }

    #[test]
    fn config_commands_accept_module_scoped_show_and_edit() {
        let list = Cli::try_parse_from(["crosslane", "config", "list"]).expect("config list");
        assert!(matches!(
            list.command,
            Some(Command::Config {
                command: Some(ConfigCommand::List)
            })
        ));

        let show = Cli::try_parse_from(["crosslane", "config", "show", "server", "--effective"])
            .expect("config show module");
        assert!(matches!(
            show.command,
            Some(Command::Config {
                command: Some(ConfigCommand::Show {
                    module: Some(module),
                    effective: true,
                    show_secrets: false,
                })
            }) if module == "server"
        ));

        let edit = Cli::try_parse_from(["crosslane", "config", "edit", "pool"])
            .expect("config edit module");
        assert!(matches!(
            edit.command,
            Some(Command::Config {
                command: Some(ConfigCommand::Edit {
                    module: Some(module),
                })
            }) if module == "pool"
        ));

        let full_edit =
            Cli::try_parse_from(["crosslane", "config", "edit"]).expect("full config edit");
        assert!(matches!(
            full_edit.command,
            Some(Command::Config {
                command: Some(ConfigCommand::Edit { module: None })
            })
        ));
    }

    #[test]
    fn account_rm_accepts_one_or_multiple_accounts() {
        let single = Cli::try_parse_from(["crosslane", "account", "rm", "acc_00000001"])
            .expect("single account remove");
        assert!(matches!(
            single.command,
            Some(Command::Account {
                command: Some(crate::commands::account::AccountCommand::Rm { ids, .. })
            }) if ids == ["acc_00000001"]
        ));

        let multiple = Cli::try_parse_from([
            "crosslane",
            "account",
            "rm",
            "acc_00000001",
            "a@example.com",
        ])
        .expect("batch account remove");
        assert!(matches!(
            multiple.command,
            Some(Command::Account {
                command: Some(crate::commands::account::AccountCommand::Rm { ids, .. })
            }) if ids == ["acc_00000001", "a@example.com"]
        ));

        assert!(Cli::try_parse_from(["crosslane", "account", "rm"]).is_err());
    }

    #[test]
    fn account_overage_commands_are_explicit_and_scriptable() {
        let show =
            Cli::try_parse_from(["crosslane", "account", "overage"]).expect("default overage view");
        assert!(matches!(
            show.command,
            Some(Command::Account {
                command: Some(crate::commands::account::AccountCommand::Overage { command: None })
            })
        ));

        let refresh = Cli::try_parse_from(["crosslane", "account", "overage", "show", "--refresh"])
            .expect("refresh overage view");
        assert!(matches!(
            refresh.command,
            Some(Command::Account {
                command: Some(crate::commands::account::AccountCommand::Overage {
                    command: Some(crate::commands::account::AccountOverageCommand::Show {
                        refresh: true
                    })
                })
            })
        ));

        let enable = Cli::try_parse_from([
            "crosslane",
            "account",
            "overage",
            "enable",
            "--max-credits",
            "500",
            "--no-refresh",
        ])
        .expect("enable overage with local limit");
        assert!(matches!(
            enable.command,
            Some(Command::Account {
                command: Some(crate::commands::account::AccountCommand::Overage {
                    command: Some(crate::commands::account::AccountOverageCommand::Enable {
                        max_credits: Some(500.0),
                        kiro_limit: false,
                        no_refresh: true
                    })
                })
            })
        ));

        let kiro_limit =
            Cli::try_parse_from(["crosslane", "account", "overage", "enable", "--kiro-limit"])
                .expect("enable overage with Kiro limit");
        assert!(matches!(
            kiro_limit.command,
            Some(Command::Account {
                command: Some(crate::commands::account::AccountCommand::Overage {
                    command: Some(crate::commands::account::AccountOverageCommand::Enable {
                        max_credits: None,
                        kiro_limit: true,
                        no_refresh: false
                    })
                })
            })
        ));

        assert!(Cli::try_parse_from([
            "crosslane",
            "account",
            "overage",
            "enable",
            "--max-credits",
            "-1"
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "crosslane",
            "account",
            "overage",
            "enable",
            "--max-credits",
            "500",
            "--kiro-limit"
        ])
        .is_err());
    }

    #[test]
    fn account_tag_accepts_one_or_multiple_accounts() {
        let single = Cli::try_parse_from([
            "crosslane",
            "account",
            "tag",
            "acc_00000001",
            "--add",
            "prod",
        ])
        .expect("legacy single-account tag command");
        assert!(matches!(
            single.command,
            Some(Command::Account {
                command: Some(crate::commands::account::AccountCommand::Tag {
                    ids,
                    add,
                    remove,
                    ..
                })
            }) if ids == ["acc_00000001"] && add == ["prod"] && remove.is_empty()
        ));

        let multiple = Cli::try_parse_from([
            "crosslane",
            "account",
            "tag",
            "--add",
            "test",
            "acc_2c36cfad",
            "acc_332c7cb2",
            "acc_41c6e3ad",
        ])
        .expect("batch account tag command");
        assert!(matches!(
            multiple.command,
            Some(Command::Account {
                command: Some(crate::commands::account::AccountCommand::Tag {
                    ids,
                    add,
                    remove,
                    ..
                })
            }) if ids == ["acc_2c36cfad", "acc_332c7cb2", "acc_41c6e3ad"]
                && add == ["test"] && remove.is_empty()
        ));
        assert!(Cli::try_parse_from(["crosslane", "account", "tag", "--add", "test"]).is_err());
    }

    #[test]
    fn account_creation_accepts_tags_and_services_lookup() {
        let services =
            Cli::try_parse_from(["crosslane", "account", "services", "alice@example.com"])
                .expect("account services");
        assert!(matches!(
            services.command,
            Some(Command::Account {
                command: Some(crate::commands::account::AccountCommand::Services { id, .. })
            }) if id == "alice@example.com"
        ));

        let import = Cli::try_parse_from([
            "crosslane",
            "account",
            "import",
            "--file",
            "accounts.json",
            "--tag",
            "team-a,prod",
            "--tag",
            "shared",
        ])
        .expect("tagged import");
        assert!(matches!(
            import.command,
            Some(Command::Account {
                command: Some(crate::commands::account::AccountCommand::Import { tags, .. })
            }) if tags == ["team-a", "prod", "shared"]
        ));

        let api_key = Cli::try_parse_from([
            "crosslane",
            "account",
            "add-api-key",
            "--email",
            "ci@example.com",
            "--tag",
            "ci",
        ])
        .expect("tagged API key account");
        assert!(matches!(
            api_key.command,
            Some(Command::Account {
                command: Some(crate::commands::account::AccountCommand::AddApiKey { tags, .. })
            }) if tags == ["ci"]
        ));

        let sso_batch = Cli::try_parse_from([
            "crosslane",
            "account",
            "add-sso",
            "--batch",
            "accounts.csv",
            "--tag",
            "team-a",
        ])
        .expect("tagged SSO batch");
        assert!(matches!(
            sso_batch.command,
            Some(Command::Account {
                command: Some(crate::commands::account::AccountCommand::AddSso { tags, .. })
            }) if tags == ["team-a"]
        ));
    }

    #[test]
    fn status_and_stats_accept_explicit_time_ranges() {
        let status = Cli::try_parse_from([
            "crosslane",
            "status",
            "--start",
            "2026-08-27T10:00:00+08:00",
            "--end",
            "2026-08-27T12:00:00+08:00",
        ])
        .expect("status range");
        let Some(Command::Status { range, .. }) = status.command else {
            panic!("expected status command");
        };
        let (_, start, end) = parse_time_range_args(&range).expect("parsed range");
        assert_eq!(start, Some(1_787_796_000));
        assert_eq!(end, Some(1_787_803_200));

        let stats =
            Cli::try_parse_from(["crosslane", "stats", "--since", "1h"]).expect("stats since");
        let Some(Command::Stats { range, .. }) = stats.command else {
            panic!("expected stats command");
        };
        assert_eq!(parse_time_range_args(&range).expect("since").0, Some(3_600));
        assert!(Cli::try_parse_from(["crosslane", "stats", "--detail", "--recent", "0"]).is_ok());
        assert!(
            Cli::try_parse_from(["crosslane", "stats", "--detail", "--recent", "1000"]).is_ok()
        );
        assert!(
            Cli::try_parse_from(["crosslane", "stats", "--detail", "--recent", "1001"]).is_err()
        );
        assert!(Cli::try_parse_from([
            "crosslane",
            "stats",
            "--since",
            "1h",
            "--start",
            "2026-08-27T10:00:00+08:00"
        ])
        .is_err());
    }

    #[test]
    fn models_supports_resolution_without_breaking_list_flags() {
        let explicit_list =
            Cli::try_parse_from(["crosslane", "models", "list", "--mapped", "--refresh"])
                .expect("explicit model list");
        assert!(matches!(
            explicit_list.command,
            Some(Command::Models {
                command: Some(ModelsCommand::List {
                    mapped: true,
                    refresh: true,
                    ..
                })
            })
        ));

        let resolve = Cli::try_parse_from([
            "crosslane",
            "models",
            "resolve",
            "opus5",
            "--api-key",
            "production",
            "--refresh",
        ])
        .expect("model resolve");
        let Some(Command::Models {
            command:
                Some(ModelsCommand::Resolve {
                    model,
                    api_key,
                    refresh: true,
                    ..
                }),
        }) = resolve.command
        else {
            panic!("expected models resolve command");
        };
        assert_eq!(model, "opus5");
        assert_eq!(api_key.as_deref(), Some("production"));
    }

    #[test]
    fn task_list_and_full_diagnosis_have_explicit_actions() {
        let tasks = Cli::try_parse_from(["crosslane", "tasks", "list"]).expect("task list");
        assert!(matches!(
            tasks.command,
            Some(Command::Tasks {
                command: Some(TaskCommand::List)
            })
        ));

        let diagnose = Cli::try_parse_from([
            "crosslane",
            "diagnose",
            "all",
            "--region",
            "us-west-2",
            "--timeout",
            "30s",
            "--concurrency",
            "4",
        ])
        .expect("full diagnosis");
        assert!(matches!(
            diagnose.command,
            Some(Command::Diagnose {
                command: Some(DiagnoseCommand::All {
                    region,
                    timeout,
                    concurrency: 4,
                })
            }) if region == "us-west-2" && timeout == 30
        ));
    }

    #[test]
    fn logs_support_discovery_subcommands() {
        let files = Cli::try_parse_from(["crosslane", "logs", "files", "--level", "error"])
            .expect("log files command");
        assert!(matches!(
            files.command,
            Some(Command::Logs {
                command: Some(LogsCommand::Files {
                    level: Some(LogFileLevel::Error)
                }),
                ..
            })
        ));
        let trace_id = "trace_0123456789abcdef0123456789abcdef";
        let trace = Cli::try_parse_from([
            "crosslane",
            "logs",
            "trace",
            trace_id,
            "--tail",
            "250",
            "--level",
            "warn",
        ])
        .expect("trace log command");
        assert!(matches!(
            trace.command,
            Some(Command::Logs {
                command: Some(LogsCommand::Trace {
                    trace_id: parsed,
                    tail: 250,
                    level: Some(LogFileLevel::Warn),
                }),
                ..
            }) if parsed == trace_id
        ));
        assert!(Cli::try_parse_from(["crosslane", "logs", "--tail", "10"]).is_err());
        assert!(Cli::try_parse_from(["crosslane", "logs", "-f"]).is_err());
        assert!(Cli::try_parse_from(["crosslane", "models", "--mapped"]).is_err());
        assert!(Cli::try_parse_from(["crosslane", "account", "add-sso-batch"]).is_err());
    }

    #[test]
    fn service_edit_and_apikey_limit_clear_have_convenient_syntax() {
        let service = Cli::try_parse_from([
            "crosslane",
            "service",
            "edit",
            "main",
            "--port",
            "5581",
            "--skip-user-agent-check",
            "false",
            "--add-api-key",
            "ci,team",
        ])
        .expect("service edit");
        let Some(Command::Service {
            command:
                Some(crate::commands::runtime::ServiceCommand::Edit {
                    service,
                    port,
                    skip_user_agent_check,
                    add_api_key,
                    ..
                }),
        }) = service.command
        else {
            panic!("expected service edit command");
        };
        assert_eq!(service, "main");
        assert_eq!(port, Some(5581));
        assert_eq!(skip_user_agent_check, Some(false));
        assert_eq!(add_api_key, vec!["ci", "team"]);

        let key = Cli::try_parse_from([
            "crosslane",
            "apikey",
            "edit",
            "ci",
            "--skip-user-agent-check",
            "true",
        ])
        .expect("API key policy edit");
        assert!(matches!(
            key.command,
            Some(Command::ApiKey {
                command: Some(crate::commands::runtime::ApiKeyCommand::Edit {
                    skip_user_agent_check: Some(true),
                    ..
                })
            })
        ));

        let limit = Cli::try_parse_from(["crosslane", "apikey", "limit", "ci", "--clear"])
            .expect("clear API key limit");
        assert!(matches!(
            limit.command,
            Some(Command::ApiKey {
                command: Some(crate::commands::runtime::ApiKeyCommand::Limit {
                    clear: true,
                    credits: None,
                    ..
                })
            })
        ));
    }

    #[test]
    fn service_create_and_edit_accept_multiple_account_tags() {
        let create = Cli::try_parse_from([
            "crosslane",
            "service",
            "create",
            "--name",
            "team",
            "--account-tag",
            "xx1",
            "xx2",
        ])
        .expect("create with multiple account tags");
        assert!(matches!(
            create.command,
            Some(Command::Service {
                command: Some(crate::commands::runtime::ServiceCommand::Create { account_tag, .. })
            }) if account_tag == ["xx1", "xx2"]
        ));

        let edit = Cli::try_parse_from([
            "crosslane",
            "service",
            "edit",
            "team",
            "--account-tag",
            "xx1,xx2",
        ])
        .expect("edit with multiple account tags");
        assert!(matches!(
            edit.command,
            Some(Command::Service {
                command: Some(crate::commands::runtime::ServiceCommand::Edit { account_tag, .. })
            }) if account_tag == ["xx1", "xx2"]
        ));
        assert!(Cli::try_parse_from([
            "crosslane",
            "service",
            "create",
            "--name",
            "team",
            "--account-tag"
        ])
        .is_err());
    }

    #[test]
    fn user_agent_policy_creation_flags_accept_explicit_booleans() {
        let service = Cli::try_parse_from([
            "crosslane",
            "service",
            "create",
            "--name",
            "compatible",
            "--skip-user-agent-check",
            "true",
        ])
        .expect("service creation policy");
        assert!(matches!(
            service.command,
            Some(Command::Service {
                command: Some(crate::commands::runtime::ServiceCommand::Create {
                    skip_user_agent_check: true,
                    ..
                })
            })
        ));

        let key = Cli::try_parse_from([
            "crosslane",
            "apikey",
            "add",
            "--name",
            "compatible",
            "--skip-user-agent-check",
            "true",
        ])
        .expect("API key creation policy");
        assert!(matches!(
            key.command,
            Some(Command::ApiKey {
                command: Some(crate::commands::runtime::ApiKeyCommand::Add {
                    skip_user_agent_check: true,
                    ..
                })
            })
        ));
    }

    #[test]
    fn apikey_add_accepts_a_restored_key_and_create_alias() {
        for command in ["add", "create"] {
            let cli = Cli::try_parse_from([
                "crosslane",
                "apikey",
                command,
                "--name",
                "recovered",
                "--key",
                "sk-original-key",
            ])
            .expect("API key restore command");
            let Some(Command::ApiKey {
                command: Some(crate::commands::runtime::ApiKeyCommand::Add { name, key, .. }),
            }) = cli.command
            else {
                panic!("expected API key add command");
            };
            assert_eq!(name, "recovered");
            assert_eq!(key.as_deref(), Some("sk-original-key"));
        }
    }

    #[test]
    fn alert_command_replaces_the_webhook_entrypoint() {
        let cli = Cli::try_parse_from(["crosslane", "alert", "config"]).expect("alert command");
        let Some(Command::Alert {
            command: Some(crate::commands::runtime::AlertCommand::Config),
        }) = cli.command
        else {
            panic!("expected alert config command");
        };
        assert!(Cli::try_parse_from(["crosslane", "webhook", "list"]).is_err());
    }

    #[test]
    fn alert_events_accept_repeated_and_comma_separated_values() {
        let cli = Cli::try_parse_from([
            "crosslane",
            "alert",
            "add",
            "--name",
            "ops",
            "--platform",
            "dingtalk",
            "--webhook-url",
            "https://example.com/hook",
            "--dingtalk-sign",
            "SEC-test-sign",
            "--event",
            "account-credit-protected,account-quota-exhausted,service-quota-exhausted",
            "--event",
            "token-refresh-failed",
        ])
        .expect("multi-event alert target");
        let Some(Command::Alert {
            command:
                Some(crate::commands::runtime::AlertCommand::Add {
                    webhook_url,
                    events,
                    dingtalk_sign,
                    ..
                }),
        }) = cli.command
        else {
            panic!("expected alert add command");
        };
        assert_eq!(
            events,
            vec![
                crate::commands::runtime::AlertEvent::AccountCreditProtected,
                crate::commands::runtime::AlertEvent::AccountQuotaExhausted,
                crate::commands::runtime::AlertEvent::ServiceQuotaExhausted,
                crate::commands::runtime::AlertEvent::TokenRefreshFailed,
            ]
        );
        assert_eq!(webhook_url, "https://example.com/hook");
        assert_eq!(dingtalk_sign.as_deref(), Some("SEC-test-sign"));
    }

    #[test]
    fn alert_rejects_removed_parameter_aliases() {
        assert!(Cli::try_parse_from([
            "crosslane",
            "alert",
            "add",
            "--name",
            "ops",
            "--kind",
            "wechat-work",
            "--webhook-url",
            "https://example.com/hook",
            "--event",
            "token-refresh-failed",
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "crosslane",
            "alert",
            "add",
            "--name",
            "ops",
            "--platform",
            "dingtalk",
            "--url",
            "https://example.com/hook",
            "--event",
            "token-refresh-failed",
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "crosslane",
            "alert",
            "add",
            "--name",
            "ops",
            "--platform",
            "wechat",
            "--webhook-url",
            "https://example.com/hook",
            "--event",
            "token-refresh-failed",
        ])
        .is_err());
    }

    #[test]
    fn alert_edit_accepts_positional_or_named_target() {
        let positional = Cli::try_parse_from([
            "crosslane",
            "alert",
            "edit",
            "ops",
            "--webhook-url",
            "https://example.com/new-hook",
            "--event",
            "token-refresh-failed",
        ])
        .expect("positional alert target");
        let Some(Command::Alert {
            command:
                Some(crate::commands::runtime::AlertCommand::Edit {
                    target,
                    name,
                    webhook_url,
                    ..
                }),
        }) = positional.command
        else {
            panic!("expected alert edit command");
        };
        assert_eq!(target.as_deref(), Some("ops"));
        assert_eq!(name, None);
        assert_eq!(webhook_url.as_deref(), Some("https://example.com/new-hook"));

        let named = Cli::try_parse_from([
            "crosslane",
            "alert",
            "edit",
            "--name",
            "ops",
            "--platform",
            "feishu",
        ])
        .expect("named alert target");
        let Some(Command::Alert {
            command:
                Some(crate::commands::runtime::AlertCommand::Edit {
                    target,
                    name,
                    platform,
                    ..
                }),
        }) = named.command
        else {
            panic!("expected alert edit command");
        };
        assert_eq!(target, None);
        assert_eq!(name.as_deref(), Some("ops"));
        assert_eq!(
            platform,
            Some(crate::commands::runtime::AlertPlatform::Feishu)
        );
    }

    #[test]
    fn alert_platform_rejects_unknown_values() {
        let error = Cli::try_parse_from([
            "crosslane",
            "alert",
            "add",
            "--name",
            "ops",
            "--platform",
            "unknown",
            "--webhook-url",
            "https://example.com/hook",
            "--event",
            "token-refresh-failed",
        ])
        .expect_err("unknown platform must fail");
        let message = error.to_string();
        assert!(message.contains("possible values"));
        assert!(message.contains("dingtalk"));
        assert!(message.contains("wechat-work"));
    }
}
