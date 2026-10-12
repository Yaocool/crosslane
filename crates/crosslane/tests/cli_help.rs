//! Process-level tests for the public CLI navigation contract.

use std::path::Path;
use std::process::{Command, Output};

fn run(current_dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_crosslane"))
        .args(args)
        .current_dir(current_dir)
        .env_remove("CROSSLANE_ADMIN_SOCKET")
        .env_remove("CROSSLANE_WRAPPER_LOCAL_ONLY")
        .env_remove("CROSSLANE_PROVIDER")
        .env("RUST_BACKTRACE", "0")
        .output()
        .expect("run crosslane")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("UTF-8 stdout")
}

#[test]
fn help_distinguishes_ids_types_and_every_leaf_scope() {
    let workspace = tempfile::tempdir().unwrap();
    for (args, scope) in [
        (&["account", "add", "--help"][..], "Kiro / GitHub Copilot"),
        (&["account", "overage", "enable", "--help"][..], "仅 Kiro"),
        (
            &["account", "login-code", "--help"][..],
            "仅 GitHub Copilot",
        ),
        (&["service", "apikey", "--help"][..], "按资源 ID/名称定位"),
        (&["model", "disable", "--help"][..], "全局"),
    ] {
        let output = run(workspace.path(), args);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        let help = stdout(&output);
        assert!(help.contains(&format!("适用范围：{scope}")), "{help}");
        assert!(!help.contains("--provider github-copilot"), "{help}");
    }
    let account = stdout(&run(workspace.path(), &["account", "add", "--help"]));
    assert!(account.contains("-p, --provider <ID>"));
    assert!(!account.contains("--provider-kind"), "{account}");
    assert!(!account.contains("--kind"), "{account}");
    let key = stdout(&run(workspace.path(), &["service", "apikey", "--help"]));
    assert!(key.contains("默认隐藏密钥明文"));
    assert!(!key.contains("--provider <ID>"));
    assert!(key.contains("--show-secret"));
    let tasks = stdout(&run(workspace.path(), &["tasks", "list", "--help"]));
    assert!(!tasks.contains("--provider <ID>"), "{tasks}");
    assert!(tasks.contains("不接受 --provider"));
    let tree = stdout(&run(workspace.path(), &["help", "--all"]));
    for label in [
        "[仅 Kiro]",
        "[仅 GitHub Copilot]",
        "[全局]",
        "[按资源 ID/名称定位]",
    ] {
        assert!(tree.contains(label), "missing {label}");
    }
}

#[test]
fn custom_provider_guide_uses_instance_id_without_changing_driver_kind() {
    let workspace = tempfile::tempdir().unwrap();
    let output = run(workspace.path(), &["guide", "copilot", "-p", "work-cp"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let guide = stdout(&output);
    assert!(guide.contains("--id work-cp --kind copilot"));
    assert!(guide.contains("account add --provider work-cp"));
    assert!(guide.contains("--default-provider work-cp"));
    assert!(!guide.contains("--provider copilot"));
}

#[test]
fn unsupported_scope_fails_before_loading_runtime_environment() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join(".env"), "BROKEN='unterminated\n").unwrap();
    for args in [
        &["health"][..],
        &["config", "show"][..],
        &["service", "show", "main"][..],
        &["model-map", "delete", "rule", "--yes"][..],
        &["tasks", "list"][..],
    ] {
        let mut scoped = vec!["-p", "work-cp"];
        scoped.extend_from_slice(args);
        let output = run(workspace.path(), &scoped);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{scoped:?}: {}",
            stderr(&output)
        );
        assert!(stderr(&output).contains("不接受 --provider"));
        assert!(!stderr(&output).contains("unterminated"));
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("UTF-8 stderr")
}

fn bash_case_options<'a>(script: &'a str, command: &str) -> &'a str {
    let marker = format!("{command})");
    let mut lines = script
        .lines()
        .skip_while(|line| line.trim() != marker)
        .skip(1);
    lines
        .find_map(|line| line.trim().strip_prefix("opts=\"")?.strip_suffix('"'))
        .unwrap_or_else(|| panic!("missing Bash completion options for {command}"))
}

