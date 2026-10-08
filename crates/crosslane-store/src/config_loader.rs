//! 配置加载与文件监听热重载。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use crosslane_core::config::Config;
use notify::{RecursiveMode, Watcher};
use notify_debouncer_full::{new_debouncer, Debouncer, FileIdMap};
use tracing::{error, info, warn};

use crate::atomic::{is_missing, lock_file_exclusive_blocking, read_to_string_with_retry};

/// Synchronous runtime validation/application hook used by the file watcher.
pub type ConfigApplyHook = Arc<dyn Fn(&Config) -> std::result::Result<(), String> + Send + Sync>;

/// 一次重载的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReloadOutcome {
    /// 新配置是否已应用。
    pub applied: bool,
    /// 失败原因。
    pub error: Option<String>,
    /// 需要重启才能真正生效的字段。
    pub needs_restart: Vec<&'static str>,
}

/// 加载配置；文件不存在时返回默认配置。
pub async fn load_config(path: &Path) -> Result<Config> {
    let raw = match read_to_string_with_retry(path).await {
        Ok(raw) => raw,
        Err(error) if is_missing(&error) => return Ok(Config::default()),
        Err(error) => return Err(error),
    };
    parse_config(&raw, path)
}

fn parse_config(raw: &str, path: &Path) -> Result<Config> {
    toml::from_str(raw).with_context(|| format!("parse {}", path.display()))
}

/// 共享配置句柄。
#[derive(Debug, Clone)]
pub struct ConfigHandle {
    inner: Arc<RwLock<Arc<Config>>>,
}

impl ConfigHandle {
    /// 用初始配置构造。
    pub fn new(config: Config) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Arc::new(config))),
        }
    }

    /// 获取当前配置快照。
    pub fn current(&self) -> Arc<Config> {
        match self.inner.read() {
            Ok(guard) => Arc::clone(&guard),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    /// 整体替换配置。
    pub fn replace(&self, config: Config) {
        let next = Arc::new(config);
        match self.inner.write() {
            Ok(mut guard) => *guard = next,
            Err(poisoned) => *poisoned.into_inner() = next,
        }
    }
}

/// 返回需要重启才能生效的字段。
pub fn diff_restart_required(old: &Config, new: &Config) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if old.admin.socket != new.admin.socket {
        fields.push("admin.socket");
    }
    if old.server.tls.enabled != new.server.tls.enabled {
        fields.push("server.tls.enabled");
    }
    fields
}

/// 合并一次热重载：热更新字段取新值，管理 socket 与 TLS 监听模式保留运行值。
///
/// 返回的字段列表提示调用方在下次进程重启后，磁盘中的新值才会生效。
pub fn merge_hot_reload(old: &Config, mut new: Config) -> (Config, Vec<&'static str>) {
    let needs_restart = diff_restart_required(old, &new);
    new.admin.socket.clone_from(&old.admin.socket);
    if old.server.tls.enabled != new.server.tls.enabled {
        new.server.tls = old.server.tls.clone();
    }
    (new, needs_restart)
}

/// 文件监听句柄，drop 后停止监听。
pub struct ConfigWatcher {
    _debouncer: Debouncer<notify::RecommendedWatcher, FileIdMap>,
    poll_stop: Arc<AtomicBool>,
    _poller: std::thread::JoinHandle<()>,
}

impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        self.poll_stop.store(true, Ordering::Release);
    }
}

/// 启动配置文件监听，变更后防抖重载。
///
/// 解析或校验失败时保留旧配置继续服务。
pub fn spawn_config_watcher(
    path: PathBuf,
    handle: ConfigHandle,
    debounce: Duration,
) -> Result<ConfigWatcher> {
    spawn_config_watcher_with_hook(path, handle, debounce, Arc::new(|_| Ok(())))
}

