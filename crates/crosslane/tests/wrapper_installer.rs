//! Verify that wrapper upgrades either replace the target completely or leave it untouched.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

const MANAGED_OLD_WRAPPER: &str =
    "#!/bin/sh\n# Managed by crosslane's install-crosslane-wrapper.sh.\necho old\n";

fn installer() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/install-crosslane-wrapper.sh")
}

fn temp_artifacts(directory: &Path) -> Vec<String> {
    fs::read_dir(directory)
        .expect("read target directory")
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().into_string().ok()?;
            name.starts_with(".crosslane.tmp.").then_some(name)
        })
        .collect()
}

#[test]
fn successful_install_replaces_a_managed_wrapper_without_temp_files() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let target = workspace.path().join("crosslane");
    fs::write(&target, MANAGED_OLD_WRAPPER).expect("write old wrapper");

    let output = Command::new("/bin/sh")
        .arg(installer())
        .args(["--target", target.to_str().expect("UTF-8 path")])
        .output()
        .expect("run installer");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(&target).expect("read installed wrapper"),
        fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/crosslane-docker"))
            .expect("read source wrapper")
    );
    assert!(temp_artifacts(workspace.path()).is_empty());
}

#[test]
fn rename_upgrade_recognizes_a_pre_rename_managed_wrapper_at_a_custom_target() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let target = workspace.path().join("my-proxy");
    fs::write(
        &target,
        "#!/bin/sh\n# Managed by kiro-proxy's install-kproxy-wrapper.sh.\necho legacy\n",
    )
    .expect("write pre-rename wrapper");

    for check_only in [true, false] {
        let mut command = Command::new("/bin/sh");
        command
            .arg(installer())
            .args(["--target", target.to_str().unwrap()]);
        if check_only {
            command.arg("--check");
        }
        let output = command.output().expect("run installer");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        if check_only {
            assert!(fs::read_to_string(&target).unwrap().contains("echo legacy"));
        }
    }
    assert_eq!(
        fs::read(&target).unwrap(),
        fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/crosslane-docker"))
            .unwrap()
    );
    assert!(temp_artifacts(workspace.path()).is_empty());
}

#[test]
fn unrelated_commands_are_preserved_without_force() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let target = workspace.path().join("crosslane");
    let original = "#!/bin/sh\necho unrelated\n";
    fs::write(&target, original).unwrap();
    for check_only in [true, false] {
        let mut command = Command::new("/bin/sh");
        command.arg(installer()).arg("--target").arg(&target);
        if check_only {
            command.arg("--check");
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("not managed by this project"));
        assert_eq!(fs::read_to_string(&target).unwrap(), original);
        assert!(temp_artifacts(workspace.path()).is_empty());
    }
}

#[test]
fn failed_atomic_rename_preserves_the_existing_wrapper() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let target = workspace.path().join("crosslane");
    fs::write(&target, MANAGED_OLD_WRAPPER).expect("write old wrapper");

    let bin = workspace.path().join("bin");
    fs::create_dir(&bin).expect("create bin directory");
    let fake_mv = bin.join("mv");
    fs::write(&fake_mv, "#!/bin/sh\nexit 73\n").expect("write fake mv");
    let mut permissions = fs::metadata(&fake_mv)
        .expect("fake mv metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&fake_mv, permissions).expect("make fake mv executable");

    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new("/bin/sh")
        .arg(installer())
        .args(["--target", target.to_str().expect("UTF-8 path")])
        .env("PATH", path)
        .output()
        .expect("run installer");

    assert_eq!(output.status.code(), Some(73));
    assert_eq!(
        fs::read_to_string(&target).expect("read preserved wrapper"),
        MANAGED_OLD_WRAPPER
    );
    assert!(temp_artifacts(workspace.path()).is_empty());
}

#[test]
fn directory_targets_are_rejected_even_with_force() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let target = workspace.path().join("bin");
    fs::create_dir(&target).expect("create target directory");

    let output = Command::new("/bin/sh")
        .arg(installer())
        .args(["--target", target.to_str().expect("UTF-8 path"), "--force"])
        .output()
        .expect("run installer");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("target must be a file path"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fs::read_dir(&target)
        .expect("read target directory")
        .next()
        .is_none());
}
