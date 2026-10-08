//! Verify that the new explicit actions preserve their intended RPC contracts.
#![cfg(unix)]

use std::time::Duration;
use std::{fs, path::Path};

use crosslane_ipc::protocol::{decode_line, encode_line, method, Request, Response};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::process::Command;

async fn run(args: &[&str], results: Vec<Value>) -> (std::process::Output, Vec<Request>) {
    let workspace = tempfile::tempdir().expect("tempdir");
    run_in_workspace(workspace.path(), args, results).await
}

#[tokio::test]
async fn short_provider_selector_reaches_rpc_at_every_depth_and_overrides_environment() {
    for args in [
        &[
            "-p", "work-cp", "account", "tag", "acc_test", "--add", "prod",
        ][..],
        &[
            "account", "-p", "work-cp", "tag", "acc_test", "--add", "prod",
        ][..],
        &[
            "account", "tag", "acc_test", "--add", "prod", "-p", "work-cp",
        ][..],
        &[
            "-p", "kiro", "account", "tag", "acc_test", "--add", "prod", "-p", "work-cp",
        ][..],
    ] {
        let workspace = tempfile::tempdir().unwrap();
        let (output, requests) = run_in_workspace_scoped(
            workspace.path(),
            args,
            vec![json!({"id":"acc_test","tags":["prod"],"changed":true})],
            true,
            Some("wrong-id"),
        )
        .await;
        assert_success(&output);
        assert_eq!(requests[0].method, method::V2_ACCOUNT_TAG);
        assert_eq!(requests[0].params["provider"], "work-cp");
    }
}

#[tokio::test]
async fn service_apikey_defaults_to_redacted_and_reveals_only_when_requested() {
    for json_output in [true, false] {
        for (args, reveal) in [
            (&["service", "apikey", "main"][..], false),
            (&["service", "apikeys", "main"][..], false),
            (&["service", "apikey", "main", "--show-secret"][..], true),
            (&["service", "apikey", "main", "--show-secrets"][..], true),
            (&["service", "apikey", "main", "--redact"][..], false),
        ] {
            let workspace = tempfile::tempdir().unwrap();
            let result = json!({"service_id":"svc_test","service_name":"main","api_keys":[{
                "id":"ak_test","name":"test","format":"sk","enabled":true,
                "key":if reveal {Some("sk-local-fixture-only")} else {None}
            }]});
            let (output, requests) = run_in_workspace_scoped(
                workspace.path(),
                args,
                vec![result],
                json_output,
                Some("work-cp"),
            )
            .await;
            assert_success(&output);
            assert_eq!(requests[0].method, method::SERVICE_APIKEYS);
            assert_eq!(
                requests[0].params,
                json!({"service":"main","show_secret":reveal})
            );
            let text = String::from_utf8(output.stdout).unwrap();
            assert_eq!(
                text.contains("sk-local-fixture-only"),
                reveal,
                "{args:?}: {text}"
            );
            assert!(!text.contains("--show-secret"));
        }
    }
}

#[tokio::test]
async fn account_export_redacts_by_default_and_requires_an_explicit_secret_flag() {
    for (args, reveal) in [
        (&["account", "export"][..], false),
        (&["account", "export", "--provider", "all"][..], false),
        (&["account", "export", "--redact"][..], false),
        (&["account", "export", "--include-secrets"][..], true),
    ] {
        let (output, requests) = run(args, vec![json!({"accounts":[]})]).await;
        assert_success(&output);
        assert_eq!(requests[0].params["redact"], !reveal, "{args:?}");
    }
}

#[tokio::test]
async fn config_show_redacts_raw_and_effective_config_by_default() {
    let fixture = json!({
        "path":"/tmp/config.toml",
        "raw":"[[api_key]]\nkey = \"sk-fixture-secret\"\n",
        "effective_json":{"api_key":[{"key":"sk-fixture-secret"}]}
    });
    for (args, reveal) in [
        (&["config", "show"][..], false),
        (&["config", "show", "--effective"][..], false),
        (&["config", "show", "--show-secrets"][..], true),
    ] {
        let (output, requests) = run(args, vec![fixture.clone()]).await;
        assert_success(&output);
        assert_eq!(requests[0].method, method::CONFIG_SHOW);
        assert_eq!(
            String::from_utf8(output.stdout)
                .unwrap()
                .contains("sk-fixture-secret"),
            reveal,
            "{args:?}"
        );
    }
}

#[tokio::test]
async fn effective_config_works_when_the_on_disk_toml_is_invalid() {
    let fixture = json!({
        "path":"/tmp/config.toml",
        "raw":"invalid = [ # unclosed and secret=do-not-print",
        "effective_json":{"api_key":[{"key":"sk-fixture-secret"}],"server":{"port":5580}}
    });
    for json_output in [true, false] {
        let workspace = tempfile::tempdir().unwrap();
        let (output, _) = run_in_workspace_mode(
            workspace.path(),
            &["config", "show", "--effective"],
            vec![fixture.clone()],
            json_output,
        )
        .await;
        assert_success(&output);
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(!text.contains("do-not-print"), "{text}");
        assert!(!text.contains("sk-fixture-secret"), "{text}");
        if json_output {
            let value: Value = serde_json::from_str(&text).unwrap();
            assert!(value.get("raw").is_none());
            assert_eq!(value["effective_json"]["server"]["port"], 5580);
        }
    }
}

