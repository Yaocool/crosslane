//! Transactional lifecycle for the independent status/admin listener.
use std::io::ErrorKind;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use crosslane_core::config::StatusHttpConfig;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::state::AppState;

struct Running {
    config: StatusHttpConfig,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

#[derive(Default)]
struct Inner {
    initialized: bool,
    running: Option<Running>,
    draining: Vec<JoinHandle<()>>,
}

#[derive(Default)]
pub(crate) struct ListenerManager {
    inner: Mutex<Inner>,
}

pub(crate) enum Prepared {
    Unchanged,
    Replace {
        config: StatusHttpConfig,
        listener: Option<TcpListener>,
    },
    Rebind(StatusHttpConfig),
}

impl ListenerManager {
    pub(crate) async fn initialize(&self, app: Arc<AppState>) -> Result<()> {
        let config = app.config.current().status_http.clone();
        let listener = bind(&config).await?;
        let mut inner = self.inner.lock().await;
        inner.initialized = true;
        inner.running = listener.map(|listener| start(app, config, listener));
        Ok(())
    }

    /// Reserve a replacement before changing runtime configuration. The caller
    /// serializes prepare/commit with AppState's configuration mutation lock.
    pub(crate) async fn prepare(&self, config: &StatusHttpConfig) -> Result<Prepared> {
        let inner = self.inner.lock().await;
        if !inner.initialized
            || (!config.enabled && inner.running.is_none())
            || inner
                .running
                .as_ref()
                .is_some_and(|running| running.config == *config && !running.task.is_finished())
        {
            return Ok(Prepared::Unchanged);
        }
        let old_port = inner.running.as_ref().map(|running| running.config.port);
        drop(inner);
        match bind(config).await {
            Ok(listener) => Ok(Prepared::Replace {
                config: config.clone(),
                listener,
            }),
            Err(error)
                if config.enabled && old_port == Some(config.port) && address_in_use(&error) =>
            {
                // Wildcard/loopback host changes on the same port overlap.
                // Rebind only after other fallible configuration work succeeds.
                Ok(Prepared::Rebind(config.clone()))
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn commit(&self, app: Arc<AppState>, prepared: Prepared) -> Result<()> {
        if matches!(prepared, Prepared::Unchanged) {
            return Ok(());
        }
        let mut inner = self.inner.lock().await;
        match prepared {
            Prepared::Unchanged => unreachable!(),
            Prepared::Replace { config, listener } => {
                let replacement = listener.map(|listener| start(app, config, listener));
                if let Some(previous) = inner.running.take() {
                    retire(&mut inner, previous);
                }
                inner.running = replacement;
                Ok(())
            }
            Prepared::Rebind(config) => {
                let previous = inner
                    .running
                    .take()
                    .context("status listener changed during reconfiguration")?;
                let previous_config = previous.config.clone();
                retire(&mut inner, previous);
                match bind_after_close(&config).await {
                    Ok(Some(listener)) => {
                        inner.running = Some(start(app, config, listener));
                        Ok(())
                    }
                    Ok(None) => unreachable!("rebind requires an enabled listener"),
                    Err(error) => {
                        match bind_after_close(&previous_config).await {
                            Ok(Some(listener)) => {
                                inner.running = Some(start(app, previous_config, listener));
                                Err(error.context("status listener rebind failed; previous listener restored"))
                            }
                            Ok(None) => unreachable!("previous listener was enabled"),
                            Err(rollback) => Err(anyhow!("status listener rebind failed ({error}); restoring previous listener also failed: {rollback}")),
                        }
                    }
                }
            }
        }
    }

    pub(crate) async fn run(&self, app: Arc<AppState>) -> Result<()> {
        let failure = loop {
            tokio::select! {
                biased;
                _ = app.shutdown.cancelled() => break None,
                _ = tokio::time::sleep(Duration::from_millis(500)) => {
                    if self.inner.lock().await.running.as_ref().is_some_and(|running| running.task.is_finished()) {
                        break Some(anyhow!("status HTTP listener stopped unexpectedly"));
                    }
                }
            }
        };
        app.shutdown.cancel();
        let draining = {
            let mut inner = self.inner.lock().await;
            if let Some(running) = inner.running.take() {
                retire(&mut inner, running);
            }
            std::mem::take(&mut inner.draining)
        };
        futures::future::join_all(draining).await;
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

async fn bind(config: &StatusHttpConfig) -> Result<Option<TcpListener>> {
    tokio::time::timeout(Duration::from_secs(10), super::bind(config))
        .await
        .context("status HTTP bind timed out")?
}

fn address_in_use(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == ErrorKind::AddrInUse)
    })
}

async fn bind_after_close(config: &StatusHttpConfig) -> Result<Option<TcpListener>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match bind(config).await {
            Err(error) if address_in_use(&error) && tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            result => return result,
        }
    }
}

fn start(app: Arc<AppState>, config: StatusHttpConfig, listener: TcpListener) -> Running {
    let cancel = app.shutdown.child_token();
    let stop = cancel.clone();
    let router = super::router_for_listener(app, config.clone(), cancel.clone());
    tracing::info!(host = %config.host, port = config.port, "status HTTP listening");
    let task = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router)
            .with_graceful_shutdown(stop.cancelled_owned())
            .await
        {
            tracing::error!(%error, "status HTTP listener failed");
        }
    });
    Running {
        config,
        cancel,
        task,
    }
}

