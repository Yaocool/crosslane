//! Keeps `config.toml` equal to the running configuration when a
//! daemon-initiated reload (SIGHUP or the file watcher) is rejected because it
//! would leave an enabled proxy service without any available account.
//!
//! Those reloads have no force flag, so the rejected file is saved beside the
//! original and the last applied text is written back. Administrators re-apply
//! the saved edit explicitly with `crosslane config edit --force`.
//!
//! When that restore cannot happen, the daemon records a drift report that
//! `crosslane status` shows until the next successful apply clears it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use crosslane_ipc::protocol::ConfigDriftView;
use tracing::{error, warn};

use crate::state::AppState;

/// Restores the last applied configuration text and keeps the rejected file.
/// The caller holds the cross-process config file lock.
pub(crate) async fn restore_rejected(state: &Arc<AppState>, reason: &str) {
    let path = &state.paths.config_file;
    let saved = match save_rejected(path).await {
        Ok(saved) => Some(saved),
        Err(failure) => {
            error!(%reason, error = %format!("{failure:#}"), "could not back up the rejected config");
            None
        }
    };
    let Some(applied) = state.applied_config_text() else {
        error!(%reason, path = %path.display(),
            "config reload rejected, but the last applied file is unknown; disk and runtime differ");
        state.record_config_drift(applied_text_unknown(path, reason, saved.as_deref()));
        return;
    };
    // Never overwrite the only copy of the rejected edit.
    let Some(saved) = saved else {
        state.record_config_drift(restore_failed(
            path,
            reason,
            None,
            "backing up the rejected file failed, so it was left in place",
        ));
        return;
    };
    match restore(path, &applied).await {
        Ok(()) => warn!(%reason, saved = %saved.display(),
            "config reload rejected; restored the running configuration on disk. \
             Re-apply the saved file with `crosslane config edit --force` after review"),
        Err(failure) => {
            let failure = format!("{failure:#}");
            error!(%reason, error = %failure,
                "config reload rejected and restoring the running configuration failed");
            state.record_config_drift(restore_failed(path, reason, Some(&saved), &failure));
        }
    }
}

async fn save_rejected(path: &Path) -> Result<PathBuf> {
    let rejected = tokio::fs::read(path)
        .await
        .with_context(|| format!("read {}", path.display()))?;
    let saved = rejected_path(path);
    crosslane_store::atomic::write_bytes_atomically(&saved, &rejected, Some(0o600))
        .await
        .with_context(|| format!("save rejected config to {}", saved.display()))?;
    Ok(saved)
}

async fn restore(path: &Path, applied: &str) -> Result<()> {
    crosslane_store::atomic::write_bytes_atomically(path, applied.as_bytes(), Some(0o600))
        .await
        .with_context(|| format!("restore {}", path.display()))
}

fn restore_failed(
    path: &Path,
    reason: &str,
    saved: Option<&Path>,
    failure: &str,
) -> ConfigDriftView {
    let path = path.display();
    ConfigDriftView {
        kind: "restore_failed".into(),
        detail: format!(
            "{path} holds an edit the daemon rejected ({reason}); the running \
             configuration is the previous one. Writing it back failed: {failure}"
        ),
        resolution: format!(
            "Fix the file system problem first (disk space, permissions). Then either run \
             `crosslane config edit --force` to apply the edit in {path} anyway, or undo \
             the edit and run `crosslane config reload`. A successful reload clears this \
             warning. Do not restart the daemon before that: startup applies {path} \
             without the pool check."
        ),
        rejected_file: saved.map(|saved| saved.display().to_string()),
        detected_at: now_secs(),
    }
}

fn applied_text_unknown(path: &Path, reason: &str, saved: Option<&Path>) -> ConfigDriftView {
    let path = path.display();
    ConfigDriftView {
        kind: "applied_text_unknown".into(),
        detail: format!(
            "{path} holds an edit the daemon rejected ({reason}); the running \
             configuration is the previous one. The daemon lost its copy of that file \
             after a read error, so it did not write anything back."
        ),
        resolution: format!(
            "Either run `crosslane config edit --force` to apply the edit in {path} anyway, \
             or undo the edit in {path} and run `crosslane config reload`. A successful \
             reload clears this warning. Do not restart the daemon before that: startup \
             applies {path} without the pool check."
        ),
        rejected_file: saved.map(|saved| saved.display().to_string()),
        detected_at: now_secs(),
    }
}

fn rejected_path(path: &Path) -> PathBuf {
    // Unix milliseconds keep names unique and sortable without a date crate.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let name = path
        .file_name()
        .map_or_else(|| "config.toml".into(), |name| name.to_string_lossy());
    path.with_file_name(format!("{name}.rejected-{stamp}"))
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejected_file_is_saved_and_the_applied_text_is_restored() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("config.toml");
        tokio::fs::write(&path, "# rejected edit\n")
            .await
            .expect("write");
        let saved = save_rejected(&path).await.expect("save");
        restore(&path, "# applied\n").await.expect("restore");
        assert_eq!(
            tokio::fs::read_to_string(&path).await.expect("read"),
            "# applied\n"
        );
        assert_eq!(
            tokio::fs::read_to_string(&saved).await.expect("read saved"),
            "# rejected edit\n"
        );
        assert!(saved
            .file_name()
            .expect("name")
            .to_string_lossy()
            .starts_with("config.toml.rejected-"));
    }
}
