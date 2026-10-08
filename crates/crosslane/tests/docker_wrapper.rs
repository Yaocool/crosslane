//! Exercise Docker-backed CLI routing with a strict Docker stub.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

fn executable(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write executable");
    let mut permissions = fs::metadata(path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("set executable mode");
}

fn run_wrapper_with_local_support(
    state: &str,
    args: &[&str],
    local_supported: bool,
) -> (Output, String, String) {
    run_wrapper_scoped(state, args, local_supported, None)
}

fn run_wrapper_scoped(
    state: &str,
    args: &[&str],
    local_supported: bool,
    provider: Option<&str>,
) -> (Output, String, String) {
    let workspace = tempfile::tempdir().expect("tempdir");
    let docker = workspace.path().join("docker");
    let calls = workspace.path().join("docker.calls");
    let stdin_log = workspace.path().join("docker.stdin");
    let batch_file = workspace.path().join("accounts.csv");
    fs::write(&batch_file, "email,password\nalice@example.com,secret\n")
        .expect("write batch fixture");
    executable(
        &docker,
        r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$DOCKER_CALL_LOG"
case "$1" in
  ps)
    case "$DOCKER_STATE" in
      missing) ;;
      multiple) printf '%s\n' test-container other-container ;;
      *) printf '%s\n' test-container ;;
    esac
    ;;
  inspect)
    case "$*" in
      *'{{.State.Status}}'*) printf '%s\n' "$DOCKER_STATE" ;;
      *'{{.Image}}'*) printf '%s\n' 'sha256:test-image' ;;
      *'{{range .Mounts}}'*) printf '%s\n' '/srv/crosslane-data' ;;
      *) printf '%s\n' 'unexpected inspect' >&2; exit 91 ;;
    esac
    ;;
  run)
    case "$DOCKER_LOCAL_SUPPORTED:$*" in
      0:*' help --all') exit 2 ;;
    esac
    printf '%s\n' 'stopped-container navigation'
    ;;
  restart|start|stop)
    ;;
  exec)
    cat > "$DOCKER_STDIN_LOG"
    printf '%s\n' 'running-container command'
    ;;
  *)
    printf 'unexpected docker command: %s\n' "$*" >&2
    exit 92
    ;;
esac
"#,
    );
    let path = format!(
        "{}:{}",
        workspace.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let wrapper = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/crosslane-docker");
    let args = args
        .iter()
        .map(|argument| {
            if *argument == "__HOST_BATCH__" {
                batch_file.to_string_lossy().into_owned()
            } else {
                (*argument).to_owned()
            }
        })
        .collect::<Vec<_>>();
    let mut command = Command::new("sh");
    if let Some(provider) = provider {
        command.env("CROSSLANE_PROVIDER", provider);
    } else {
        command.env_remove("CROSSLANE_PROVIDER");
    }
    let output = command
        .arg(wrapper)
        .args(&args)
        .env("PATH", path)
        .env("DOCKER_CALL_LOG", &calls)
        .env("DOCKER_STDIN_LOG", &stdin_log)
        .env("DOCKER_STATE", state)
        .env(
            "DOCKER_LOCAL_SUPPORTED",
            if local_supported { "1" } else { "0" },
        )
        .output()
        .expect("run wrapper");
    let calls = fs::read_to_string(calls).unwrap_or_default();
    let stdin = fs::read_to_string(stdin_log).unwrap_or_default();
    (output, calls, stdin)
}

fn run_wrapper(state: &str, args: &[&str]) -> (Output, String, String) {
    run_wrapper_with_local_support(state, args, true)
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("UTF-8 stdout")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("UTF-8 stderr")
}

fn run_without_docker(args: &[&str]) -> Output {
    let wrapper = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/crosslane-docker");
    Command::new("/bin/sh")
        .arg(wrapper)
        .args(args)
        .env_clear()
        .env("PATH", "/path/without/docker")
        .output()
        .expect("run wrapper without Docker")
}