#[tokio::test]
async fn service_create_reports_a_disabled_listener_accurately() {
    let result = json!({
        "service":{
            "id":"svc_fixture","name":"fixture","host":"127.0.0.1",
            "port":5580,"enabled":true,"running":false,"created_at":0
        },
        "api_key":{"id":"ak_fixture","name":"fixture-default","key":"sk-fixture"}
    });
    let (output, _) = run_human(&["service", "create", "--name", "fixture"], vec![result]).await;
    assert_success(&output);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("业务监听未启动"), "{text}");
    assert!(!text.contains("已创建并启动"), "{text}");
    assert!(!text.contains("，监听 "), "{text}");
}

#[tokio::test]
async fn watched_json_snapshots_are_one_value_per_line() {
    let status = json!({
        "version":"test","pid":1,"uptime_secs":1,"listen":"127.0.0.1:5580",
        "admin_socket":"/tmp/admin.sock","account_total":0,"account_enabled":0,
        "config_path":"/tmp/config.toml"
    });
    for (args, snapshot, expected_method) in [
        (&["status", "--watch"][..], status, method::STATUS),
        (
            &["pool", "--provider", "all", "--watch"][..],
            json!({"accounts":[],"complete":true}),
            method::V2_ACCOUNT_LIST,
        ),
    ] {
        let workspace = tempfile::tempdir().unwrap();
        let socket = workspace.path().join("admin.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let (read, mut write) = stream.into_split();
                let mut lines = BufReader::new(read).lines();
                let request: Request =
                    decode_line(&lines.next_line().await.unwrap().unwrap()).unwrap();
                assert_eq!(request.method, expected_method);
                write
                    .write_all(
                        encode_line(&Response::ok(request.id, snapshot.clone()))
                            .unwrap()
                            .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });
        let mut child = Command::new(env!("CARGO_BIN_EXE_crosslane"))
            .current_dir(workspace.path())
            .arg("--json")
            .arg("--socket")
            .arg(&socket)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        for _ in 0..2 {
            let line = tokio::time::timeout(Duration::from_secs(6), lines.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(!line.is_empty());
            let value: Value = serde_json::from_str(&line).expect("one compact JSON snapshot");
            assert!(value.is_object());
        }
        child.kill().await.unwrap();
        child.wait().await.unwrap();
        server.await.unwrap();
    }
}

#[tokio::test]
async fn config_edit_json_emits_only_a_result_object() {
    let workspace = tempfile::tempdir().unwrap();
    let config_file = workspace.path().join("config.toml");
    fs::write(&config_file, "[server]\nport = 5580\n").unwrap();
    let socket = workspace.path().join("admin.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server_path = config_file.clone();
    let server = tokio::spawn(async move {
        for result in [
            json!({"config_file":server_path,"accounts_file":"","daily_file":"","stats_file":"","admin_socket":"","log_base_path":"","log_directory":""}),
            json!({"applied":true,"needs_restart":[]}),
        ] {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let request: Request = decode_line(&lines.next_line().await.unwrap().unwrap()).unwrap();
            write
                .write_all(
                    encode_line(&Response::ok(request.id, result))
                        .unwrap()
                        .as_bytes(),
                )
                .await
                .unwrap();
        }
    });
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        Command::new(env!("CARGO_BIN_EXE_crosslane"))
            .current_dir(workspace.path())
            .arg("--json")
            .arg("--socket")
            .arg(&socket)
            .args(["config", "edit"])
            .env("VISUAL", "/bin/echo editor-output")
            .env("EDITOR", "/usr/bin/true")
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_success(&output);
    let value: Value = serde_json::from_slice(&output.stdout).expect("one JSON result");
    assert_eq!(value["saved"], true);
    assert_eq!(value["changed"], false);
    assert_eq!(value["config_file"], config_file.to_string_lossy().as_ref());
    assert!(String::from_utf8_lossy(&output.stderr).contains("editor-output"));
    server.await.unwrap();
}

#[tokio::test]
async fn config_reload_reports_an_unapplied_result_as_failure() {
    for json_output in [true, false] {
        let workspace = tempfile::tempdir().unwrap();
        let config_file = workspace.path().join("config.toml");
        fs::write(&config_file, "[server]\nport = 5580\n").unwrap();
        let (output, requests) = run_in_workspace_mode(
            workspace.path(),
            &["config", "reload"],
            vec![
                json!({"config_file":config_file,"accounts_file":"","daily_file":"","stats_file":"","admin_socket":"","log_base_path":"","log_directory":""}),
                json!({"applied":false,"error":"invalid config","needs_restart":[]}),
            ],
            json_output,
        )
        .await;
        assert!(!output.status.success());
        assert_eq!(requests[0].method, method::CONFIG_PATH);
        assert_eq!(requests[1].method, method::CONFIG_RELOAD);
        assert!(stderr(&output).contains("invalid config"));
        if json_output {
            assert_eq!(
                serde_json::from_slice::<Value>(&output.stdout).unwrap()["applied"],
                false
            );
        }
    }
}

#[tokio::test]
async fn provider_list_scopes_and_next_steps_keep_custom_ids() {
    let provider = json!({"id":"work-cp","kind":"copilot","enabled":true,
        "status":"ready","capabilities":{"protocols":[],"device_flow":true,"token_import":true}});
    let (output, requests) = run_human(
        &["provider", "list", "-p", "work-cp"],
        vec![json!({"providers":[provider.clone()]})],
    )
    .await;
    assert_success(&output);
    assert_eq!(requests[0].method, method::V2_PROVIDER_LIST);
    assert_eq!(requests[0].params["provider"], "work-cp");
    let list = String::from_utf8(output.stdout).unwrap();
    assert!(list.contains("实例 ID (--provider)"));
    assert!(list.contains("GitHub Copilot (copilot)"));
    let (output, _) = run_human(&["provider", "show", "work-cp"], vec![provider]).await;
    assert_success(&output);
    let detail = String::from_utf8(output.stdout).unwrap();
    assert!(detail.contains("account add --provider work-cp --auth device-flow"));
    assert!(detail.contains("model list --provider work-cp"));
    assert!(!detail.contains("--provider copilot"));
    assert!(!detail.contains("--provider github-copilot"));
}

#[tokio::test]
async fn task_scope_is_enforced_and_shown_without_changing_json_contracts() {
    for (task, selected, expected) in [
        ("status_check", "kiro", Value::Null),
        ("stats_persist", "work-cp", Value::Null),
        ("model_cache_refresh", "work-cp", json!("work-cp")),
    ] {
        let workspace = tempfile::tempdir().unwrap();
        let (output, requests) = run_in_workspace_scoped(
            workspace.path(),
            &["tasks", "run", task],
            vec![json!({"ok":true})],
            true,
            Some(selected),
        )
        .await;
        assert_success(&output);
        assert_eq!(requests[0].params["provider"], expected);
    }
    let (output, requests) = run(&["tasks", "run", "status_check", "-p", "work-cp"], vec![]).await;
    assert_eq!(output.status.code(), Some(2));
    assert!(requests.is_empty());
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("Kiro 任务只接受 --provider kiro"));
    let tasks = json!({
        "token_refresh":{"interval_ms":30000,"run":null},
        "model_cache_refresh":{"interval_ms":60000,"run":null},
        "stats_persist":{"interval_ms":10000,"run":null}
    });
    let (output, _) = run_human(&["tasks", "list"], vec![tasks.clone()]).await;
    assert_success(&output);
    let text = String::from_utf8(output.stdout).unwrap();
    for label in ["适用范围", "仅 Kiro", "Kiro / GitHub Copilot", "全局"] {
        assert!(text.contains(label), "{text}");
    }
    let (output, _) = run(&["tasks", "list"], vec![tasks.clone()]).await;
    assert_success(&output);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        tasks
    );
}