#[test]
fn bare_command_groups_show_help_without_loading_the_environment() {
    let workspace = tempfile::tempdir().expect("tempdir");
    std::fs::write(workspace.path().join(".env"), "BROKEN='unterminated\n")
        .expect("write malformed .env");

    for group in [
        "provider",
        "config",
        "account",
        "diagnose",
        "tasks",
        "logs",
        "apikey",
        "service",
        "alert",
        "models",
        "model-map",
    ] {
        let output = run(workspace.path(), &[group]);
        assert!(output.status.success(), "{group}: {}", stderr(&output));
        let text = stdout(&output);
        assert!(text.contains("Usage: crosslane"), "{group}: {text}");
        assert!(text.contains("Commands:"), "{group}: {text}");
    }

    for args in [
        &["help", "--all"][..],
        &["guide", "balance"][..],
        &["completions", "zsh"][..],
        &["version"][..],
    ] {
        let output = run(workspace.path(), args);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        assert!(!stdout(&output).is_empty(), "{args:?}");
    }

    let business = run(workspace.path(), &["status"]);
    assert!(!business.status.success());
    assert!(stderr(&business).contains("unterminated"));
}

#[test]
fn json_requires_an_explicit_group_action() {
    let workspace = tempfile::tempdir().expect("tempdir");
    for group in [
        "provider",
        "config",
        "account",
        "diagnose",
        "tasks",
        "logs",
        "apikey",
        "service",
        "alert",
        "models",
        "model-map",
    ] {
        let output = run(workspace.path(), &["--json", group]);
        assert_eq!(output.status.code(), Some(2), "{group}");
        assert!(stdout(&output).is_empty(), "{group}");
        assert!(stderr(&output).contains("需要明确指定子命令"), "{group}");
    }

    for (group, action) in [
        ("logs", "crosslane logs show"),
        ("models", "crosslane models list"),
        ("tasks", "crosslane tasks list"),
        ("diagnose", "crosslane diagnose all"),
    ] {
        let output = run(workspace.path(), &["--json", group]);
        assert!(stderr(&output).contains(action), "{group}");
    }
}

#[test]
fn nested_help_uses_the_same_clap_command_definition() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let bare_root = run(workspace.path(), &[]);
    let flag_root = run(workspace.path(), &["--help"]);
    let routed_root = run(workspace.path(), &["help"]);
    assert!(bare_root.status.success());
    assert_eq!(stdout(&bare_root), stdout(&flag_root));
    assert_eq!(stdout(&bare_root), stdout(&routed_root));

    let bare_group = run(workspace.path(), &["logs"]);
    let flag_group = run(workspace.path(), &["logs", "--help"]);
    let routed_group = run(workspace.path(), &["help", "logs"]);
    assert!(bare_group.status.success());
    assert_eq!(stdout(&bare_group), stdout(&flag_group));
    assert_eq!(stdout(&bare_group), stdout(&routed_group));
    let json_help = run(workspace.path(), &["logs", "--json", "--help"]);
    assert!(json_help.status.success());
    assert_eq!(stdout(&bare_group), stdout(&json_help));

    let flag_help = run(workspace.path(), &["logs", "trace", "--help"]);
    let routed_help = run(workspace.path(), &["help", "logs", "trace"]);
    assert!(flag_help.status.success());
    assert!(routed_help.status.success());
    assert_eq!(stdout(&flag_help), stdout(&routed_help));
    assert!(stdout(&routed_help).contains("Usage: crosslane logs trace"));
}

