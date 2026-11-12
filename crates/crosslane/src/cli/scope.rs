//! One scope vocabulary for nested help, command discovery and validation.

use clap::{Arg, ArgMatches, Command};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Scope {
    Shared,
    Kiro,
    Copilot,
    Global,
    Resource,
    Mixed,
    Navigation,
}

impl Scope {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Shared => "Kiro / GitHub Copilot",
            Self::Kiro => "仅 Kiro",
            Self::Copilot => "仅 GitHub Copilot",
            Self::Global => "全局",
            Self::Resource => "按资源 ID/名称定位",
            Self::Mixed => "按子命令或任务区分",
            Self::Navigation => "本地帮助",
        }
    }
}

pub(super) fn for_path(path: &[&str]) -> Scope {
    if path.contains(&"help") {
        return Scope::Navigation;
    }
    match path {
        [] | ["help" | "guide" | "completions", ..] => Scope::Navigation,
        ["health" | "version" | "restart" | "stop" | "uninstall" | "config" | "admin", ..]
        | ["logs", "trace" | "files" | "path"]
        | ["logs", "admin", ..]
        | ["alert"]
        | ["alert", "config" | "events" | "platforms" | "list" | "logs"]
        | ["models", "disable" | "enable"] => Scope::Global,
        ["diagnose", ..]
        | ["account", "overage" | "add-sso" | "add-api-key" | "import" | "regen-machine-id", ..] => {
            Scope::Kiro
        }
        ["account", "login-code"] => Scope::Copilot,
        ["provider", "show" | "add" | "edit" | "enable" | "disable" | "delete"]
        | ["service", "show" | "enable" | "disable" | "delete" | "apikeys"]
        | ["apikey", "show" | "rm" | "enable" | "disable" | "limit" | "reset-usage"]
        | ["alert", "add" | "edit" | "delete" | "test"]
        | ["model-map", "delete"] => Scope::Resource,
        ["tasks", "list"] => Scope::Global,
        ["tasks", ..] | ["account" | "logs" | "models"] => Scope::Mixed,
        _ => Scope::Shared,
    }
}

pub(super) fn selected_path(matches: &ArgMatches) -> Vec<&str> {
    let mut path = Vec::new();
    let mut current = matches;
    while let Some((name, child)) = current.subcommand() {
        path.push(name);
        current = child;
    }
    path
}