#[tokio::test]
async fn editing_resources_does_not_take_permission_changes_from_shell_defaults() {
    let initial = r#"
[[provider]]
id = "kiro"
kind = "kiro"
[[provider]]
id = "copilot"
kind = "copilot"
[[api_key]]
id = "ak_team"
name = "team"
key = "sk-test"
allowed_providers = ["kiro", "copilot"]
[[proxy_service]]
id = "svc_team"
name = "team"
host = "127.0.0.1"
port = 6201
api_key_ids = ["ak_team"]
allowed_providers = ["kiro", "copilot"]
default_provider = "kiro"
[[model_mapping]]
name = "team"
type = "alias"
source_models = ["fast"]
target_models = ["gpt-test"]
providers = ["kiro", "copilot"]
"#;
    for args in [
        &["service", "edit", "team", "--rename", "renamed"][..],
        &["apikey", "edit", "team", "--skip-user-agent-check", "true"][..],
        &["model-map", "edit", "team", "--priority", "10"][..],
    ] {
        let workspace = tempfile::tempdir().unwrap();
        let file = workspace.path().join("config.toml");
        fs::write(&file, initial).unwrap();
        let (output, requests) = run_in_workspace_scoped(
            workspace.path(), args,
            vec![json!({"config_file":file,"accounts_file":"","daily_file":"","stats_file":"","admin_socket":""}),
                 json!({"applied":true,"needs_restart":[]})],
            false, Some("copilot"),
        ).await;
        assert_success(&output);
        assert_eq!(requests.len(), 2);
        let config: Value = serde_json::to_value(
            toml::from_str::<toml::Value>(&fs::read_to_string(&file).unwrap()).unwrap(),
        )
        .unwrap();
        for (section, field) in [
            ("api_key", "allowed_providers"),
            ("proxy_service", "allowed_providers"),
            ("model_mapping", "providers"),
        ] {
            assert_eq!(
                config[section][0][field],
                json!(["kiro", "copilot"]),
                "{args:?}: {section}"
            );
        }
        assert_eq!(config["proxy_service"][0]["default_provider"], "kiro");
    }
}

#[tokio::test]
async fn contradictory_provider_edits_and_multi_provider_tasks_fail_before_rpc() {
    for args in [
        &[
            "service",
            "edit",
            "team",
            "-p",
            "copilot",
            "--clear-providers",
        ][..],
        &[
            "-p",
            "copilot",
            "apikey",
            "edit",
            "team",
            "--clear-providers",
        ][..],
        &[
            "model-map",
            "edit",
            "team",
            "--clear-providers",
            "-p",
            "copilot",
        ][..],
        &["tasks", "run", "status_check", "-p", "copilot,kiro"][..],
    ] {
        let (output, requests) = run(args, vec![]).await;
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            stderr(&output)
        );
        assert!(requests.is_empty());
    }
}

#[tokio::test]
async fn aggregate_pool_reports_partial_discovery_in_text_and_json() {
    for json_output in [false, true] {
        let workspace = tempfile::tempdir().unwrap();
        let (output, _) = run_in_workspace_scoped(
            workspace.path(),
            &["pool", "-p", "all"],
            vec![json!({"accounts":[],"complete":false,"errors":{"work-cp":"unavailable"}})],
            json_output,
            None,
        )
        .await;
        assert!(!output.status.success());
        if json_output {
            let result: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(result["complete"], false);
            assert_eq!(result["errors"]["work-cp"], "unavailable");
        } else {
            assert!(stderr(&output).contains("work-cp: unavailable"));
        }
    }
}