#[test]
fn stopped_container_uses_its_exact_image_for_local_navigation() {
    let (output, calls, _) = run_wrapper("exited", &["logs"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "stopped-container navigation\n");
    assert!(calls.contains("ps --all"), "{calls}");
    assert!(calls.contains("inspect --format {{.State.Status}} test-container"));
    assert!(calls.contains("inspect --format {{.Image}} test-container"));
    assert!(calls.contains("sha256:test-image help --all"));
    assert!(calls.contains(
        "run --rm --pull never --network none --read-only --no-healthcheck -e CROSSLANE_WRAPPER_LOCAL_ONLY=1 --entrypoint /usr/local/bin/crosslane sha256:test-image logs"
    ));
    assert!(!calls.contains("exec -i"));
}

#[test]
fn stopped_legacy_image_reports_that_offline_navigation_is_unavailable() {
    let (output, calls, _) = run_wrapper_with_local_support("exited", &["logs"], false);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("image is unavailable or too old"),
        "{}",
        stderr(&output)
    );
    assert!(calls.contains("sha256:test-image help --all"), "{calls}");
    assert!(!calls
        .lines()
        .any(|line| line.ends_with("sha256:test-image logs")));
}

#[test]
fn ambiguous_or_missing_deployments_have_actionable_errors() {
    let (missing, _, _) = run_wrapper("missing", &["help"]);
    assert!(!missing.status.success());
    assert!(stderr(&missing).contains("no crosslane daemon container found"));

    let (multiple, _, _) = run_wrapper("multiple", &["help"]);
    assert!(!multiple.status.success());
    assert!(stderr(&multiple).contains("set CROSSLANE_COMPOSE_PROJECT"));
}