#[test]
fn every_command_supports_consistent_help_without_runtime_setup() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join(".env"), "BROKEN='unterminated\n").unwrap();
    let tree = stdout(&run(workspace.path(), &["help", "--all"]));
    let paths = tree
        .lines()
        .filter(|line| line.starts_with("  ") && line.contains(" ["))
        .map(|line| {
            line.split(" [")
                .next()
                .unwrap()
                .split('（')
                .next()
                .unwrap()
                .split_whitespace()
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    assert!(paths.len() > 100, "incomplete tree: {tree}");
    for path in &paths {
        let mut direct_args = path.clone();
        direct_args.push("--help");
        let direct = run(workspace.path(), &direct_args);
        assert!(
            direct.status.success(),
            "{direct_args:?}: {}",
            stderr(&direct)
        );
        let mut short_args = path.clone();
        short_args.push("-h");
        let short = run(workspace.path(), &short_args);
        assert!(short.status.success(), "{short_args:?}: {}", stderr(&short));
        assert_eq!(stdout(&direct), stdout(&short), "{path:?}");
        let mut routed_args = vec!["help"];
        routed_args.extend_from_slice(path);
        let routed = run(workspace.path(), &routed_args);
        assert!(
            routed.status.success(),
            "{routed_args:?}: {}",
            stderr(&routed)
        );
        assert_eq!(stdout(&direct), stdout(&routed), "{path:?}");
        if path.len() > 1 {
            let mut nested_args = path[..path.len() - 1].to_vec();
            nested_args.extend(["help", path[path.len() - 1]]);
            let nested = run(workspace.path(), &nested_args);
            assert!(
                nested.status.success(),
                "{nested_args:?}: {}",
                stderr(&nested)
            );
            assert_eq!(stdout(&direct), stdout(&nested), "{path:?}");
        }
    }
    let subtree = run(workspace.path(), &["help", "account", "--all"]);
    assert!(subtree.status.success(), "{}", stderr(&subtree));
    let subtree = stdout(&subtree);
    assert!(subtree.contains("account overage enable"));
    assert!(!subtree.contains("service create"));
    assert!(!subtree.contains("account help"));
    let alias = run(workspace.path(), &["help", "model", "--all"]);
    assert!(alias.status.success(), "{}", stderr(&alias));
    assert!(stdout(&alias).contains("models resolve"));
}

#[test]
fn alert_reference_commands_ignore_the_provider_environment() {
    let workspace = tempfile::tempdir().unwrap();
    for action in ["config", "events", "platforms"] {
        let output = Command::new(env!("CARGO_BIN_EXE_crosslane"))
            .args(["alert", action])
            .current_dir(workspace.path())
            .env("CROSSLANE_PROVIDER", "work-cp")
            .output()
            .unwrap();
        assert!(output.status.success(), "{action}: {}", stderr(&output));
    }
    let output = run(workspace.path(), &["alert", "list", "--provider", "kiro"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("不接受 --provider"));
}

#[test]
fn full_tree_and_completions_expose_public_commands_only() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let tree = run(workspace.path(), &["help", "--all"]);
    assert!(tree.status.success());
    let tree = stdout(&tree);
    for command in [
        "account services",
        "logs trace",
        "models list",
        "tasks list",
        "diagnose all",
    ] {
        assert!(tree.contains(command), "missing {command}: {tree}");
    }
    assert!(!tree.contains("add-sso-batch"));
    assert!(!tree.contains("CROSSLANE_WRAPPER_LOCAL_ONLY"));

    for shell in ["bash", "zsh", "fish"] {
        let completion = run(workspace.path(), &["completions", shell]);
        assert!(
            completion.status.success(),
            "{shell}: {}",
            stderr(&completion)
        );
        let completion = stdout(&completion);
        assert!(!completion.is_empty(), "{shell}");
        assert!(completion.contains("balance"), "{shell}");
        assert!(
            !completion.contains("CROSSLANE_WRAPPER_LOCAL_ONLY"),
            "{shell}"
        );
        assert!(!completion.contains("add-sso-batch"), "{shell}");
        if shell == "bash" {
            let log_options = bash_case_options(&completion, "crosslane__logs");
            for removed in ["-f", "--tail", "--level", "--account", "--follow"] {
                assert!(
                    !log_options
                        .split_whitespace()
                        .any(|option| option == removed),
                    "removed option {removed} leaked into logs completion: {log_options}"
                );
            }
            let model_options = bash_case_options(&completion, "crosslane__models");
            for removed in ["--mapped", "--refresh"] {
                assert!(
                    !model_options
                        .split_whitespace()
                        .any(|option| option == removed),
                    "removed option {removed} leaked into models completion: {model_options}"
                );
            }
            for command in ["crosslane__alert__add", "crosslane__alert__edit"] {
                let alert_options = bash_case_options(&completion, command);
                for removed in ["--kind", "--url"] {
                    assert!(
                        !alert_options
                            .split_whitespace()
                            .any(|option| option == removed),
                        "removed option {removed} leaked into {command} completion: {alert_options}"
                    );
                }
                for current in ["--platform", "--webhook-url"] {
                    assert!(
                        alert_options
                            .split_whitespace()
                            .any(|option| option == current),
                        "missing current option {current} in {command} completion: {alert_options}"
                    );
                }
            }
        }
    }
}

#[test]
fn guide_topics_are_validated_as_cli_values() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let output = run(workspace.path(), &["guide", "missing"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("possible values"));
}

#[test]
fn provider_guidance_and_config_catalog_show_actual_scopes_offline() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let guide = run(workspace.path(), &["guide", "provider"]);
    assert!(guide.status.success(), "{}", stderr(&guide));
    let guide = stdout(&guide);
    for expected in ["Kiro 专项", "Copilot 专项", "全局", "guide config"] {
        assert!(guide.contains(expected), "missing {expected}: {guide}");
    }

    let catalog = run(workspace.path(), &["--json", "config", "list"]);
    assert!(catalog.status.success(), "{}", stderr(&catalog));
    let catalog: serde_json::Value = serde_json::from_str(&stdout(&catalog)).unwrap();
    let scope = |name: &str| {
        catalog
            .as_array()
            .unwrap()
            .iter()
            .find(|module| module["name"] == name)
            .unwrap()["scope"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(scope("sso"), "kiro");
    assert_eq!(scope("provider"), "provider");
    assert_eq!(scope("server"), "global");
    assert_eq!(scope("tasks"), "mixed");

    let alerts = run(workspace.path(), &["--json", "alert", "events"]);
    assert!(alerts.status.success(), "{}", stderr(&alerts));
    let alerts: serde_json::Value = serde_json::from_str(&stdout(&alerts)).unwrap();
    assert!(alerts
        .as_array()
        .unwrap()
        .iter()
        .all(|event| event["provider_scope"] == "kiro"));
}

#[test]
fn diagnose_account_requires_a_target_during_argument_parsing() {
    let workspace = tempfile::tempdir().expect("tempdir");
    std::fs::write(workspace.path().join(".env"), "BROKEN='unterminated\n")
        .expect("write malformed .env");

    let output = run(workspace.path(), &["diagnose", "account"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stdout(&output).is_empty());
    let error = stderr(&output);
    assert!(error.contains("required arguments"), "{error}");
    assert!(error.contains("<ID|--all>"), "{error}");
    assert!(!error.contains("unterminated"), "{error}");
}

#[test]
fn bounded_numeric_arguments_are_validated_before_any_runtime_setup() {
    let workspace = tempfile::tempdir().expect("tempdir");
    std::fs::write(workspace.path().join(".env"), "BROKEN='unterminated\n")
        .expect("write malformed .env");

    for args in [
        &["diagnose", "all", "--timeout", "0s"][..],
        &["diagnose", "all", "--timeout", "301s"][..],
        &["diagnose", "all", "--concurrency", "0"][..],
        &["diagnose", "account", "--all", "--concurrency", "9"][..],
        &["logs", "show", "--tail", "0"][..],
        &["logs", "follow", "--tail", "1001"][..],
        &[
            "logs",
            "trace",
            "trace_0123456789abcdef0123456789abcdef",
            "--tail",
            "0",
        ][..],
        &[
            "logs",
            "trace",
            "trace_0123456789abcdef0123456789abcdef",
            "--tail",
            "1001",
        ][..],
        &[
            "model-map",
            "add",
            "--name",
            "x",
            "--source",
            "a",
            "--target",
            "b",
            "--below-credits-percent",
            "101",
        ][..],
        &[
            "model-map",
            "test",
            "a",
            "--remaining-credits-percent",
            "NaN",
        ][..],
        &["apikey", "add", "--name", "x", "--credits-limit", "-1"][..],
        &["apikey", "limit", "x", "--credits", "inf"][..],
        &["apikey", "history", "x", "--tail", "0"][..],
        &["alert", "logs", "--tail", "1001"][..],
        &["service", "create", "--name", "x", "--port", "1023"][..],
        &["service", "edit", "x", "--port", "0"][..],
    ] {
        let output = run(workspace.path(), args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stdout(&output).is_empty(), "{args:?}");
        assert!(!stderr(&output).contains("unterminated"), "{args:?}");
    }
}

#[test]
fn required_targets_and_sources_are_validated_before_runtime_setup() {
    let workspace = tempfile::tempdir().expect("tempdir");
    std::fs::write(workspace.path().join(".env"), "BROKEN='unterminated\n")
        .expect("write malformed .env");

    for args in [
        &["account", "import"][..],
        &["account", "add-sso"][..],
        &["account", "add-sso", "--email", "alice@example.com"][..],
        &["account", "add-sso", "--password-stdin"][..],
        &[
            "account",
            "add-sso",
            "--batch",
            "accounts.csv",
            "--concurrency",
            "9",
        ][..],
        &["account", "tag", "acc_00000001"][..],
        &["account", "refresh"][..],
        &["account", "probe"][..],
        &["account", "reset-health"][..],
        &["alert", "test"][..],
    ] {
        let output = run(workspace.path(), args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stdout(&output).is_empty(), "{args:?}");
        let error = stderr(&output);
        assert!(!error.contains("unterminated"), "{args:?}: {error}");
    }
}

#[test]
fn constrained_enums_reject_typos_before_runtime_setup() {
    let workspace = tempfile::tempdir().expect("tempdir");
    std::fs::write(workspace.path().join(".env"), "BROKEN='unterminated\n")
        .expect("write malformed .env");

    for args in [
        &["stats", "--detail", "--by", "api-key"][..],
        &["logs", "show", "--level", "warningg"][..],
        &["apikey", "add", "--name", "x", "--format", "bearer"][..],
        &[
            "service",
            "create",
            "--name",
            "x",
            "--api-key-format",
            "bearer",
        ][..],
        &[
            "model-map",
            "add",
            "--name",
            "x",
            "--kind",
            "random",
            "--source",
            "a",
            "--target",
            "b",
        ][..],
    ] {
        let output = run(workspace.path(), args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stdout(&output).is_empty(), "{args:?}");
        let error = stderr(&output);
        assert!(error.contains("possible values"), "{args:?}: {error}");
        assert!(!error.contains("unterminated"), "{args:?}: {error}");
    }
}

#[test]
fn destructive_commands_expose_a_non_interactive_yes_flag() {
    let workspace = tempfile::tempdir().expect("tempdir");
    for args in [
        &["provider", "delete", "--help"][..],
        &["config", "reset", "--help"][..],
        &["account", "rm", "--help"][..],
        &["apikey", "rm", "--help"][..],
        &["apikey", "reset-usage", "--help"][..],
        &["service", "delete", "--help"][..],
        &["alert", "delete", "--help"][..],
        &["model-map", "delete", "--help"][..],
    ] {
        let output = run(workspace.path(), args);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        let help = stdout(&output);
        assert!(help.contains("-y, --yes"), "{args:?}: {help}");
    }
}

#[test]
fn config_validate_honors_json_output() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let config = workspace.path().join("config.toml");
    std::fs::write(&config, "").expect("write config");
    let output = run(
        workspace.path(),
        &[
            "--json",
            "config",
            "validate",
            config.to_str().expect("UTF-8 path"),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON output");
    assert_eq!(value["valid"], true);
    assert_eq!(value["config_file"], config.to_string_lossy().as_ref());

    let missing = workspace.path().join("missing.toml");
    let output = run(
        workspace.path(),
        &["config", "validate", missing.to_str().expect("UTF-8 path")],
    );
    assert!(!output.status.success());
    assert!(stderr(&output).contains("配置文件不存在或无法访问"));
}

#[test]
fn offline_catalogs_and_explicit_validation_ignore_broken_runtime_environment() {
    let workspace = tempfile::tempdir().expect("tempdir");
    std::fs::write(workspace.path().join(".env"), "BROKEN='unterminated\n")
        .expect("write malformed .env");
    let config = workspace.path().join("candidate.toml");
    std::fs::write(&config, "").expect("write config");

    for args in [
        vec!["config", "list"],
        vec!["config", "validate", config.to_str().expect("UTF-8 path")],
        vec!["alert", "config"],
        vec!["alert", "events"],
        vec!["alert", "platforms"],
    ] {
        let output = run(workspace.path(), &args);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        assert!(!stdout(&output).is_empty(), "{args:?}");
    }

    let runtime_command = run(workspace.path(), &["config", "show"]);
    assert!(!runtime_command.status.success());
    assert!(stderr(&runtime_command).contains("unterminated"));
}

#[test]
fn invalid_nested_commands_fail_before_runtime_setup() {
    let workspace = tempfile::tempdir().expect("tempdir");
    std::fs::write(workspace.path().join(".env"), "BROKEN='unterminated\n")
        .expect("write malformed .env");

    for args in [
        &["logs", "missing"][..],
        &["logs", "trace"][..],
        &["help", "logs", "missing"][..],
        &["logs", "--tail", "10", "files"][..],
    ] {
        let output = run(workspace.path(), args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stdout(&output).is_empty(), "{args:?}");
        let error = stderr(&output);
        assert!(!error.contains("unterminated"), "{args:?}: {error}");
    }
}

#[test]
fn wrapper_local_mode_rejects_business_commands() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let output = Command::new(env!("CARGO_BIN_EXE_crosslane"))
        .args(["logs", "show"])
        .current_dir(workspace.path())
        .env("CROSSLANE_WRAPPER_LOCAL_ONLY", "1")
        .output()
        .expect("run local-only crosslane");
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("crosslaned 未运行"));

    let help = Command::new(env!("CARGO_BIN_EXE_crosslane"))
        .arg("logs")
        .current_dir(workspace.path())
        .env("CROSSLANE_WRAPPER_LOCAL_ONLY", "1")
        .output()
        .expect("run local-only help");
    assert!(help.status.success(), "{}", stderr(&help));
    assert!(stdout(&help).contains("logs show"));

    for args in [
        &["config", "list"][..],
        &["alert", "config"][..],
        &["alert", "events"][..],
        &["alert", "platforms"][..],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_crosslane"))
            .args(args)
            .current_dir(workspace.path())
            .env("CROSSLANE_WRAPPER_LOCAL_ONLY", "1")
            .output()
            .expect("run offline command");
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
    }
}

#[test]
fn removed_command_forms_are_rejected_before_runtime_setup() {
    let workspace = tempfile::tempdir().expect("tempdir");
    std::fs::write(workspace.path().join(".env"), "BROKEN='unterminated\n")
        .expect("write malformed .env");

    for args in [
        &["logs", "--tail", "25"][..],
        &["logs", "--level", "error"][..],
        &["logs", "--account", "alice@example.com"][..],
        &["logs", "-f"][..],
        &["logs", "--follow"][..],
        &["models", "--mapped"][..],
        &["models", "--refresh"][..],
        &["account", "add-sso-batch", "--file", "accounts.csv"][..],
        &[
            "alert",
            "add",
            "--name",
            "ops",
            "--kind",
            "dingtalk",
            "--webhook-url",
            "https://example.com/hook",
            "--event",
            "token-refresh-failed",
        ][..],
        &[
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
        ][..],
        &[
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
        ][..],
        &["alert", "edit", "ops", "--kind", "dingtalk"][..],
        &["alert", "edit", "ops", "--url", "https://example.com/hook"][..],
        &["alert", "edit", "ops", "--platform", "wechat"][..],
        &["help", "balance"][..],
    ] {
        let output = run(workspace.path(), args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stdout(&output).is_empty(), "{args:?}");
        let error = stderr(&output);
        assert!(!error.contains("unterminated"), "{args:?}: {error}");
    }
}