#[tokio::test]
async fn disabled_models_cli_persists_global_rules_preserves_comments_and_enables_aliases() {
    for (command, input, expected) in [
        ("disable", "gpt-test", vec!["claude-sonnet-4.6", "gpt-test"]),
        ("enable", "sonnet4.6", vec![]),
    ] {
        let workspace = tempfile::tempdir().unwrap();
        let file = workspace.path().join("config.toml");
        fs::write(&file, "# policy comment\n[models]\ndisabled = [\"claude-sonnet-4.6\"]\ncache_ttl_ms = 60000\n").unwrap();
        let (output, requests) = run_in_workspace(workspace.path(), &["model", command, input], vec![
            json!({"features":[crosslane_ipc::protocol::GLOBAL_MODEL_DISABLE_FEATURE]}),
            json!({"config_file":file,"accounts_file":"","daily_file":"","stats_file":"","admin_socket":""}),
            json!({"applied":true,"needs_restart":[]}),
        ]).await;
        assert_success(&output);
        assert_eq!(
            requests
                .iter()
                .map(|request| request.method.as_str())
                .collect::<Vec<_>>(),
            [
                method::V2_CAPABILITIES,
                method::CONFIG_PATH,
                method::CONFIG_RELOAD
            ]
        );
        let raw = fs::read_to_string(file).unwrap();
        assert!(raw.contains("# policy comment"));
        let config: crosslane_core::config::Config = toml::from_str(&raw).unwrap();
        assert_eq!(config.models.disabled, expected);
        assert_eq!(config.models.cache_ttl_ms, 60000);
    }
}

#[tokio::test]
async fn disabled_models_cli_rejects_old_daemons_without_mutating_configuration() {
    let workspace = tempfile::tempdir().unwrap();
    let file = workspace.path().join("config.toml");
    let raw = "[models]\ndisabled = []\n";
    fs::write(&file, raw).unwrap();
    let (output, requests) = run_in_workspace(
        workspace.path(),
        &["model", "disable", "gpt-test"],
        vec![json!({"features":[]})],
    )
    .await;
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("不支持全局模型禁用"));
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, method::V2_CAPABILITIES);
    assert_eq!(fs::read_to_string(file).unwrap(), raw);
}

#[tokio::test]
async fn disabled_models_cli_list_marks_catalog_and_missing_entries() {
    let result = json!({"schema_version":2,"scope":{"provider":"all"},"models":[
        {"provider_id":"copilot","id":"gpt-test","display_name":"Test","protocols":[],"capabilities":{}}
    ],"disabled_models":["gpt-test","retired-model"],"errors":{},"complete":true});
    let (output, _) = run(&["model", "list"], vec![result.clone()]).await;
    assert_success(&output);
    let body: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(body["models"].as_array().unwrap().len(), 2);
    for model in body["models"].as_array().unwrap() {
        assert_eq!(model["disabled"], true);
        assert_eq!(model["available"], false);
    }
    let (output, _) = run_human(&["models", "list"], vec![result]).await;
    assert_success(&output);
    assert_eq!(
        String::from_utf8(output.stdout)
            .unwrap()
            .matches("不可用（已禁用）")
            .count(),
        2
    );
}

async fn run_human(args: &[&str], results: Vec<Value>) -> (std::process::Output, Vec<Request>) {
    let workspace = tempfile::tempdir().expect("tempdir");
    run_in_workspace_mode(workspace.path(), args, results, false).await
}

async fn run_in_workspace(
    workspace: &Path,
    args: &[&str],
    results: Vec<Value>,
) -> (std::process::Output, Vec<Request>) {
    run_in_workspace_mode(workspace, args, results, true).await
}

async fn run_in_workspace_mode(
    workspace: &Path,
    args: &[&str],
    results: Vec<Value>,
    json_output: bool,
) -> (std::process::Output, Vec<Request>) {
    run_in_workspace_scoped(workspace, args, results, json_output, None).await
}

async fn run_in_workspace_scoped(
    workspace: &Path,
    args: &[&str],
    results: Vec<Value>,
    json_output: bool,
    provider: Option<&str>,
) -> (std::process::Output, Vec<Request>) {
    let socket = workspace.join("admin.sock");
    let listener = UnixListener::bind(&socket).expect("bind mock admin socket");
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for result in results {
            let (stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .expect("accept timeout")
                .expect("accept request");
            let (read, mut write) = stream.into_split();
            let line = BufReader::new(read)
                .lines()
                .next_line()
                .await
                .expect("read request")
                .expect("request line");
            let request: Request = decode_line(&line).expect("decode request");
            write
                .write_all(
                    encode_line(&Response::ok(request.id, result))
                        .expect("encode response")
                        .as_bytes(),
                )
                .await
                .expect("write response");
            requests.push(request);
        }
        requests
    });
    let mut command = Command::new(env!("CARGO_BIN_EXE_crosslane"));
    if json_output {
        command.arg("--json");
    }
    command
        .current_dir(workspace)
        .arg("--socket")
        .arg(&socket)
        .args(args)
        .env_remove("CROSSLANE_ADMIN_SOCKET")
        .env_remove("CROSSLANE_WRAPPER_LOCAL_ONLY")
        .env_remove("CROSSLANE_PROVIDER");
    if let Some(provider) = provider {
        command.env("CROSSLANE_PROVIDER", provider);
    }
    let output = tokio::time::timeout(Duration::from_secs(5), command.output())
        .await
        .expect("CLI timeout")
        .expect("run CLI");
    let requests = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("mock server timeout")
        .expect("mock server task");
    (output, requests)
}

