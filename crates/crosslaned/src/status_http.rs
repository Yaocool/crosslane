//! Independent HTTP listener for public status and authenticated administration.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use crosslane_core::config::{ProxyServiceConfig, StatusHttpConfig};
use serde_json::{json, Value};
use tokio::net::TcpListener;

use crate::state::AppState;

pub(crate) async fn bind(config: &StatusHttpConfig) -> Result<Option<TcpListener>> {
    if !config.enabled {
        return Ok(None);
    }
    let host = config.host.trim_start_matches('[').trim_end_matches(']');
    let listener = TcpListener::bind((host, config.port))
        .await
        .with_context(|| format!("bind status HTTP {}:{}", config.host, config.port))?;
    Ok(Some(listener))
}

pub(crate) async fn serve(state: Arc<AppState>, listener: Option<TcpListener>) -> Result<()> {
    let shutdown = state.shutdown.clone();
    let Some(listener) = listener else {
        shutdown.cancelled().await;
        return Ok(());
    };
    tracing::info!(address = %listener.local_addr()?, "status HTTP listening");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
        .context("serve status HTTP")
}

pub(crate) fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/health", get(health))
        .route("/ready", get(readiness))
        .with_state(Arc::clone(&state))
        .nest("/admin/v1", crate::admin::rest::router(state))
}

async fn root() -> Json<Value> {
    Json(json!({"name":"crosslane","status":"ok","version":env!("CARGO_PKG_VERSION")}))
}

#[derive(Default)]
struct AccountHealth {
    provider_scope: Vec<String>,
    total: usize,
    available: usize,
    protected: usize,
    cooling: usize,
    exhausted: usize,
    banned: usize,
    refreshing: usize,
    disabled: usize,
    unavailable: usize,
    errors: Vec<String>,
}

enum AccountScope<'a> {
    Untagged,
    Service(&'a ProxyServiceConfig),
}

impl AccountScope<'_> {
    fn includes(&self, provider: &str, id: &str, tags: &[String]) -> bool {
        match self {
            Self::Untagged => tags.is_empty(),
            Self::Service(service) => service.includes_provider_account(provider, id, tags),
        }
    }
}

/// Read local snapshots only: probes never refresh tokens or contact upstreams.
async fn account_health(state: &Arc<AppState>, scope: AccountScope<'_>) -> AccountHealth {
    let config = state.config.current();
    let providers = config.effective_providers();
    let allowed = match &scope {
        AccountScope::Service(service) => config
            .allowed_providers_for_service(service)
            .into_iter()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>(),
        AccountScope::Untagged => providers
            .iter()
            .map(|provider| provider.id.clone())
            .collect(),
    };
    let mut health = AccountHealth {
        provider_scope: allowed.iter().cloned().collect(),
        ..AccountHealth::default()
    };
    for provider in providers
        .iter()
        .filter(|provider| allowed.contains(&provider.id))
    {
        match provider.kind.as_str() {
            "kiro" => {
                let pool = state.pool();
                let accounts = pool
                    .snapshot()
                    .await
                    .into_iter()
                    .filter(|account| scope.includes(&provider.id, &account.id, &account.tags))
                    .collect::<Vec<_>>();
                health.total += accounts.len();
                if provider.enabled {
                    let ids = accounts
                        .iter()
                        .map(|account| account.id.clone())
                        .collect::<HashSet<_>>();
                    let counts = pool.scheduling_counts_scoped(&ids).await;
                    health.available += counts.available;
                    health.protected += counts.protected;
                    health.cooling += counts.cooling;
                    health.exhausted += counts.exhausted;
                    health.banned += counts.banned;
                    health.refreshing += counts.refreshing;
                    health.disabled += counts.disabled;
                } else {
                    health.disabled += accounts.len();
                }
            }
            "copilot" => {
                let id = match crosslane_core::provider::ProviderId::parse(provider.id.clone()) {
                    Ok(id) => id,
                    Err(error) => {
                        health.errors.push(error.to_string());
                        continue;
                    }
                };
                let Some(runtime) = state.providers.copilot(&id).await else {
                    health
                        .errors
                        .push(format!("provider {} runtime is unavailable", provider.id));
                    continue;
                };
                for account in runtime.account_summaries().await {
                    if !scope.includes(&provider.id, &account.id, &account.tags) {
                        continue;
                    }
                    health.total += 1;
                    if !provider.enabled || !account.enabled {
                        health.disabled += 1;
                    } else if account.is_available() {
                        health.available += 1;
                    } else {
                        health.unavailable += 1;
                    }
                }
            }
            kind if provider.enabled => health.errors.push(format!(
                "provider {} uses unavailable driver {kind}",
                provider.id
            )),
            _ => {}
        }
    }
    health
}

