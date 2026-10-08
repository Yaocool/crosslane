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
    match path {
        [] | ["help" | "guide" | "completions", ..] => Scope::Navigation,
        ["health" | "version" | "restart" | "stop" | "uninstall" | "config", ..]
        | ["logs", "trace" | "files" | "path"]
        | ["models", "disable" | "enable"] => Scope::Global,
        ["diagnose" | "alert", ..]
        | ["account", "overage" | "add-sso" | "add-api-key" | "import" | "regen-machine-id", ..] => {
            Scope::Kiro
        }
        ["account", "login-code"] => Scope::Copilot,
        ["provider", "show" | "add" | "edit" | "enable" | "disable" | "delete"]
        | ["service", "show" | "enable" | "disable" | "delete" | "apikeys"]
        | ["apikey", "show" | "rm" | "enable" | "disable" | "limit" | "reset-usage"]
        | ["model-map", "show" | "delete" | "enable" | "disable"] => Scope::Resource,
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
    if command
        .get_arguments()
        .any(|arg| arg.get_id() == "provider")
    {
        *command = command
            .clone()
            .mut_arg("provider", |arg| arg.short('p').value_name("ID"));
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
    } else {
        match scope {
        Scope::Resource => "使用命令自身的 ID/名称参数定位或创建资源；不接受 --provider 筛选，CROSSLANE_PROVIDER 不影响本操作。",
        Scope::Global => "不按提供源筛选。模型禁用/启用作用于全部提供源；配置模块适用范围见 config list。",
        Scope::Kiro => "Kiro 专项操作；不能用于 Copilot 实例。",
        Scope::Copilot => "--provider/-p 填 GitHub Copilot 实例的 ID。",
        Scope::Shared | Scope::Mixed | Scope::Navigation => "支持来源选择的命令用 --provider/-p 指定实例 ID；具体选项见本命令的 Options。用 provider list 查看实际 ID。",
        }
    };
    let identity = "示例中的 kiro/copilot 是实例 ID 示例；自定义实例请替换为实际 ID，GitHub Copilot 是类型名称。";
    let edit_scope = if matches!(
        path_segments.as_slice(),
        ["service" | "apikey" | "model-map", "edit"]
    ) {
        "\n修改来源权限必须显式传入 --provider；CROSSLANE_PROVIDER 不改变资源现有权限。"
    } else {
        ""
    };
    *command = command.clone().after_help(format!(
        "适用范围：{}。{detail}\n{identity}{edit_scope}\n\n{existing}",
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