#[tokio::test]
async fn provider_environment_and_explicit_scope_reach_the_same_tag_rpc() {
    for (args, expected) in [
        (
            vec!["account", "tag", "acc_b85b28b7", "--add", "copilot-test"],
            "copilot",
        ),
        (
            vec![
                "--provider",
                "kiro",
                "account",
                "tag",
                "acc_test",
                "--add",
                "prod",
            ],
            "kiro",
        ),
        (
            vec![
                "account",
                "--provider",
                "kiro",
                "tag",
                "acc_test",
                "--add",
                "prod",
            ],
            "kiro",
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let (output, requests) = run_in_workspace_scoped(
            directory.path(),
            &args,
            vec![json!({"id":"acc_test","tags":["prod"]})],
            true,
            Some("copilot"),
        )
        .await;
        assert_success(&output);
        assert_eq!(requests[0].method, method::V2_ACCOUNT_TAG);
        assert_eq!(requests[0].params["provider"], expected);
    }
}

#[tokio::test]
async fn copilot_scope_cannot_execute_kiro_only_account_commands() {
    let output = Command::new(env!("CARGO_BIN_EXE_crosslane"))
        .args(["--provider", "copilot", "account", "overage", "disable"])
        .env_remove("CROSSLANE_PROVIDER")
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(stderr(&output).contains("仅适用于 Kiro"));
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn run_with_secret_input(
    args: &[&str],
    input: &str,
    result: Value,
) -> (std::process::Output, Request) {
    let directory = tempfile::tempdir().unwrap();
    run_with_secret_input_in_workspace(directory.path(), args, input, result, None).await
}

async fn run_with_secret_input_in_workspace(
    workspace: &Path,
    args: &[&str],
    input: &str,
    result: Value,
    provider: Option<&str>,
) -> (std::process::Output, Request) {
    let socket = workspace.join("admin.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let raw = BufReader::new(read)
            .lines()
            .next_line()
            .await
            .unwrap()
            .unwrap();
        let request: Request = decode_line(&raw).unwrap();
        write
            .write_all(
                encode_line(&Response::ok(request.id, result))
                    .unwrap()
                    .as_bytes(),
            )
            .await
            .unwrap();
        request
    });
    let mut command = Command::new(env!("CARGO_BIN_EXE_crosslane"));
    command
        .current_dir(workspace)
        .args(["--json", "--socket"])
        .arg(&socket)
        .args(args)
        .env_remove("CROSSLANE_ADMIN_SOCKET")
        .env_remove("CROSSLANE_PROVIDER")
        .env_remove("CROSSLANE_WRAPPER_LOCAL_ONLY")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let Some(provider) = provider {
        command.env("CROSSLANE_PROVIDER", provider);
    }
    let mut process = command.spawn().unwrap();
    let mut stdin = process.stdin.take().unwrap();
    stdin.write_all(input.as_bytes()).await.unwrap();
    stdin.shutdown().await.unwrap();
    drop(stdin);
    let output = tokio::time::timeout(Duration::from_secs(5), process.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_success(&output);
    let request = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    (output, request)
}

#[tokio::test]
async fn dotenv_provider_reaches_auth_rpc_and_respects_override_precedence() {
    for (args, rpc, input) in [
        (
            &["account", "add", "--token-stdin"][..],
            method::V2_ACCOUNT_IMPORT_TOKEN,
            "synthetic-test-token\n",
        ),
        (
            &[
                "account",
                "login-code",
                "--task",
                "test-task",
                "--code-stdin",
            ][..],
            method::V2_LOGIN_SUBMIT_CODE,
            "123456\n",
        ),
    ] {
        for (shell_provider, explicit_provider, expected) in [
            (None, None, "dotenv-cp"),
            (Some("shell-cp"), None, "shell-cp"),
            (Some("shell-cp"), Some("flag-cp"), "flag-cp"),
        ] {
            let workspace = tempfile::tempdir().unwrap();
            fs::write(
                workspace.path().join(".env"),
                "CROSSLANE_PROVIDER=dotenv-cp\n",
            )
            .unwrap();
            let mut args = args.to_vec();
            if let Some(provider) = explicit_provider {
                args.extend(["--provider", provider]);
            }
            let (output, request) = run_with_secret_input_in_workspace(
                workspace.path(),
                &args,
                input,
                json!({"ok":true}),
                shell_provider,
            )
            .await;
            assert_success(&output);
            assert_eq!(request.method, rpc);
            assert_eq!(request.params["provider"], expected);
            let field = if rpc == method::V2_ACCOUNT_IMPORT_TOKEN {
                "token"
            } else {
                "code"
            };
            assert_eq!(request.params[field], input.trim());
            assert!(!String::from_utf8_lossy(&output.stdout).contains(input.trim()));
            assert!(!String::from_utf8_lossy(&output.stderr).contains(input.trim()));
        }
    }
}

#[tokio::test]
async fn auth_commands_still_require_a_provider_after_loading_dotenv() {
    for args in [
        &["account", "add", "--token-stdin"][..],
        &[
            "account",
            "login-code",
            "--task",
            "test-task",
            "--code-stdin",
        ][..],
    ] {
        let (output, requests) = run(args, vec![]).await;
        assert_eq!(output.status.code(), Some(2));
        assert!(requests.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("需要 --provider <ID>"));
    }
}

#[tokio::test]
async fn remote_copilot_login_uses_the_instance_id_and_keeps_origin_credentials_separate() {
    let (output, request) = run_with_secret_input(
        &[
            "account",
            "add",
            "--provider",
            "github-copilot",
            "--auth",
            "device-flow",
            "--username",
            "developer_company",
            "--sso-username",
            "developer@example.com",
            "--password-stdin",
            "--capture-headers",
        ],
        "private-azure-password\n",
        json!({
            "id":"login_test","status":"authorized","account_id":"acc_test",
            "browser":{"mode":"headless","stage":"finished","message":"远端授权完成"}
        }),
    )
    .await;
    assert_success(&output);
    assert_eq!(request.method, method::V2_LOGIN_START);
    assert_eq!(request.params["provider"], "github-copilot");
    assert_eq!(request.params["auth"], "device-flow-headless");
    assert_eq!(
        request.params["browser"]["github_username"],
        "developer_company"
    );
    assert!(request.params["browser"].get("github_password").is_none());
    assert_eq!(
        request.params["browser"]["sso_password"],
        "private-azure-password"
    );
    assert_eq!(request.params["capture_headers"], true);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stdout.contains("private-azure-password"));
    assert!(!stderr.contains("private-azure-password"));
    assert!(stderr.contains("无需打开本地浏览器"));
    assert!(!stderr.contains("请在自己电脑"));
    let final_state: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(final_state["status"], "authorized");
}

#[tokio::test]
async fn copilot_csv_batch_uses_remote_browser_credentials_and_returns_one_json_result() {
    let (output, request) = run_with_secret_input(
        &["account", "add", "--provider", "github-copilot", "--batch", "-"],
        "github_username,sso_username,sso_password,label\ndeveloper_company,developer@example.com,private-azure-password,team\n",
        json!({"id":"login_test","status":"authorized","account_id":"acc_test"}),
    ).await;
    assert_success(&output);
    assert_eq!(request.method, method::V2_LOGIN_START);
    assert_eq!(request.params["auth"], "device-flow-headless");
    assert_eq!(request.params["provider"], "github-copilot");
    assert_eq!(
        request.params["browser"]["github_username"],
        "developer_company"
    );
    assert_eq!(
        request.params["browser"]["sso_password"],
        "private-azure-password"
    );
    assert_eq!(request.params["label"], "team");
    assert!(request.params["browser"].get("github_password").is_none());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private-azure-password"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("private-azure-password"));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["accounts"].as_array().unwrap().len(), 1);
    assert_eq!(result["complete"], true);
}

#[tokio::test]
async fn remote_copilot_mfa_code_stays_out_of_command_arguments_and_output() {
    let (output, request) = run_with_secret_input(
        &[
            "account",
            "login-code",
            "--provider",
            "github-copilot",
            "--task",
            "login_test",
            "--code-stdin",
        ],
        "123456\n",
        json!({"submitted":true}),
    )
    .await;
    assert_success(&output);
    assert_eq!(request.method, method::V2_LOGIN_SUBMIT_CODE);
    assert_eq!(request.params["code"], "123456");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("123456"));
}