async fn health(State(state): State<Arc<AppState>>) -> Json<Value> {
    let accounts = account_health(&state, AccountScope::Untagged).await;
    let views = state.proxy_services.views(&state.config.current()).await;
    // Keep public service state explicit; serializing the full view exposes IDs and bindings.
    let services = views
        .into_iter()
        .map(|service| {
            json!({
                "name":service.name,
                "enabled":service.enabled,
                "running":service.running
            })
        })
        .collect::<Vec<_>>();
    Json(json!({
        "status":"ok",
        "total_accounts":accounts.total,
        "available_accounts":accounts.available,
        "protected_accounts":accounts.protected,
        "cooling_accounts":accounts.cooling,
        "exhausted_accounts":accounts.exhausted,
        "banned_accounts":accounts.banned,
        "refreshing_accounts":accounts.refreshing,
        "disabled_accounts":accounts.disabled,
        "unavailable_accounts":accounts.unavailable,
        "services":services
    }))
}

async fn readiness(State(state): State<Arc<AppState>>) -> Response {
    let config = state.config.current();
    let views = state.proxy_services.views(&config).await;
    let mut reasons = state.task_registry.readiness_issues(&state);
    let enabled = config
        .proxy_service
        .iter()
        .filter(|service| service.enabled)
        .collect::<Vec<_>>();
    if enabled.is_empty() {
        reasons.push("no enabled proxy service is configured".to_string());
    }
    let mut services = Vec::new();
    for service in enabled {
        let running = views
            .iter()
            .any(|view| view.id == service.id && view.running);
        if !running {
            reasons.push(format!("proxy service {} is not running", service.name));
        }
        let accounts = account_health(&state, AccountScope::Service(service)).await;
        if accounts.available == 0 {
            reasons.push(format!(
                "proxy service {} has no available account in its provider scope",
                service.name
            ));
        }
        reasons.extend(accounts.errors.iter().cloned());
        services.push(json!({
            "service_id":service.id,
            "service_name":service.name,
            "running":running,
            "provider_scope":accounts.provider_scope,
            "available_accounts":accounts.available,
            "unavailable_accounts":accounts.unavailable,
            "provider_errors":accounts.errors
        }));
    }
    if let Some(error) = state.meter.recovery_error() {
        reasons.push(format!("metering recovery required: {error}"));
    }
    let ready = reasons.is_empty();
    (
        if ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(json!({
            "status":if ready { "ready" } else { "not_ready" },
            "ready":ready,
            "scope":"process",
            "reasons":reasons,
            "services":services,
            "uptime_secs":state.uptime_secs()
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disabled_listener_does_not_bind_and_enabled_bind_errors_are_reported() {
        let occupied = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let mut config = StatusHttpConfig {
            port: occupied.local_addr().unwrap().port(),
            ..StatusHttpConfig::default()
        };
        assert!(bind(&config).await.unwrap().is_none());
        config.enabled = true;
        let error = bind(&config).await.unwrap_err();
        assert!(error.to_string().contains("bind status HTTP"), "{error}");
        drop(occupied);
        assert!(bind(&config).await.unwrap().is_some());
    }
}
