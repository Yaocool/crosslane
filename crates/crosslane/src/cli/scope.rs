//! One scope vocabulary for nested help, command discovery and validation.

use clap::{ArgMatches, Command};

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
        ["health" | "version" | "restart" | "stop" | "uninstall" | "config", ..]
        | ["logs", "trace" | "files" | "path"]
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
        | ["model-map", "show" | "delete" | "enable" | "disable"] => Scope::Resource,
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

pub(super) fn decorate(command: &mut Command, path: &mut Vec<String>) {
    let scope = for_path(&path.iter().map(String::as_str).collect::<Vec<_>>());
    let path_segments = path.iter().map(String::as_str).collect::<Vec<_>>();
    let tasks_list = matches!(path_segments.as_slice(), ["tasks", "list"]);
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
        *command = command
            .clone()
            .subcommand(super::help::group_help_command(path));
    }
    if command
        .get_arguments()
        .any(|arg| arg.get_id() == "provider")
    {
        *command = command.clone().mut_arg("provider", |arg| {
            let arg = if arg.get_help().is_none() {
                arg.help("提供源实例 ID；用于选择来源或消歧账号，见 provider list。")
            } else {
                arg
            };
            arg.short('p').value_name("ID")
        });
    } else if (matches!(scope, Scope::Global | Scope::Resource) || tasks_list)
        && !matches!(path_segments.as_slice(), ["models", "disable" | "enable"])
    {
        // Keep accepting the global option for an actionable validation error,
        // but do not advertise a selector that this command cannot apply.
        *command = command.clone().arg(
            clap::Arg::new("provider")
                .long("provider")
                .short('p')
                .value_name("ID")
                .action(clap::ArgAction::Append)
                .value_delimiter(',')
                .env("CROSSLANE_PROVIDER")
                .global(true)
                .hide(true),
        );
    }
    let existing = command
        .get_after_help()
        .map(ToString::to_string)
        .unwrap_or_default();
    let detail = if tasks_list {
        "展示全部周期任务；不接受 --provider。tasks run 仅支持任务列表中的可手工运行项。"
    } else if matches!(
        path_segments.as_slice(),
        ["service", "create"] | ["apikey" | "model-map", "add"]
    ) {
        "此处 --provider/-p 设置新资源允许使用的实例范围，可重复或逗号分隔；省略时为 Kiro。"
    } else if matches!(
        path_segments.as_slice(),
        ["service" | "apikey" | "model-map", "edit"]
    ) {
        "此处 --provider/-p 整体替换资源允许使用的实例范围，可重复或逗号分隔。"
    } else if matches!(path_segments.as_slice(), ["models", "disable" | "enable"]) {
        "模型禁用/启用作用于全部提供源；只接受 --provider all 作为全局范围标记。"
    } else if matches!(path_segments.as_slice(), ["alert", ..]) {
        "告警目标和通知平台是共享资源，不按 provider 筛选；当前可订阅的账号额度/token 事件来自 Kiro。"
    } else {
        match scope {
        Scope::Resource => "使用命令自身的 ID/名称参数定位或创建资源；不接受 --provider 筛选，CROSSLANE_PROVIDER 不影响本操作。",
        Scope::Global => "不按提供源筛选，不接受 --provider；CROSSLANE_PROVIDER 不影响本操作。",
        Scope::Kiro => "Kiro 专项操作；不能用于 Copilot 实例。",
        Scope::Copilot => "--provider/-p 填 GitHub Copilot 实例的 ID。",
        Scope::Navigation => "帮助、指南和补全不需要运行 daemon。",
        Scope::Shared | Scope::Mixed => "--provider/-p 使用 provider list 中的实例 ID；默认范围见本命令的参数说明。",
        }
    };
    let identity = if matches!(
        scope,
        Scope::Shared | Scope::Mixed | Scope::Kiro | Scope::Copilot
    ) || path_segments.as_slice() == ["provider", "add"]
    {
        "\n示例中的 kiro/copilot 是实例 ID；自定义实例请替换为 provider list 中的 ID。"
    } else {
        ""
    };
    let edit_scope = if matches!(
        path_segments.as_slice(),
        ["service" | "apikey" | "model-map", "edit"]
    ) {
        "\n修改来源权限必须显式传入 --provider；CROSSLANE_PROVIDER 不改变资源现有权限。"
    } else {
        ""
    };
    let guide = match path.first().map(String::as_str) {
        Some("restart" | "stop" | "uninstall") => "docker",
        Some("ready") => "health",
        Some("help" | "guide" | "completions") | None => "",
        Some(group) => group,
    };
    let guide_hint = if guide.is_empty() || existing.contains("操作说明：") {
        String::new()
    } else {
        format!("\n\n操作说明：crosslane guide {guide}")
    };
    *command = command.clone().after_help(format!(
        "适用范围：{}。{detail}{identity}{edit_scope}\n\n{existing}{guide_hint}",
        scope.label()
    ));
    for child in command.get_subcommands_mut() {
        path.push(child.get_name().to_owned());
        decorate(child, path);
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