#[tokio::test]
async fn explicit_list_actions_use_the_existing_methods() {
    let (tasks, requests) = run(&["tasks", "list"], vec![json!([])]).await;
    assert_success(&tasks);
    assert_eq!(requests[0].method, method::TASKS);
    assert_eq!(requests[0].params, json!({}));

    let (models, requests) = run(
        &["models", "list", "--refresh"],
        vec![json!({
            "schema_version": 2,
            "scope": {"provider": "all"},
            "models": [],
            "errors": {},
            "complete": true
        })],
    )
    .await;
    assert_success(&models);
    assert_eq!(requests[0].method, method::V2_MODELS);
    assert_eq!(
        requests[0].params,
        json!({"provider": "all", "provider_kind": null, "refresh": true})
    );
}

#[tokio::test]
async fn account_list_sends_a_boolean_enabled_filter() {
    let (output, requests) = run(
        &["account", "list"],
        vec![json!({
            "schema_version": 2,
            "scope": {"provider": "all"},
            "accounts": [],
            "errors": {},
            "complete": true
        })],
    )
    .await;
    assert_success(&output);
    assert_eq!(requests[0].method, method::V2_ACCOUNT_LIST);
    assert_eq!(requests[0].params["enabled_only"], false);
}

#[tokio::test]
async fn account_overage_enable_updates_config_and_uses_the_minimal_rpc_sequence() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let config_file = workspace.path().join("config.toml");
    fs::write(
        &config_file,
        "# keep this comment\n[pool]\nenable_overage = false\nmax_overage_credits_per_account = 250.0\n",
    )
    .expect("write config");
    let mut effective = crosslane_core::config::Config::default();
    effective.pool.enable_overage = true;
    effective.pool.max_overage_credits_per_account = Some(500.0);
    let path = config_file.to_string_lossy().into_owned();
    let (output, requests) = run_in_workspace(
        workspace.path(),
        &[
            "account",
            "overage",
            "enable",
            "--max-credits",
            "500",
            "--no-refresh",
        ],
        vec![
            json!({
                "config_file": path,
                "accounts_file": workspace.path().join("accounts.json"),
                "daily_file": workspace.path().join("daily.json"),
                "stats_file": workspace.path().join("stats.json"),
                "admin_socket": workspace.path().join("admin.sock")
            }),
            json!({"applied": true, "needs_restart": []}),
            json!({
                "path": config_file,
                "raw": "",
                "effective_json": serde_json::to_value(effective).expect("effective config")
            }),
            json!({
                "schema_version": 2,
                "scope": {"provider": "kiro"},
                "accounts": [],
                "errors": {},
                "complete": true
            }),
        ],
    )
    .await;
    assert_success(&output);
    assert_eq!(
        requests
            .iter()
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        [
            method::CONFIG_PATH,
            method::CONFIG_RELOAD,
            method::CONFIG_SHOW,
            method::V2_ACCOUNT_LIST
        ]
    );
    assert_eq!(requests[3].params["enabled_only"], false);
    let saved = fs::read_to_string(&config_file).expect("read updated config");
    assert!(saved.contains("# keep this comment"));
    let parsed: crosslane_core::config::Config =
        toml::from_str(&saved).expect("parse updated config");
    assert!(parsed.pool.enable_overage);
    assert_eq!(parsed.pool.max_overage_credits_per_account, Some(500.0));
    let output: Value = serde_json::from_slice(&output.stdout).expect("JSON output");
    assert_eq!(output["action"], "enabled");
    assert_eq!(output["enabled"], true);
}

