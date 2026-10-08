//! CLI navigation, guides, and completion generation.

pub mod completions;
pub mod guide;
pub mod help;
pub(crate) mod scope;

use anyhow::Result;
use clap::{CommandFactory, FromArgMatches};
use std::ffi::OsString;

use crate::{Cli, Command, ConfigCommand};

/// Parse one CLI invocation and preserve Clap's stdout/stderr and exit-code contract.
pub fn parse_or_exit(args: impl IntoIterator<Item = OsString>) -> Cli {
    parse(args, true)
}

/// Keep navigation independent of `.env`, but defer required provider defaults
/// until runtime parsing has loaded the environment.
pub fn parse_navigation_or_exit(args: impl IntoIterator<Item = OsString>) -> Cli {
    parse(args, false)
}

fn parse(args: impl IntoIterator<Item = OsString>, require_provider: bool) -> Cli {
    let matches = command()
        .try_get_matches_from(args)
        .unwrap_or_else(|error| error.exit());
    let mut cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
    let path = scope::selected_path(&matches);
    let selected_scope = scope::for_path(&path);
    let explicit_provider =
        matches.value_source("provider") == Some(clap::parser::ValueSource::CommandLine);
    let global_policy = matches!(path.as_slice(), ["models", "disable" | "enable"]);
    if matches!(
        selected_scope,
        scope::Scope::Global | scope::Scope::Resource
    ) && !global_policy
    {
        if explicit_provider {
            clap::Error::raw(clap::error::ErrorKind::ArgumentConflict, format!(
                "`{}` 的适用范围为{}，不接受 --provider；请移除该参数。可用 `crosslane help {}` 查看操作范围。",
                path.join(" "), selected_scope.label(), path.join(" ")
            )).exit();
        }
        // A shell default only applies to commands that select a provider.
        cli.provider.clear();
    }
    if !explicit_provider {
        // An edit addresses an existing resource. A shell query default must
        // never become an implicit replacement of that resource's permissions.
        let edited_providers = match &mut cli.command {
            Some(Command::Service {
                command: Some(crate::commands::runtime::ServiceCommand::Edit { providers, .. }),
            })
            | Some(Command::ApiKey {
                command: Some(crate::commands::runtime::ApiKeyCommand::Edit { providers, .. }),
            })
            | Some(Command::ModelMap {
                command: Some(crate::ModelMapCommand::Edit { providers, .. }),
            }) => Some(providers),
            _ => None,
        };
        if let Some(providers) = edited_providers {
            providers.clear();
            cli.provider.clear();
        }
    }
    if let Some(Command::Tasks {
        command: Some(crate::TaskCommand::Run { name, provider }),
    }) = &mut cli.command
    {
        if name != "model_cache_refresh" && provider.is_some() {
            let kiro_task = scope::task_scope(name) == "仅 Kiro";
            // Check every supplied ID before discarding this Kiro-only selector.
            if (kiro_task
                && (cli.provider.len() != 1 || provider.as_deref().is_some_and(|id| id != "kiro")))
                || (!kiro_task && explicit_provider)
            {
                clap::Error::raw(clap::error::ErrorKind::ArgumentConflict,
                    "model_cache_refresh 可选择来源；Kiro 任务只接受 --provider kiro，其他任务不接受 --provider").exit();
            }
            *provider = None;
            cli.provider.clear();
        }
    }
    if matches!(path.as_slice(), ["tasks", "list"]) {
        if explicit_provider {
            clap::Error::raw(
                clap::error::ErrorKind::ArgumentConflict,
                "tasks list 展示全部任务及各自适用范围，不接受 --provider",
            )
            .exit();
        }
        cli.provider.clear();
    }
    if let Err(error) = validate_scope(&cli, require_provider || wrapper_local_only()) {
        clap::Error::raw(clap::error::ErrorKind::ValueValidation, error).exit();
    }
    cli
}

pub fn command() -> clap::Command {
    let mut command = Cli::command();
    scope::decorate(&mut command, &mut Vec::new());
    command
}