pub(super) fn decorate(command: &mut Command, path: &mut Vec<String>, global_args: &[Arg]) {
    let path_segments = path.iter().map(String::as_str).collect::<Vec<_>>();
    let segments = path_segments.as_slice();
    let scope = for_path(segments);
    let tasks_list = segments == ["tasks", "list"];
    let global_model_policy = matches!(segments, ["models", "disable" | "enable"]);
    let admin_logs = matches!(segments, ["logs", "admin"] | ["logs", "admin", "show"]);
    let navigation = scope == Scope::Navigation && !segments.is_empty();
    let host_lifecycle = matches!(segments, ["restart" | "stop" | "uninstall"]);
    let uses_local_data = matches!(
        segments,
        ["version"]
            | ["config", "list" | "validate"]
            | ["alert", "config" | "events" | "platforms"]
    );
    // Both help flags show the complete document, including long descriptions
    // and examples. Register group-local help explicitly: Clap's root-level
    // disable_help_subcommand setting is inherited by every child.
    *command = command.clone().disable_help_flag(true).arg(
        clap::Arg::new("help")
            .short('h')
            .long("help")
            .action(clap::ArgAction::HelpLong)
            .help("显示完整帮助与示例"),
    );
    if !path.is_empty() && command.get_subcommands().next().is_some() {
        let inherited = command
            .get_arguments()
            .filter(|arg| arg.is_global_set())
            .cloned()
            .map(|arg| arg.hide(true));
        let help = super::help::group_help_command(path).args(inherited);
        *command = command.clone().subcommand(help);
    }
    // Hide inherited options that do not affect this command. Keep their
    // definitions intact so existing invocations and diagnostics still work.
    for arg in global_args {
        let customized = match arg.get_id().as_str() {
            "provider" if segments == ["guide"] => {
                arg.clone().help("替换 Copilot 接入指南中的实例 ID。")
            }
            "provider" if global_model_policy => arg
                .clone()
                .value_name("all")
                .help("用 all 覆盖 CROSSLANE_PROVIDER，显式选择全局范围。"),
            "provider" if scope == Scope::Kiro => arg
                .clone()
                .value_name("kiro")
                .help("仅接受 kiro。可覆盖 CROSSLANE_PROVIDER 中的其他实例 ID。"),
            "provider" if matches!(scope, Scope::Global | Scope::Resource) || navigation => {
                arg.clone().hide(true)
            }
            "socket" if navigation || host_lifecycle || uses_local_data => arg.clone().hide(true),
            "json" if navigation || host_lifecycle => arg.clone().hide(true),
            _ => continue,
        };
        if !command
            .get_arguments()
            .any(|existing| existing.get_id() == arg.get_id())
        {
            *command = command.clone().arg(customized);
        }
    }
    if command
        .get_arguments()
        .any(|arg| arg.get_id() == "provider" && !arg.is_global_set())
    {
        *command = command.clone().mut_arg("provider", |arg| {
            let arg = if arg.get_help().is_none() {
                arg.help("提供源实例 ID；用于选择来源或消歧账号，见 provider list。")
            } else {
                arg
            };
            arg.short('p').value_name("ID")
        });
    }
    let existing = command
        .get_after_help()
        .map(ToString::to_string)
        .unwrap_or_default();
    let detail = if tasks_list {
        "展示全部周期任务。tasks run 仅支持任务列表中的可手工运行项。"
    } else if matches!(
        segments,
        ["service", "create"] | ["apikey" | "model-map", "add"]
    ) {
        "此处 --provider/-p 设置新资源允许使用的实例范围，可重复或逗号分隔；省略时为 Kiro。"
    } else if matches!(segments, ["service" | "apikey" | "model-map", "edit"]) {
        "此处 --provider/-p 整体替换资源允许使用的实例范围，可重复或逗号分隔。"
    } else if global_model_policy {
        "模型禁用/启用作用于全部提供源；只接受 --provider all 作为全局范围标记。"
    } else if admin_logs {
        "按请求 ID 或日志级别查询 Admin 日志。兼容保留的 --provider 不参与筛选。"
    } else if host_lifecycle {
        "由 Docker 宿主机上的 crosslane wrapper 执行。"
    } else if matches!(segments, ["alert", ..]) {
        "告警目标和通知平台是共享资源，不按 provider 筛选；当前可订阅的账号额度/token 事件来自 Kiro。"
    } else {
        match scope {
        Scope::Resource => "使用命令自身的 ID/名称参数定位或创建资源；不接受 --provider 筛选，CROSSLANE_PROVIDER 不影响本操作。",
        Scope::Global => "不按提供源筛选，不接受 --provider；CROSSLANE_PROVIDER 不影响本操作。",
        Scope::Kiro => "来源固定为 Kiro。",
        Scope::Copilot => "--provider/-p 填 GitHub Copilot 实例的 ID。",
        Scope::Navigation => "帮助、指南和补全不需要运行 daemon。",
        Scope::Shared => "--provider/-p 使用 provider list 中的实例 ID。可用 CROSSLANE_PROVIDER 设置默认值。",
        Scope::Mixed => "各操作的来源范围和默认值见对应帮助。",
        }
    };
    let edit_scope = if matches!(segments, ["service" | "apikey" | "model-map", "edit"]) {
        "\n修改来源权限必须显式传入 --provider。CROSSLANE_PROVIDER 不改变资源现有权限。"
    } else {
        ""
    };
    let guide = match path.first().map(String::as_str) {
        _ if navigation => "",
        Some("restart" | "stop" | "uninstall") => "docker",
        Some("ready") => "health",
        None => "",
        Some(group) => group,
    };
    let guide_hint = if guide.is_empty() || existing.contains("操作说明：") {
        String::new()
    } else {
        format!("\n\n操作说明：crosslane guide {guide}")
    };
    let mut footer = format!("适用范围：{}。{detail}{edit_scope}", scope.label());
    if !existing.is_empty() {
        footer.push_str("\n\n");
        footer.push_str(&existing);
    }
    footer.push_str(&guide_hint);
    *command = command.clone().after_help(footer);
    for child in command.get_subcommands_mut() {
        path.push(child.get_name().to_owned());
        decorate(child, path, global_args);
        path.pop();
    }
}

pub(crate) fn kind_name(kind: &str) -> &str {
    match kind {
        "kiro" => "Kiro (kiro)",
        "copilot" => "GitHub Copilot (copilot)",
        other => other,
    }
}

/// IDs are validated by configuration; do not substitute the driver kind here.
pub(crate) fn print_next_steps(id: &str, kind: &str) {
    let auth = match kind {
        "kiro" => Some(" --auth sso --email <EMAIL> --password-stdin"),
        "copilot" => Some(" --auth device-flow"),
        _ => None,
    };
    println!("下一步（实例 ID：{id}，类型：{}）：", kind_name(kind));
    if let Some(auth) = auth {
        println!("  crosslane account add --provider {id}{auth}");
    }
    println!("  crosslane account list --provider {id}");
    println!("  crosslane model list --provider {id}");
    println!("可设置 export CROSSLANE_PROVIDER={id}，后续支持来源选择的命令可省略 --provider。");
}

pub(crate) fn task_scope(name: &str) -> &'static str {
    match name {
        "account_file_watcher" | "token_refresh" | "status_check" | "health_recheck" => "仅 Kiro",
        "model_cache_refresh" => "Kiro / GitHub Copilot",
        "stats_persist" | "daily_reset" | "proxy_service_reconcile" | "adaptive_admission" => {
            "全局"
        }
        _ => "未知（由 daemon 定义）",
    }
}

pub(crate) const MANUAL_TASKS: [&str; 7] = [
    "token_refresh",
    "status_check",
    "health_recheck",
    "model_cache_refresh",
    "stats_persist",
    "daily_reset",
    "proxy_service_reconcile",
];

pub(crate) fn manual_task_parser() -> clap::builder::PossibleValuesParser {
    clap::builder::PossibleValuesParser::new(MANUAL_TASKS)
}