/// 启动配置监听，并在提交配置前同步通知运行时组件。
///
/// hook 返回错误时保留原配置，避免文件解析成功但运行时监听启动失败后仍
/// 把新配置报告为已应用。
pub fn spawn_config_watcher_with_hook(
    path: PathBuf,
    handle: ConfigHandle,
    debounce: Duration,
    hook: ConfigApplyHook,
) -> Result<ConfigWatcher> {
    let watch_target = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let reload_path = path.clone();
    let reload_watch_target = watch_target.clone();
    let event_handle = handle.clone();
    let event_hook = Arc::clone(&hook);
    let last_seen_contents = Arc::new(Mutex::new(std::fs::read(&path).ok()));
    let event_last_seen_contents = Arc::clone(&last_seen_contents);
    let mut debouncer = new_debouncer(
        debounce,
        None,
        move |result: notify_debouncer_full::DebounceEventResult| match result {
            Ok(events) => {
                if events_affect_config(&events, &reload_path, &reload_watch_target) {
                    let _outcome = apply_reload_if_contents_changed(
                        &reload_path,
                        &event_handle,
                        &event_hook,
                        &event_last_seen_contents,
                    );
                }
            }
            Err(errors) => {
                for watcher_error in errors {
                    warn!(error = %watcher_error, "config watcher error");
                }
            }
        },
    )
    .context("create config debouncer")?;

    debouncer
        .watcher()
        .watch(&watch_target, RecursiveMode::NonRecursive)
        .with_context(|| format!("watch {}", watch_target.display()))?;
    debouncer
        .cache()
        .add_root(&watch_target, RecursiveMode::NonRecursive);

    // 原生 watcher 在部分容器 bind mount、网络文件系统与受限沙箱中
    // 不产生事件。低频内容轮询作为自动兜底；SIGHUP/RPC 仍可手动重载。
    let poll_stop = Arc::new(AtomicBool::new(false));
    let poller_stop = Arc::clone(&poll_stop);
    let poll_path = path;
    let poll_handle = handle;
    let poll_hook = hook;
    let poll_last_seen_contents = Arc::clone(&last_seen_contents);
    let poll_interval = if debounce < Duration::from_millis(50) {
        Duration::from_millis(50)
    } else {
        debounce
    };
    let poller = std::thread::Builder::new()
        .name("crosslane-config-poller".into())
        .spawn(move || {
            while !poller_stop.load(Ordering::Acquire) {
                std::thread::sleep(poll_interval);
                let _outcome = apply_reload_if_contents_changed(
                    &poll_path,
                    &poll_handle,
                    &poll_hook,
                    &poll_last_seen_contents,
                );
            }
        })
        .context("spawn config polling fallback")?;

    Ok(ConfigWatcher {
        _debouncer: debouncer,
        poll_stop,
        _poller: poller,
    })
}

fn events_affect_config(
    events: &[notify_debouncer_full::DebouncedEvent],
    config_path: &Path,
    watch_target: &Path,
) -> bool {
    events.iter().any(|event| {
        event.paths.is_empty()
            || event
                .paths
                .iter()
                .any(|path| path == config_path || path == watch_target)
    })
}

fn apply_reload_if_contents_changed(
    path: &Path,
    handle: &ConfigHandle,
    hook: &ConfigApplyHook,
    last_seen_contents: &Mutex<Option<Vec<u8>>>,
) -> ReloadOutcome {
    // Cooperating writers keep this lock for their complete read-modify-write
    // and runtime-apply transaction. Waiting here prevents the watcher from
    // applying an intermediate or stale snapshot while such a transaction is
    // still in progress.
    let _file_lock = match lock_file_exclusive_blocking(path) {
        Ok(lock) => lock,
        Err(lock_error) => {
            error!(error = %lock_error, path = %path.display(), "cannot lock config for reload");
            return ReloadOutcome {
                error: Some(lock_error.to_string()),
                ..ReloadOutcome::default()
            };
        }
    };
    let current = std::fs::read(path).ok();
    {
        let mut last_seen = last_seen_contents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *last_seen == current {
            return ReloadOutcome::default();
        }
        *last_seen = current;
    }
    apply_reload(path, handle, hook)
}

