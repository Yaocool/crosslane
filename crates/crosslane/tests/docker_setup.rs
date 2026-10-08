//! Exercise the real deployment script with a strict Docker stub, never a daemon.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

struct Deployment {
    output: Output,
    calls: String,
    saved_image: Option<String>,
    installed_wrapper: bool,
    wrapper_contents: Option<String>,
}

impl Deployment {
    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr).into_owned()
    }

    fn assert_success(&self) {
        assert!(
            self.output.status.success(),
            "stdout:\n{}\nstderr:\n{}\ncalls:\n{}",
            String::from_utf8_lossy(&self.output.stdout),
            self.stderr(),
            self.calls
        );
        assert!(self.stderr().is_empty(), "{}", self.stderr());
        assert!(!self
            .calls
            .contains("crosslane-rollback:crosslane | compose"));
    }

    fn health_calls(&self) -> usize {
        self.calls
            .lines()
            .filter(|line| line.contains(" exec -T crosslaned /usr/local/bin/crosslane health"))
            .count()
    }
}

fn executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn deploy(scenario: &str, build: bool) -> Deployment {
    deploy_with_wrapper(scenario, build, None)
}

fn deploy_with_wrapper(scenario: &str, build: bool, existing_wrapper: Option<&str>) -> Deployment {
    let workspace = tempfile::tempdir().unwrap();
    let bin = workspace.path().join("bin");
    fs::create_dir(&bin).unwrap();
    executable(
        &bin.join("docker"),
        include_str!("fixtures/docker_setup_mock.sh"),
    );
    // Keep timeout/retry tests deterministic and fast without changing production defaults.
    executable(&bin.join("sleep"), "#!/bin/sh\nexit 0\n");
    // Reproduce the production Linux volume checks on macOS test hosts as well.
    executable(&bin.join("uname"), "#!/bin/sh\necho Linux\n");
    let image_state = workspace.path().join("image-state");
    fs::write(&image_state, "crosslane:test-old\n").unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let wrapper = workspace.path().join("crosslane");
    if let Some(contents) = existing_wrapper {
        fs::write(&wrapper, contents).unwrap();
    }
    let output = Command::new("/bin/sh")
        .arg(repo.join("deploy/docker-setup.sh"))
        .args(["--image", "crosslane:test-new"])
        .arg(if build { "--build" } else { "--no-pull" })
        .args(["--timeout", "1", "--target"])
        .arg(&wrapper)
        .env_clear()
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", bin.display()),
        )
        .env("DOCKER_SETUP_TEST_DIR", workspace.path())
        .env("DOCKER_SETUP_TEST_SCENARIO", scenario)
        .env("CROSSLANE_IMAGE_STATE_FILE", &image_state)
        .env("CROSSLANE_BUILD_IMAGE", "crosslane:test-new")
        .output()
        .unwrap();
    let calls = fs::read_to_string(workspace.path().join("docker-calls")).unwrap();
    // The stub rejects unrecognized commands instead of masking harness mistakes.
    assert!(!calls.contains("UNEXPECTED"), "{calls}");
    Deployment {
        output,
        calls,
        saved_image: fs::read_to_string(image_state).ok(),
        installed_wrapper: wrapper.is_file(),
        wrapper_contents: fs::read_to_string(wrapper).ok(),
    }
}

#[test]
fn healthy_deployment_uses_supported_exec_flag_without_rollback() {
    for build in [false, true] {
        let result = deploy("healthy", build);
        result.assert_success();
        assert!(result.installed_wrapper);
        assert_eq!(result.health_calls(), 1);
        let stdout = String::from_utf8_lossy(&result.output.stdout);
        let health_position = stdout
            .find("Waiting for crosslaned to become healthy")
            .expect("health wait output");
        let install_position = stdout
            .find("Installing the host crosslane command")
            .expect("wrapper install output");
        assert!(health_position < install_position, "{stdout}");
        assert_eq!(
            result.saved_image.as_deref(),
            Some(if build {
                "crosslane:test-old\n"
            } else {
                "crosslane:test-new\n"
            })
        );
    }
}

#[test]
fn transient_health_failure_is_retried_without_rollback() {
    for build in [false, true] {
        let result = deploy("transient", build);
        result.assert_success();
        assert_eq!(result.health_calls(), 2);
    }
}

#[test]
fn unhealthy_deployment_reports_the_error_and_restores_previous_image() {
    for build in [false, true] {
        let result = deploy("unhealthy", build);
        assert!(!result.output.status.success());
        assert!(!result.installed_wrapper);
        let stderr = result.stderr();
        assert!(
            stderr.contains("Health check failed after 1s (exit 7)"),
            "{stderr}"
        );
        assert!(
            stderr.contains("admin socket unavailable: crosslane:test-new"),
            "{stderr}"
        );
        assert!(
            stderr.contains("the previous image was restored successfully"),
            "{stderr}"
        );
        assert_eq!(result.health_calls(), 3);
        assert!(result
            .calls
            .contains("crosslane-rollback:crosslane | compose"));
        assert_eq!(result.saved_image.as_deref(), Some("crosslane:test-old\n"));
    }
}

#[test]
fn unhealthy_rollback_reports_its_own_health_error() {
    for build in [false, true] {
        let result = deploy("rollback-unhealthy", build);
        assert!(!result.output.status.success());
        assert!(!result.installed_wrapper);
        let stderr = result.stderr();
        assert_eq!(
            stderr
                .matches("Health check failed after 1s (exit 7)")
                .count(),
            2
        );
        assert!(
            stderr.contains("admin socket unavailable: crosslane:test-new"),
            "{stderr}"
        );
        assert!(
            stderr.contains("admin socket unavailable: crosslane-rollback:crosslane"),
            "{stderr}"
        );
        assert!(
            stderr.contains("automatic rollback did not become healthy"),
            "{stderr}"
        );
        assert_eq!(result.health_calls(), 4);
        assert_eq!(result.saved_image.as_deref(), Some("crosslane:test-old\n"));
    }
}

#[test]
fn unrelated_wrapper_is_rejected_before_replacing_the_container() {
    let original = "#!/bin/sh\necho unrelated\n";
    let result = deploy_with_wrapper("healthy", false, Some(original));

    assert!(!result.output.status.success());
    assert!(result.stderr().contains("is not managed by this project"));
    assert_eq!(result.wrapper_contents.as_deref(), Some(original));
    assert!(!result
        .calls
        .lines()
        .any(|line| line.contains(" up --detach")));
    assert_eq!(result.saved_image.as_deref(), Some("crosslane:test-old\n"));
}