#[test]
fn running_container_keeps_the_normal_exec_path() {
    let (output, calls, _) = run_wrapper("running", &["logs", "show", "--tail", "10"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "running-container command\n");
    assert!(calls.contains(
        "exec -i -e CROSSLANE_WRAPPER_HOST_DATA_DIR test-container /usr/local/bin/crosslane logs show --tail 10"
    ));
    assert!(!calls.contains("run --rm"));
}

#[test]
fn public_sso_batch_files_are_streamed_from_the_host() {
    let (output, calls, stdin) = run_wrapper(
        "running",
        &["account", "--json", "add-sso", "--batch", "__HOST_BATCH__"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        calls.contains("exec -i -e CROSSLANE_WRAPPER_BATCH_STDIN=1"),
        "{calls}"
    );
    assert_eq!(stdin, "email,password\nalice@example.com,secret\n");
}

#[test]
fn copilot_batch_files_are_streamed_from_the_host() {
    let (output, calls, stdin) = run_wrapper(
        "running",
        &[
            "account",
            "add",
            "--provider",
            "github-copilot",
            "--batch",
            "__HOST_BATCH__",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        calls.contains("exec -i -e CROSSLANE_WRAPPER_BATCH_STDIN=1"),
        "{calls}"
    );
    assert!(!calls.contains("alice@example.com,secret"));
    assert_eq!(stdin, "email,password\nalice@example.com,secret\n");
}

#[test]
fn provider_scope_does_not_break_host_batch_detection_at_any_depth() {
    for args in [
        &[
            "--provider",
            "work-cp",
            "account",
            "add",
            "--batch",
            "__HOST_BATCH__",
        ][..],
        &[
            "account",
            "--provider",
            "work-cp",
            "add",
            "--batch",
            "__HOST_BATCH__",
        ][..],
        &[
            "-p",
            "work-cp",
            "account",
            "add",
            "--batch",
            "__HOST_BATCH__",
        ][..],
        &["account", "-pwork-cp", "add", "--batch", "__HOST_BATCH__"][..],
        &[
            "--provider=work-cp",
            "account",
            "add",
            "--batch",
            "__HOST_BATCH__",
        ][..],
    ] {
        let (output, calls, stdin) = run_wrapper("running", args);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        assert!(calls.contains("CROSSLANE_WRAPPER_BATCH_STDIN=1"), "{calls}");
        assert_eq!(stdin, "email,password\nalice@example.com,secret\n");
    }
}

#[test]
fn host_provider_default_is_forwarded_for_business_batch_and_offline_navigation() {
    for (state, args) in [
        ("running", &["account", "list"][..]),
        (
            "running",
            &["account", "add", "--batch", "__HOST_BATCH__"][..],
        ),
        ("exited", &["guide", "copilot"][..]),
    ] {
        let (output, calls, _) = run_wrapper_scoped(state, args, true, Some("work-cp"));
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(calls.contains("-e CROSSLANE_PROVIDER "), "{calls}");
    }
}

#[test]
fn sso_batch_help_does_not_inspect_the_host_path() {
    let (output, calls, stdin) =
        run_wrapper("running", &["account", "add-sso", "--batch", ".", "--help"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(calls.lines().any(|line| line.starts_with("exec -i ")));
    assert!(
        !calls.contains("CROSSLANE_WRAPPER_BATCH_STDIN=1"),
        "{calls}"
    );
    assert!(stdin.is_empty());
}

#[test]
fn lifecycle_help_is_navigation_even_when_the_daemon_is_stopped() {
    for args in [
        &["uninstall", "--help"][..],
        &["uninstall", "--yes", "--help"][..],
        &["restart", "--json", "--help"][..],
        &["stop", "--socket", "/tmp/admin.sock", "--help"][..],
        &["--json", "restart", "--help"][..],
        &["--socket", "/tmp/admin.sock", "stop", "--help"][..],
    ] {
        let (output, calls, _) = run_wrapper("exited", args);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        assert!(
            calls.lines().any(|line| {
                line.starts_with("run ")
                    && line.contains("sha256:test-image")
                    && args.iter().all(|argument| line.contains(argument))
            }),
            "{args:?}: {calls}"
        );
        assert!(!calls.lines().any(|line| line.starts_with("stop ")));
        assert!(!calls.lines().any(|line| line.starts_with("rm ")));
        assert!(!calls.lines().any(|line| line.starts_with("restart ")));
    }
}

#[test]
fn lifecycle_help_has_a_safe_fallback_when_docker_is_unavailable() {
    for args in [
        &["restart", "--json", "--help"][..],
        &["stop", "--help"][..],
        &["uninstall", "--yes", "--help"][..],
        &["--json", "restart", "--help"][..],
        &["--socket", "/tmp/admin.sock", "stop", "--help"][..],
    ] {
        let output = run_without_docker(args);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        let text = stdout(&output);
        assert!(text.contains("Usage: crosslane"), "{args:?}: {text}");
        assert!(text.contains("Docker 当前不可用"), "{args:?}: {text}");
    }

    let regular_help = run_without_docker(&["help"]);
    assert!(!regular_help.status.success());
    assert!(stderr(&regular_help).contains("docker command not found"));
}

#[test]
fn lifecycle_actions_accept_leading_global_options() {
    let (restart, calls, _) = run_wrapper("running", &["--json", "restart"]);
    assert!(restart.status.success(), "{}", stderr(&restart));
    assert!(
        calls.contains("restart --time 30 test-container"),
        "{calls}"
    );
    assert!(!calls.lines().any(|line| line.starts_with("exec ")));

    let (stop, calls, _) = run_wrapper("running", &["--socket", "/tmp/admin.sock", "stop"]);
    assert!(stop.status.success(), "{}", stderr(&stop));
    assert!(calls.contains("stop --time 30 test-container"), "{calls}");
    assert!(!calls.lines().any(|line| line.starts_with("exec ")));
}

#[test]
fn malformed_lifecycle_arguments_never_execute_an_action() {
    for args in [
        &["--socket", "--help", "restart"][..],
        &["restart", "unexpected", "--help"][..],
        &["--json", "--json", "restart"][..],
        &["restart", "--socket", "--json"][..],
        &["restart", "--socket", "one", "--socket", "two"][..],
        &["uninstall", "--backup-dir", "--yes", "-y"][..],
        &["uninstall", "--backup-dir", ""][..],
        &["uninstall", "--backup-dir=one", "--backup-dir=two"][..],
        &["uninstall", "-y", "--yes"][..],
        &["uninstall", "--keep-backup", "--delete-backup"][..],
    ] {
        let (output, calls, _) = run_wrapper("running", args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            stderr(&output)
        );
        assert!(calls.is_empty(), "{args:?}: {calls}");
    }
}