fn apply_reload(path: &Path, handle: &ConfigHandle, hook: &ConfigApplyHook) -> ReloadOutcome {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(read_error) => {
            error!(error = %read_error, path = %path.display(), "cannot reload config");
            return ReloadOutcome {
                error: Some(read_error.to_string()),
                ..ReloadOutcome::default()
            };
        }
    };
    let next = match parse_config(&raw, path) {
        Ok(config) => config,
        Err(parse_error) => {
            error!(error = %parse_error, "config syntax invalid; keeping current config");
            return ReloadOutcome {
                error: Some(parse_error.to_string()),
                ..ReloadOutcome::default()
            };
        }
    };
    if let Err(validation_error) = next.validate() {
        error!(error = %validation_error, "config validation failed; keeping current config");
        return ReloadOutcome {
            error: Some(validation_error.to_string()),
            ..ReloadOutcome::default()
        };
    }

    let current = handle.current();
    let (next, needs_restart) = merge_hot_reload(&current, next);
    for field in &needs_restart {
        warn!(
            field = field,
            "config field requires restart to take effect"
        );
    }
    if let Err(apply_error) = hook(&next) {
        error!(error = %apply_error, "runtime config apply failed; keeping current config");
        return ReloadOutcome {
            applied: false,
            error: Some(apply_error),
            needs_restart,
        };
    }
    handle.replace(next);
    info!(path = %path.display(), "config reloaded");
    ReloadOutcome {
        applied: true,
        error: None,
        needs_restart,
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn missing_and_partial_configs_use_defaults() {
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("config.toml");
        assert_eq!(load_config(&path).await.expect("missing").server.port, 5580);
        tokio::fs::write(&path, "[server]\nport = 6001\n")
            .await
            .expect("write");
        let config = load_config(&path).await.expect("partial");
        assert_eq!(config.server.port, 6001);
        assert_eq!(config.pool.max_concurrent_per_account, 50);
    }

    #[tokio::test]
    async fn invalid_config_has_path_context() {
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("config.toml");
        tokio::fs::write(&path, "[server\nport = ")
            .await
            .expect("write");
        let error = load_config(&path).await.expect_err("invalid");
        assert!(error.to_string().contains("config.toml"));
    }

    #[test]
    fn handle_swaps_and_restart_diff_is_precise() {
        let handle = ConfigHandle::new(Config::default());
        let mut next = Config::default();
        next.server.port = 7000;
        assert!(diff_restart_required(&handle.current(), &next).is_empty());
        handle.replace(next);
        assert_eq!(handle.current().server.port, 7000);

        let old = Config::default();
        let mut hot = Config::default();
        hot.features.enable_prompt_cache = true;
        hot.pool.max_concurrent_per_account = 20;
        hot.log.level = "debug".into();
        assert!(diff_restart_required(&old, &hot).is_empty());
        let (hot, restart) = merge_hot_reload(&old, hot);
        assert!(restart.is_empty());
        assert_eq!(hot.log.level, "debug");

        let mut mixed = Config::default();
        mixed.server.host = "0.0.0.0".into();
        mixed.server.port = 6000;
        mixed.admin.socket = "/tmp/new.sock".into();
        mixed.features.enable_prompt_cache = true;
        mixed.log.format = "pretty".into();
        let (merged, restart) = merge_hot_reload(&old, mixed);
        assert_eq!(merged.server.host, "0.0.0.0");
        assert_eq!(merged.server.port, 6000);
        assert_eq!(merged.admin.socket, old.admin.socket);
        assert_eq!(merged.log.format, "pretty");
        assert!(merged.features.enable_prompt_cache);
        assert_eq!(restart, vec!["admin.socket"]);

        let mut certificate = Config::default();
        certificate.server.tls.cert = Some("new certificate".into());
        let (certificate, restart) = merge_hot_reload(&old, certificate);
        assert!(restart.is_empty());
        assert_eq!(
            certificate.server.tls.cert.as_deref(),
            Some("new certificate")
        );

        let mut mode = Config::default();
        mode.server.tls.enabled = true;
        let (mode, restart) = merge_hot_reload(&old, mode);
        assert_eq!(restart, vec!["server.tls.enabled"]);
        assert!(!mode.server.tls.enabled);
    }

    #[test]
    fn runtime_hook_failure_keeps_previous_config() {
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("config.toml");
        std::fs::write(&path, "[server]\nport = 6100\n").expect("edit");
        let handle = ConfigHandle::new(Config::default());
        let hook: ConfigApplyHook = Arc::new(|_| Err("listener bind failed".into()));
        let outcome = apply_reload(&path, &handle, &hook);

        assert!(!outcome.applied);
        assert_eq!(outcome.error.as_deref(), Some("listener bind failed"));
        assert_eq!(handle.current().server.port, 5580);
    }

    #[tokio::test]
    async fn watcher_applies_valid_edit_and_rejects_invalid_one() {
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("config.toml");
        tokio::fs::write(
            &path,
            "[server]\nport = 5580\n\n[features]\nenable_prompt_cache = false\n",
        )
        .await
        .expect("seed");
        let handle = ConfigHandle::new(load_config(&path).await.expect("load"));
        let _watcher =
            spawn_config_watcher(path.clone(), handle.clone(), Duration::from_millis(50))
                .expect("watcher");

        // macOS FSEvents 的后台 stream 可能在 watch() 返回后短暂初始化。
        tokio::time::sleep(Duration::from_millis(200)).await;

        tokio::fs::write(
            &path,
            "[server]\nport = 5580\n\n[features]\nenable_prompt_cache = true\n",
        )
        .await
        .expect("edit");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !handle.current().features.enable_prompt_cache {
            assert!(std::time::Instant::now() < deadline, "reload timeout");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        tokio::fs::write(&path, "[server\nport = ")
            .await
            .expect("break");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(handle.current().features.enable_prompt_cache);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watcher_waits_for_an_active_config_transaction() {
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("config.toml");
        tokio::fs::write(&path, "[server]\nport = 5580\n")
            .await
            .expect("seed");
        let handle = ConfigHandle::new(load_config(&path).await.expect("load"));
        let _watcher =
            spawn_config_watcher(path.clone(), handle.clone(), Duration::from_millis(50))
                .expect("watcher");
        tokio::time::sleep(Duration::from_millis(200)).await;

        let transaction = crate::atomic::lock_file_exclusive(&path)
            .await
            .expect("lock config transaction");
        tokio::fs::write(&path, "[server]\nport = 6100\n")
            .await
            .expect("edit while locked");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            handle.current().server.port,
            5580,
            "watcher applied the file before the transaction released its lock"
        );

        drop(transaction);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while handle.current().server.port != 6100 {
            assert!(std::time::Instant::now() < deadline, "reload timeout");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn watcher_ignores_sibling_files_and_unchanged_config_contents() {
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join("config.toml");
        let initial = "[server]\nport = 5580\n";
        tokio::fs::write(&path, initial).await.expect("seed");
        let handle = ConfigHandle::new(load_config(&path).await.expect("load"));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hook_calls = Arc::clone(&calls);
        let hook: ConfigApplyHook = Arc::new(move |_| {
            hook_calls.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });
        let _watcher =
            spawn_config_watcher_with_hook(path.clone(), handle, Duration::from_millis(50), hook)
                .expect("watcher");
        tokio::time::sleep(Duration::from_millis(200)).await;

        tokio::fs::write(directory.path().join("accounts.json"), "[]")
            .await
            .expect("sibling edit");
        tokio::fs::write(&path, initial)
            .await
            .expect("unchanged config edit");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(calls.load(Ordering::Relaxed), 0);

        tokio::fs::write(&path, "[server]\nport = 6001\n")
            .await
            .expect("config edit");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while calls.load(Ordering::Relaxed) == 0 {
            assert!(std::time::Instant::now() < deadline, "reload timeout");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}
