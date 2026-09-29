//! Help routing backed by the Clap command tree.

use anyhow::Result;
use clap::error::ErrorKind;
use clap::Command as ClapCommand;
use std::ffi::OsString;

use crate::Command;

pub fn empty_group_path(command: &Command) -> Option<&'static str> {
    match command {
        Command::Provider { command: None }
        | Command::Config { command: None }
        | Command::Account { command: None }
        | Command::ApiKey { command: None }
        | Command::Service { command: None }
        | Command::Alert { command: None }
        | Command::ModelMap { command: None } => Some(match command {
            Command::Provider { .. } => "provider",
            Command::Config { .. } => "config",
            Command::Account { .. } => "account",
            Command::ApiKey { .. } => "apikey",
            Command::Service { .. } => "service",
            Command::Alert { .. } => "alert",
            Command::ModelMap { .. } => "model-map",
            _ => unreachable!(),
        }),
        Command::Diagnose { command: None } => Some("diagnose"),
        Command::Tasks { command: None } => Some("tasks"),
        Command::Logs { command: None } => Some("logs"),
        Command::Models { command: None } => Some("models"),
        _ => None,
    }
}

pub fn print_command_help(path: &[String]) -> Result<()> {
    let mut args = Vec::with_capacity(path.len() + 2);
    args.push(OsString::from("kproxy"));
    args.extend(path.iter().map(OsString::from));
    args.push(OsString::from("--help"));
    match super::command().try_get_matches_from(args) {
        Err(error) if error.kind() == ErrorKind::DisplayHelp => {
            error.print()?;
            Ok(())
        }
        Err(error) => error.exit(),
        Ok(_) => unreachable!("the synthetic --help flag must stop command parsing"),
    }
}

pub fn print_all_commands() {
    let mut root = super::command();
    root.build();
    println!("kproxy 公开命令：");
    print_children(&root, "", 0);
    println!("\n使用 `kproxy help <命令路径>` 查看详细参数；`kproxy guide provider` 查看 Kiro/Copilot 命令适用范围。");
}

pub fn exit_missing_action(path: &str) -> ! {
    let path = vec![path.to_owned()];
    let mut command = command_at_path(&path).unwrap_or_else(super::command);
    let hint = match path[0].as_str() {
        "logs" => "请改用 `kproxy logs show`".to_owned(),
        "models" => "请改用 `kproxy models list`".to_owned(),
        "tasks" => "请改用 `kproxy tasks list`".to_owned(),
        "diagnose" => "请改用 `kproxy diagnose all`".to_owned(),
        group => format!("请用 `kproxy help {group}` 查看可用操作"),
    };
    command
        .error(
            ErrorKind::MissingSubcommand,
            format!("命令组 `{}` 需要明确指定子命令；{hint}", path.join(" ")),
        )
        .exit()
}

fn command_at_path(path: &[String]) -> Option<ClapCommand> {
    let mut command = super::command();
    command.build();
    let mut bin_name = String::from("kproxy");
    for segment in path {
        let next = command.find_subcommand(segment)?.clone();
        bin_name.push(' ');
        bin_name.push_str(next.get_name());
        command = next.disable_help_subcommand(true);
    }
    command.set_bin_name(bin_name);
    Some(command)
}

fn print_children(command: &ClapCommand, prefix: &str, depth: usize) {
    for child in command
        .get_subcommands()
        .filter(|child| !child.is_hide_set())
    {
        let path = if prefix.is_empty() {
            child.get_name().to_owned()
        } else {
            format!("{prefix} {}", child.get_name())
        };
        let aliases = child.get_visible_aliases().collect::<Vec<_>>();
        let display_path = if aliases.is_empty() {
            path.clone()
        } else {
            format!("{path}（别名：{}）", aliases.join(", "))
        };
        let indent = "  ".repeat(depth + 1);
        let about = child
            .get_about()
            .map(ToString::to_string)
            .unwrap_or_default();
        let scope = super::scope::for_path(&path.split_whitespace().collect::<Vec<_>>());
        println!("{indent}{display_path:<36} [{}] {about}", scope.label());
        print_children(child, &path, depth + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_paths_resolve_from_the_real_command_tree() {
        assert!(command_at_path(&["logs".into(), "trace".into()]).is_some());
        assert!(command_at_path(&["apikey".into(), "create".into()]).is_some());
        assert!(command_at_path(&["logs".into(), "missing".into()]).is_none());
    }

    #[test]
    fn help_surfaces_both_provider_workflows() {
        let mut root = super::super::command();
        let root_help = root.render_long_help().to_string();
        assert!(root_help.contains("guide kiro"));
        assert!(root_help.contains("guide copilot"));

        let mut account = command_at_path(&["account".into()]).unwrap();
        let account_help = account.render_long_help().to_string();
        assert!(account_help.contains("account add-sso"));
        assert!(account_help.contains("account add --provider <ID>"));
        assert!(account_help.contains("Copilot 使用 --auth device-flow"));

        let mut service_create = command_at_path(&["service".into(), "create".into()]).unwrap();
        let service_help = service_create.render_long_help().to_string();
        assert!(service_help.contains("--provider kiro"));
        assert!(service_help.contains("--provider copilot"));
    }

    #[test]
    fn provider_scoped_help_does_not_present_kiro_only_features_as_shared() {
        for (path, expected) in [
            (vec!["pool"], "--explain 的评分明细不适用于 Copilot"),
            (vec!["subscriptions"], "Copilot 返回 supported=false"),
            (vec!["alert"], "不代表 Copilot"),
            (vec!["model-map", "add"], "只支持 Kiro"),
            (vec!["config", "list"], "适用来源"),
        ] {
            let path = path.into_iter().map(str::to_owned).collect::<Vec<_>>();
            let mut command = command_at_path(&path).unwrap();
            assert!(
                command.render_long_help().to_string().contains(expected),
                "missing {expected} in {path:?} help"
            );
        }
    }
}
