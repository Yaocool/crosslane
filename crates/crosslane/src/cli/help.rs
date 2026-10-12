//! Help routing backed by the Clap command tree.

use anyhow::Result;
use clap::error::ErrorKind;
use clap::Command as ClapCommand;
use std::ffi::OsString;

use crate::Command;

pub(super) fn group_help_command(path: &[String]) -> ClapCommand {
    ClapCommand::new("help")
        .about("查看本组或指定子命令的完整帮助。")
        .arg(
            clap::Arg::new("all")
                .long("all")
                .action(clap::ArgAction::SetTrue)
                .help("递归列出本组或指定子命令下的公开命令"),
        )
        .arg(
            clap::Arg::new("path")
                .value_name("COMMAND")
                .num_args(0..)
                .help("本组内的子命令路径；省略时显示本组帮助"),
        )
        .after_help(format!(
            "示例：\n  crosslane {} help\n  crosslane {} help --all",
            path.join(" "),
            path.join(" ")
        ))
}

pub(super) fn handle_nested_help(matches: &clap::ArgMatches) -> Result<bool> {
    let mut current = matches;
    let mut path = Vec::new();
    while let Some((name, child)) = current.subcommand() {
        if name == "help" && !path.is_empty() {
            if let Some(segments) = child.get_many::<String>("path") {
                path.extend(segments.cloned());
            }
            if child.get_flag("all") {
                print_all_commands(&path)?;
            } else {
                print_command_help(&path)?;
            }
            return Ok(true);
        }
        path.push(name.to_owned());
        current = child;
    }
    Ok(false)
}

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
    args.push(OsString::from("crosslane"));
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

pub fn print_all_commands(path: &[String]) -> Result<()> {
    let Some(mut root) = command_at_path(path) else {
        return print_command_help(path);
    };
    root.build();
    if root.get_subcommands().next().is_none() {
        return print_command_help(path);
    }
    let prefix = root.get_bin_name().unwrap_or("crosslane").to_owned();
    println!("{prefix} 公开命令：");
    print_children(&root, prefix.strip_prefix("crosslane ").unwrap_or(""), 0);
    println!("\n使用 `crosslane help <命令路径>` 查看详细参数；`crosslane guide provider` 查看 Kiro/Copilot 命令适用范围。");
    Ok(())
}

pub fn exit_missing_action(path: &str) -> ! {
    let path = vec![path.to_owned()];
    let mut command = command_at_path(&path).unwrap_or_else(super::command);
    let hint = match path[0].as_str() {
        "logs" => "请改用 `crosslane logs show`".to_owned(),
        "models" => "请改用 `crosslane models list`".to_owned(),
        "tasks" => "请改用 `crosslane tasks list`".to_owned(),
        "diagnose" => "请改用 `crosslane diagnose all`".to_owned(),
        group => format!("请用 `crosslane help {group}` 查看可用操作"),
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
    let mut bin_name = String::from("crosslane");
    for segment in path {
        let next = command.find_subcommand(segment)?.clone();
        bin_name.push(' ');
        bin_name.push_str(next.get_name());
        command = next;
    }
    command.set_bin_name(bin_name);
    Some(command)
}

fn print_children(command: &ClapCommand, prefix: &str, depth: usize) {
    for child in command
        .get_subcommands()
        .filter(|child| !child.is_hide_set() && (prefix.is_empty() || child.get_name() != "help"))
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
    fn every_public_argument_and_operation_has_usable_help() {
        fn visit(command: &ClapCommand, path: &mut Vec<String>, missing: &mut Vec<String>) {
            for arg in command.get_arguments().filter(|arg| !arg.is_hide_set()) {
                if arg.get_help().is_none() && arg.get_long_help().is_none() {
                    missing.push(format!("{}: {}", path.join(" "), arg.get_id()));
                }
            }
            let mut help_command = command.clone();
            let help = help_command.render_long_help().to_string();
            if !help.contains("示例：") {
                missing.push(format!("{}: missing examples", path.join(" ")));
            }
            let root = path.len() == 1;
            for child in command
                .get_subcommands()
                .filter(|child| !child.is_hide_set() && (root || child.get_name() != "help"))
            {
                path.push(child.get_name().to_owned());
                visit(child, path, missing);
                path.pop();
            }
        }
        let mut root = super::super::command();
        root.build();
        let mut missing = Vec::new();
        visit(&root, &mut vec!["crosslane".into()], &mut missing);
        assert!(
            missing.is_empty(),
            "incomplete command help:\n{}",
            missing.join("\n")
        );
    }

    #[test]
    fn documented_cli_examples_use_valid_commands_and_options() {
        fn visit(command: &ClapCommand, root: &ClapCommand, failures: &mut Vec<String>) {
            for text in [command.get_after_help(), command.get_long_about()]
                .into_iter()
                .flatten()
            {
                for line in text
                    .to_string()
                    .lines()
                    .filter(|line| line.trim().starts_with("crosslane "))
                {
                    let Some(mut args) = shlex::split(line.trim()) else {
                        failures.push(line.to_owned());
                        continue;
                    };
                    if let Some(index) = args
                        .iter()
                        .position(|arg| matches!(arg.as_str(), "<" | ">" | "|"))
                    {
                        args.truncate(index);
                    }
                    args.push("--help".into());
                    if !matches!(root.clone().try_get_matches_from(args), Err(error) if error.kind() == ErrorKind::DisplayHelp)
                    {
                        failures.push(line.to_owned());
                    }
                }
            }
            for child in command
                .get_subcommands()
                .filter(|child| !child.is_hide_set())
            {
                visit(child, root, failures);
            }
        }
        let root = super::super::command();
        let mut failures = Vec::new();
        visit(&root, &root, &mut failures);
        assert!(
            failures.is_empty(),
            "invalid examples:\n{}",
            failures.join("\n")
        );
    }

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