fn validate_scope(cli: &Cli, require_provider: bool) -> Result<(), String> {
    use crate::commands::account::AccountCommand;
    use crate::commands::runtime::{ApiKeyCommand, ServiceCommand};
    if matches!(
        &cli.command,
        Some(Command::Service {
            command: Some(ServiceCommand::Edit { providers, clear_providers: true, .. }),
        }) | Some(Command::ApiKey {
            command: Some(ApiKeyCommand::Edit { providers, clear_providers: true, .. }),
        }) | Some(Command::ModelMap {
            command: Some(crate::ModelMapCommand::Edit { providers, clear_providers: true, .. }),
        }) if !providers.is_empty()
    ) {
        return Err("--provider 与 --clear-providers 不能同时使用".into());
    }
    for provider in cli
        .provider
        .iter()
        .filter(|id| !id.is_empty() && *id != "all")
    {
        crosslane_core::provider::ProviderId::parse(provider.as_str()).map_err(|error| format!(
            "{error}；--provider 需要实例 ID，不能使用 GitHub Copilot 等类型名称。请运行 crosslane provider list"
        ))?;
    }
    if matches!(
        &cli.command,
        Some(Command::Models {
            command: Some(
                crate::ModelsCommand::Disable { .. } | crate::ModelsCommand::Enable { .. }
            )
        })
    ) && cli.provider.iter().any(|provider| provider != "all")
    {
        return Err("模型禁用/启用是全局操作，作用于全部 provider；请移除 --provider 或 CROSSLANE_PROVIDER，或指定 --provider all".into());
    }
    if require_provider
        && matches!(&cli.command, Some(Command::Account {
        command: Some(AccountCommand::Add { provider, .. } | AccountCommand::LoginCode { provider, .. }),
    }) if provider.is_empty())
    {
        return Err("该命令需要 --provider <ID>（用 crosslane provider list 查看），也可通过 CROSSLANE_PROVIDER 设置".into());
    }
    let multiple = matches!(
        &cli.command,
        Some(Command::Service {
            command: Some(ServiceCommand::Create { .. } | ServiceCommand::Edit { .. })
        }) | Some(Command::ApiKey {
            command: Some(ApiKeyCommand::Add { .. } | ApiKeyCommand::Edit { .. })
        }) | Some(Command::ModelMap {
            command: Some(crate::ModelMapCommand::Add { .. } | crate::ModelMapCommand::Edit { .. })
        })
    );
    if multiple && cli.provider.iter().any(|id| id == "all") {
        return Err("来源权限配置需要具体实例 ID，不能使用 all；请用 --provider ID1,ID2 指定范围，运行 crosslane provider list 查看实例".into());
    }
    if cli.provider.len() > 1 && !multiple {
        return Err("该命令只接受一个 provider；聚合查询请使用 --provider all".into());
    }
    let Some(provider) = cli
        .provider
        .first()
        .map(String::as_str)
        .filter(|id| *id != "kiro")
    else {
        return Ok(());
    };
    let kiro_only = matches!(
        &cli.command,
        Some(Command::Diagnose { .. })
            | Some(Command::Alert { .. })
            | Some(Command::Account {
                command: Some(
                    AccountCommand::Overage { .. }
                        | AccountCommand::AddSso { .. }
                        | AccountCommand::AddApiKey { .. }
                        | AccountCommand::Import { .. }
                        | AccountCommand::RegenMachineId { .. }
                )
            })
    );
    if kiro_only {
        return Err(format!("该命令仅适用于 Kiro，不能选择提供源 {provider}；统一认证入口为 account add，账号诊断使用 account probe"));
    }
    Ok(())
}

/// Handle commands that do not need a running daemon.
pub async fn handle_local(cli: &Cli) -> Result<bool> {
    match &cli.command {
        None => {
            help::print_command_help(&[])?;
            Ok(true)
        }
        Some(Command::Help { all, path }) => {
            if *all {
                help::print_all_commands();
            } else {
                help::print_command_help(path)?;
            }
            Ok(true)
        }
        Some(Command::Guide { topic }) => {
            guide::print(
                topic.map(|topic| topic.as_str()),
                cli.provider.first().map(String::as_str),
            )?;
            Ok(true)
        }
        Some(Command::Completions { shell }) => {
            completions::print(*shell)?;
            Ok(true)
        }
        Some(Command::Version) => {
            print_version(cli.json)?;
            Ok(true)
        }
        Some(Command::Config {
            command: Some(ConfigCommand::List),
        }) => {
            crate::commands::runtime::list_config_modules(cli.json)?;
            Ok(true)
        }
        Some(Command::Config {
            command: Some(ConfigCommand::Validate { file }),
        }) => {
            if file.is_none() {
                crosslane_store::environment::load_dotenv()?;
            }
            crate::commands::runtime::validate_config(file.as_deref(), cli.json).await?;
            Ok(true)
        }
        Some(Command::Alert {
            command: Some(crate::commands::runtime::AlertCommand::Config),
        }) => {
            crate::commands::runtime::show_alert_config(cli.json)?;
            Ok(true)
        }
        Some(Command::Alert {
            command: Some(crate::commands::runtime::AlertCommand::Events),
        }) => {
            crate::commands::runtime::show_alert_events(cli.json)?;
            Ok(true)
        }
        Some(Command::Alert {
            command: Some(crate::commands::runtime::AlertCommand::Platforms),
        }) => {
            crate::commands::runtime::show_alert_platforms(cli.json)?;
            Ok(true)
        }
        Some(command) => {
            if let Some(path) = help::empty_group_path(command) {
                if cli.json {
                    help::exit_missing_action(path);
                }
                help::print_command_help(&[path.to_owned()])?;
                return Ok(true);
            }
            if wrapper_local_only() {
                eprintln!(
                    "crosslane: crosslaned 未运行；当前可使用 help、guide、completions、version、config list/validate、alert config/events/platforms 或无参命令组帮助"
                );
                std::process::exit(1);
            }
            Ok(false)
        }
    }
}

fn wrapper_local_only() -> bool {
    std::env::var_os("CROSSLANE_WRAPPER_LOCAL_ONLY").is_some_and(|value| value == "1")
}

fn print_version(json: bool) -> Result<()> {
    let value = serde_json::json!({
        "version":env!("CARGO_PKG_VERSION"),
        "rust":env!("CARGO_PKG_RUST_VERSION"),
        "codewhisperer":crosslane_kiro::endpoint::CODEWHISPERER_URL,
        "amazonq":crosslane_kiro::endpoint::AMAZONQ_URL
    });
    if json {
        crate::output::print_json(&value)
    } else {
        println!(
            "crosslane {} (Rust {})",
            env!("CARGO_PKG_VERSION"),
            env!("CARGO_PKG_RUST_VERSION")
        );
        println!(
            "CodeWhisperer {}",
            crosslane_kiro::endpoint::CODEWHISPERER_URL
        );
        println!("AmazonQ        {}", crosslane_kiro::endpoint::AMAZONQ_URL);
        Ok(())
    }
}
