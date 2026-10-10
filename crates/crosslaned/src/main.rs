//! crosslane 常驻服务。

mod admin;
mod alerts;
mod http;
mod logging;
mod meter;
mod provider_routing;
mod providers;
mod sso;
mod state;
mod stats;
mod tasks;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use crosslane_core::paths::Paths;
use crosslane_store::bootstrap::ensure_layout;
use crosslane_store::config_loader::{
    load_config, merge_hot_reload, spawn_config_watcher_with_hook, ConfigHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::state::AppState;

const CONFIG_DEBOUNCE: Duration = Duration::from_millis(500);

fn build_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION
        .get_or_init(|| {
            format!(
                "{}\nfeatures: crs={} sso={}",
                env!("CARGO_PKG_VERSION"),
                cfg!(feature = "crs"),
                cfg!(feature = "sso")
            )
        })
        .as_str()
}

#[derive(Debug, Parser)]
#[command(name = "crosslaned", version, long_version = build_version(), about = "crosslane 常驻服务")]
struct Cli {
    /// 配置文件路径，默认按 XDG 解析。
    #[arg(long, value_name = "PATH")]
    config: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    crosslane_store::environment::load_dotenv()?;
    let cli = Cli::parse();
    let mut paths = Paths::from_env();
    if let Some(path) = cli.config {
        paths.config_file = path.into();
    }

    let report = ensure_layout(&paths).await?;
    let config = load_config(&paths.config_file).await?;
    config.validate().context("configuration is invalid")?;
    logging::init(
        &config.log,
        paths.data_dir.join("logs").join("crosslaned.log"),
    )?;

    info!(
        ide_version = crosslane_kiro::identity::IDE_VERSION,
        cli_version = crosslane_kiro::identity::CLI_VERSION,
        "Kiro upstream client identity"
    );

    if report.created_anything() {
        info!(config = %paths.config_file.display(), "wrote default configuration and data files");
    }

    let accounts = crosslane_store::accounts::AccountStore::load(&paths.accounts_file).await?;
    if accounts.is_empty() {
        info!("no accounts configured yet; add one with `crosslane account import`");
    }

    let config_handle = ConfigHandle::new(config);
    let socket_path = config_handle.current().admin.socket.clone().into();
    let state = Arc::new(AppState::load(paths, config_handle, accounts).await?);
    let reload_state = Arc::clone(&state);
    let reload_runtime = tokio::runtime::Handle::current();
    let _watcher = spawn_config_watcher_with_hook(
        state.paths.config_file.clone(),
        state.config.clone(),
        CONFIG_DEBOUNCE,
        Arc::new(move |config| {
            reload_runtime.block_on(async {
                reload_state.apply_config_transaction(config).await?;
                reload_state.mark_config_reloaded(now_secs());
                Ok(())
            })
        }),
    )?;
    let shutdown = state.shutdown.clone();

    spawn_signal_handler(Arc::clone(&state), shutdown.clone());
    tasks::spawn(Arc::clone(&state), shutdown.clone());
    let initial_config = state.config.current();
    let failures = state.reconcile_proxy_services(&initial_config).await;
    for (service_id, error) in failures {
        warn!(%service_id, %error, "configured proxy service is not running");
    }
    let admin = admin::server::serve(Arc::clone(&state), socket_path, shutdown.clone());
    let result = admin.await;
    shutdown.cancel();
    tasks::flush_before_shutdown(&state).await;
    result
}

#[cfg(unix)]
fn spawn_signal_handler(state: Arc<AppState>, shutdown: CancellationToken) {
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};

        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(stream) => stream,
            Err(error) => {
                warn!(error = %error, "failed to register SIGTERM handler");
                return;
            }
        };
        let mut hangup = match signal(SignalKind::hangup()) {
            Ok(stream) => stream,
            Err(error) => {
                warn!(error = %error, "failed to register SIGHUP handler");
                return;
            }
        };

        loop {
            tokio::select! {
                result = tokio::signal::ctrl_c() => {
                    if let Err(error) = result {
                        warn!(error = %error, "failed to listen for Ctrl-C");
                    }
                    shutdown.cancel();
                    break;
                }
                _ = terminate.recv() => {
                    shutdown.cancel();
                    break;
                }
                _ = hangup.recv() => reload_after_sighup(&state).await,
            }
        }
    });
}

#[cfg(unix)]
async fn reload_after_sighup(state: &Arc<AppState>) {
    let _file_lock =
        match crosslane_store::atomic::lock_file_exclusive(&state.paths.config_file).await {
            Ok(lock) => lock,
            Err(error) => {
                warn!(%error, "failed to lock config before SIGHUP reload");
                return;
            }
        };
    match load_config(&state.paths.config_file).await {
        Ok(next) => match next.validate() {
            Ok(()) => {
                let current = state.config.current();
                let (next, needs_restart) = merge_hot_reload(&current, next);
                for field in needs_restart {
                    warn!(field, "SIGHUP field requires restart to take effect");
                }
                match state.apply_config_transaction(&next).await {
                    Ok(()) => {
                        state.mark_config_reloaded(now_secs());
                        info!("configuration reloaded after SIGHUP");
                    }
                    Err(error) => {
                        warn!(%error, "SIGHUP config apply failed; previous config restored")
                    }
                }
            }
            Err(error) => {
                warn!(error = %error, "SIGHUP config validation failed; keeping old config")
            }
        },
        Err(error) => warn!(error = %error, "SIGHUP config load failed; keeping old config"),
    }
}

#[cfg(not(unix))]
fn spawn_signal_handler(_state: Arc<AppState>, shutdown: CancellationToken) {
    tokio::spawn(async move {
        let _result = tokio::signal::ctrl_c().await;
        shutdown.cancel();
    });
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}