fn retire(inner: &mut Inner, mut running: Running) {
    running.cancel.cancel();
    inner.draining.retain(|task| !task.is_finished());
    inner.draining.push(tokio::spawn(async move {
        if tokio::time::timeout(Duration::from_secs(30), &mut running.task)
            .await
            .is_err()
        {
            running.task.abort();
            let _ = running.task.await;
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use crosslane_core::config::Config;
    use crosslane_core::paths::Paths;
    use crosslane_store::accounts::AccountStore;
    use crosslane_store::config_loader::ConfigHandle;
    use tower::ServiceExt;

    async fn app(port: u16) -> (tempfile::TempDir, Arc<AppState>) {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::from_env_values(Some(home.path().to_str().unwrap()), None, None, None);
        let accounts = AccountStore::load(&paths.accounts_file).await.unwrap();
        let mut config = Config::default();
        config.status_http.enabled = true;
        config.status_http.port = port;
        (
            home,
            Arc::new(AppState::new(paths, ConfigHandle::new(config), accounts)),
        )
    }

    #[tokio::test]
    async fn failed_rebind_restores_the_previous_listener() {
        let previous_port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let (_home, app) = app(previous_port).await;
        app.status_http.initialize(Arc::clone(&app)).await.unwrap();
        let occupied = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let mut target = app.config.current().status_http.clone();
        target.port = occupied.local_addr().unwrap().port();
        // Simulate another process winning the bind after preflight succeeded.
        let error = app
            .status_http
            .commit(Arc::clone(&app), Prepared::Rebind(target))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("previous listener restored"));
        assert_eq!(
            reqwest::get(format!("http://127.0.0.1:{previous_port}/health"))
                .await
                .unwrap()
                .status(),
            200
        );
        app.shutdown.cancel();
        app.status_http.run(Arc::clone(&app)).await.unwrap();
    }

    #[tokio::test]
    async fn retired_listener_routes_cannot_use_new_configuration() {
        let (_home, app) = app(7651).await;
        let old = app.config.current().status_http.clone();
        let stopped = CancellationToken::new();
        let retired =
            super::super::router_for_listener(Arc::clone(&app), old.clone(), stopped.clone());
        let mut next = app.config.current().as_ref().clone();
        next.status_http.port += 1;
        app.config.replace(next.clone());
        for path in ["/admin/v1/providers", "/health"] {
            let response = retired
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            assert_eq!(response.headers()["cache-control"], "no-store");
        }
        next.status_http = old;
        app.config.replace(next.clone());
        stopped.cancel();
        assert_eq!(
            retired
                .oneshot(
                    Request::builder()
                        .uri("/health")
                        .body(Body::empty())
                        .unwrap()
                )
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
        let response = super::super::router_for_listener(
            Arc::clone(&app),
            next.status_http,
            CancellationToken::new(),
        )
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn accepted_requests_authenticate_against_their_listener_snapshot() {
        use sha2::{Digest, Sha256};

        let (_home, app) = app(7651).await;
        let mut old = app.config.current().as_ref().clone();
        old.admin.rest.enabled = true;
        old.admin.rest.token_sha256 = format!("{:x}", Sha256::digest(b"old-listener-token"));
        let admitted = super::super::ListenerConfig(Arc::new(old.clone()));
        let mut next = old;
        next.status_http.port += 1;
        next.admin.rest.token_sha256 = format!("{:x}", Sha256::digest(b"new-listener-token"));
        app.config.replace(next);

        // Model a listener switch between the outer admission guard and the
        // inner authentication middleware, including an otherwise valid new key.
        let router = super::super::router(Arc::clone(&app));
        for (token, expected) in [
            ("new-listener-token", StatusCode::UNAUTHORIZED),
            ("old-listener-token", StatusCode::OK),
        ] {
            let request = Request::builder()
                .uri("/admin/v1/openapi.json")
                .header("authorization", format!("Bearer {token}"))
                .extension(admitted.clone())
                .body(Body::empty())
                .unwrap();
            let response = router.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), expected);
        }
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/admin/v1/openapi.json")
                    .header("authorization", "Bearer new-listener-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