#[tokio::test]
async fn account_overage_refresh_reports_raw_machine_values() {
    let mut effective = crosslane_core::config::Config::default();
    effective.pool.enable_overage = true;
    effective.pool.max_overage_credits_per_account = Some(500.0);
    let (output, requests) = run(
        &["account", "overage", "show", "--refresh"],
        vec![
            json!({"name": "status_check", "result": "ok: 1 healthy, 0 failed"}),
            json!({
                "path": "/tmp/config.toml",
                "raw": "",
                "effective_json": serde_json::to_value(effective).expect("effective config")
            }),
            json!({
                "schema_version": 2,
                "scope": {"provider": "kiro"},
                "accounts": [{
                    "provider_id": "kiro",
                    "provider_kind": "kiro",
                    "id": "acc_00000001",
                    "display_name": "a@example.com",
                    "email": "a@example.com",
                    "label": null,
                    "enabled": true,
                    "health": "available",
                    "auth_state": "ready",
                    "tags": ["prod"],
                    "quota_current": 10000.01,
                    "quota_limit": 10500.0,
                    "quota_unit": "kiro_credits",
                    "supported_models": [],
                    "details": {
                        "overage_enabled": true,
                        "max_overage_credits_per_account": 500.0,
                        "effective_overage_cap": 500.0,
                        "kiro_overage_cap": 10000.0,
                        "kiro_overage_total_limit": 20000.0
                    }
                }],
                "errors": {},
                "complete": true
            }),
        ],
    )
    .await;
    assert_success(&output);
    assert_eq!(
        requests
            .iter()
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        [
            method::TASK_RUN,
            method::CONFIG_SHOW,
            method::V2_ACCOUNT_LIST
        ]
    );
    assert_eq!(
        requests[0].params,
        json!({"name": "status_check", "provider": null})
    );
    let output: Value = serde_json::from_slice(&output.stdout).expect("JSON output");
    assert_eq!(output["refresh"]["ok"], true);
    assert_eq!(output["accounts"][0]["health"], "available");
    assert_eq!(output["accounts"][0]["current"], 10000.01);
    assert_eq!(output["accounts"][0]["proxy_total"], 10500.0);
    assert_eq!(output["accounts"][0]["kiro_total"], 20000.0);
    assert_eq!(output["accounts"][0]["effective_overage"], 500.0);
    assert_eq!(output["accounts"][0]["kiro_overage"], 10000.0);
}

#[tokio::test]
async fn explicit_log_action_preserves_the_existing_query_parameters() {
    let (output, requests) = run(
        &[
            "logs",
            "show",
            "--tail",
            "25",
            "--level",
            "error",
            "--account",
            "alice@example.com",
        ],
        vec![json!({"entries": []})],
    )
    .await;
    assert_success(&output);
    assert_eq!(requests[0].method, method::LOGS);
    assert_eq!(
        requests[0].params,
        json!({
            "after_request_id": null,
            "tail": 25,
            "wait_ms": 0,
            "level": "error",
            "account": "alice@example.com",
            "provider": null
        })
    );

    let (output, requests) = run(
        &["logs", "show", "--level", "info"],
        vec![json!({"entries": []})],
    )
    .await;
    assert_success(&output);
    assert_eq!(requests[0].params["level"], Value::Null);
}

#[tokio::test]
async fn stats_group_is_normalized_to_the_daemon_contract() {
    let (output, requests) = run(&["stats", "--detail", "--by", "apikey"], vec![json!({})]).await;
    assert_success(&output);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, method::STATS);
    assert_eq!(requests[0].params["by"], "apikey");
    assert_eq!(requests[0].params["detail"], true);
}

#[tokio::test]
async fn diagnose_all_preserves_the_two_step_diagnosis_parameters() {
    let (output, requests) = run(
        &[
            "diagnose",
            "all",
            "--region",
            "us-west-2",
            "--timeout",
            "30s",
            "--concurrency",
            "4",
        ],
        vec![json!({"reachable": true}), json!({"healthy": true})],
    )
    .await;
    assert_success(&output);
    assert_eq!(requests[0].method, method::DIAGNOSE_ENDPOINTS);
    assert_eq!(requests[0].params, json!({"region": "us-west-2"}));
    assert_eq!(requests[1].method, method::DIAGNOSE_ACCOUNT);
    assert_eq!(
        requests[1].params,
        json!({"all": true, "timeout_secs": 30, "concurrency": 4})
    );
}

#[tokio::test]
async fn service_create_sends_multiple_account_tags_as_one_array() {
    let (output, requests) = run(
        &[
            "service",
            "create",
            "--name",
            "team",
            "--account-tag",
            "xx1",
            "xx2",
        ],
        vec![json!({
            "service": {
                "id": "svc_team",
                "name": "team",
                "host": "127.0.0.1",
                "port": 5581,
                "enabled": true,
                "running": true,
                "api_key_ids": ["ak_team"],
                "account_tags": ["xx1", "xx2"],
                "account_ids": [],
                "excluded_account_ids": [],
                "created_at": 1
            },
            "api_key": {"id": "ak_team", "name": "team-default", "key": "sk-test"}
        })],
    )
    .await;
    assert_success(&output);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, method::SERVICE_CREATE);
    assert_eq!(requests[0].params["account_tag"], json!(["xx1", "xx2"]));
}

#[tokio::test]
async fn destructive_account_command_supports_yes_and_json_cancellation() {
    let (removed, requests) = run(
        &["account", "rm", "acc_00000001", "--yes"],
        vec![json!({"id": "acc_00000001", "removed": true})],
    )
    .await;
    assert_success(&removed);
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, method::V2_ACCOUNT_REMOVE);
    assert_eq!(requests[0].params["id"], "acc_00000001");

    let (cancelled, requests) = run(&["account", "rm", "acc_00000001"], vec![]).await;
    assert_success(&cancelled);
    assert!(requests.is_empty());
    let cancelled: Value = serde_json::from_slice(&cancelled.stdout).expect("JSON cancellation");
    assert_eq!(cancelled["removed"], false);
    assert_eq!(cancelled["cancelled"], true);
}

#[tokio::test]
async fn batch_account_remove_emits_one_json_document() {
    let (output, requests) = run(
        &["account", "rm", "acc_00000001", "acc_00000002", "--yes"],
        vec![
            json!({"id": "acc_00000001", "removed": true}),
            json!({"id": "acc_00000002", "removed": true}),
        ],
    )
    .await;
    assert_success(&output);
    assert_eq!(requests.len(), 2);
    let output: Value = serde_json::from_slice(&output.stdout).expect("single JSON document");
    assert_eq!(output["results"].as_array().map(Vec::len), Some(2));
    assert_eq!(output["errors"].as_array().map(Vec::len), Some(0));
    assert_eq!(output["complete"], true);
}

#[tokio::test]
async fn edit_commands_reject_empty_changes_without_rpc_calls() {
    for args in [
        &["provider", "edit", "kiro"][..],
        &["service", "edit", "main"][..],
        &["apikey", "edit", "ci"][..],
        &["alert", "edit", "ops"][..],
        &["model-map", "edit", "low-credit"][..],
    ] {
        let (output, requests) = run(args, vec![]).await;
        assert!(!output.status.success(), "{args:?}");
        assert!(requests.is_empty(), "{args:?}");
        assert!(stderr(&output).contains("没有指定修改项"), "{args:?}");
    }
    for resource in ["service", "apikey", "model-map"] {
        let workspace = tempfile::tempdir().unwrap();
        let (output, requests) = run_in_workspace_scoped(
            workspace.path(),
            &[resource, "edit", "team"],
            vec![],
            true,
            Some("copilot"),
        )
        .await;
        assert!(!output.status.success());
        assert!(requests.is_empty());
        assert!(
            stderr(&output).contains("没有指定修改项"),
            "{resource}: {}",
            stderr(&output)
        );
    }
}

#[tokio::test]
async fn diagnose_all_honors_human_output_mode() {
    let (output, requests) = run_human(
        &["diagnose", "all"],
        vec![json!({"reachable": true}), json!({"healthy": true})],
    )
    .await;
    assert_success(&output);
    assert_eq!(requests.len(), 2);
    let text = String::from_utf8(output.stdout).expect("UTF-8 output");
    assert!(text.contains("端点诊断"), "{text}");
    assert!(text.contains("账号诊断"), "{text}");
    assert!(serde_json::from_str::<Value>(&text).is_err());
}

#[tokio::test]
async fn config_mutation_commands_honor_json_output() {
    let alert_workspace = tempfile::tempdir().expect("tempdir");
    let alert_config = alert_workspace.path().join("config.toml");
    fs::write(&alert_config, "").expect("write alert config");
    let (alert, requests) = run_in_workspace(
        alert_workspace.path(),
        &[
            "alert",
            "add",
            "--name",
            "ops",
            "--platform",
            "dingtalk",
            "--webhook-url",
            "https://example.com/hook",
            "--event",
            "token-refresh-failed",
        ],
        vec![
            json!({
                "config_file": alert_config,
                "accounts_file": alert_workspace.path().join("accounts.json"),
                "daily_file": alert_workspace.path().join("daily.json"),
                "stats_file": alert_workspace.path().join("stats.json"),
                "admin_socket": alert_workspace.path().join("admin.sock")
            }),
            json!({"applied": true, "needs_restart": []}),
        ],
    )
    .await;
    assert_success(&alert);
    assert_eq!(
        requests
            .iter()
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        [method::CONFIG_PATH, method::CONFIG_RELOAD]
    );
    let alert: Value = serde_json::from_slice(&alert.stdout).expect("alert JSON");
    assert_eq!(alert, json!({"name": "ops", "created": true}));

    let map_workspace = tempfile::tempdir().expect("tempdir");
    let map_config = map_workspace.path().join("config.toml");
    fs::write(&map_config, "").expect("write model map config");
    let (mapping, requests) = run_in_workspace(
        map_workspace.path(),
        &[
            "model-map",
            "add",
            "--name",
            "low-credit",
            "--source",
            "claude-opus-*",
            "--target",
            "claude-sonnet-4",
        ],
        vec![
            json!({
                "config_file": map_config,
                "accounts_file": map_workspace.path().join("accounts.json"),
                "daily_file": map_workspace.path().join("daily.json"),
                "stats_file": map_workspace.path().join("stats.json"),
                "admin_socket": map_workspace.path().join("admin.sock")
            }),
            json!({"applied": true, "needs_restart": []}),
        ],
    )
    .await;
    assert_success(&mapping);
    assert_eq!(
        requests
            .iter()
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        [method::CONFIG_PATH, method::CONFIG_RELOAD]
    );
    let mapping: Value = serde_json::from_slice(&mapping.stdout).expect("model map JSON");
    assert_eq!(mapping, json!({"name": "low-credit", "created": true}));
}
