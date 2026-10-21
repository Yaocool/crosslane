//! Public business HTTP plane.

pub(crate) mod client_context;
#[cfg(test)]
mod client_context_tests;
mod context_recovery;
#[cfg(feature = "crs")]
pub(crate) mod crs;
mod handlers;
#[cfg(test)]
mod model_scope_tests;
pub(crate) use handlers::fallback_models;
pub(crate) mod compaction_replay;
pub(crate) mod prompt_cache;
mod response;
pub(crate) mod responses;
pub(crate) mod stream;
mod timing;
mod tool_names;
mod usage;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use axum::extract::{DefaultBodyLimit, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::FutureExt;
use tracing::Instrument;
use uuid::Uuid;

use crosslane_core::config::{ProxyServiceConfig, ServiceAccountTags};
use crosslane_ipc::protocol::ProxyServiceView;

use crate::state::AppState;

pub(crate) const TRACE_ID_HEADER: &str = "x-trace-id";
pub(crate) const REQUEST_ID_HEADER: &str = "request-id";
const MAX_LOGGED_USER_AGENT_CHARS: usize = 256;

#[derive(Clone)]
pub(crate) struct RequestTrace {
    pub id: String,
}

/// Router state scoped to one configured proxy service.
#[derive(Clone)]
pub(crate) struct ServiceHttpState {
    pub app: Arc<AppState>,
    pub service: Arc<ProxyServiceConfig>,
    pub allowed_api_key_ids: Arc<HashSet<String>>,
}

impl ServiceHttpState {
    /// Pool membership edits apply without restarting the listener.
    pub fn current_service(&self) -> ProxyServiceConfig {
        self.app
            .config
            .current()
            .proxy_service
            .iter()
            .find(|service| service.id == self.service.id)
            .cloned()
            .unwrap_or_else(|| self.service.as_ref().clone())
    }

    /// Resolve the service's effective account pool for one request. Keeping a
    /// fixed ID snapshot prevents retries and internal subrequests from
    /// escaping the service pool if accounts change mid-request.
    pub async fn account_ids(&self) -> Arc<HashSet<String>> {
        // Manual inclusions and exclusions are control-plane-only changes and
        // must not restart the listening socket. Resolve the latest service
        // snapshot by stable ID so new requests observe those edits while
        // already accepted requests keep their fixed account ID set.
        let service = self.current_service();
        Arc::new(
            self.app
                .pool()
                .matching_account_ids(|account| service.includes_account(account))
                .await,
        )
    }
}

pub(crate) fn request_trace_id(request: &axum::extract::Request) -> String {
    request
        .extensions()
        .get::<RequestTrace>()
        .map(|trace| trace.id.clone())
        .unwrap_or_else(|| format!("trace_{}", Uuid::new_v4().simple()))
}

pub(crate) fn logged_user_agent(headers: &axum::http::HeaderMap) -> String {
    let Some(value) = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return "unknown".into();
    };
    let mut chars = value.chars();
    let mut agent = chars
        .by_ref()
        .take(MAX_LOGGED_USER_AGENT_CHARS)
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>();
    if chars.next().is_some() {
        agent.pop();
        agent.push('…');
    }
    // User-Agent is client-controlled. Avoid echoing credentials accidentally
    // placed in it while retaining the product/version for ordinary clients.
    let lower = agent.to_ascii_lowercase();
    if ["sk-", "ksk_", "bearer ", "token=", "api_key="]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return "[redacted]".into();
    }
    agent
}

#[cfg(test)]
pub fn router(state: Arc<AppState>) -> Router {
    let config = state.config.current();
    let service = ProxyServiceConfig {
        id: "test".into(),
        name: "test".into(),
        host: config.server.host.clone(),
        port: config.server.port,
        enabled: true,
        skip_user_agent_check: false,
        api_key_ids: config
            .api_key
            .iter()
            .filter_map(|key| key.id.clone())
            .collect(),
        account_tag: None,
        account_ids: Vec::new(),
        excluded_account_ids: Vec::new(),
        created_at: 0,
        ..ProxyServiceConfig::default()
    };
    router_for_service(state, service, false)
}

fn router_for_service(
    state: Arc<AppState>,
    service: ProxyServiceConfig,
    enforce_service_keys: bool,
) -> Router {
    let mut allowed_api_key_ids = service.api_key_ids.iter().cloned().collect::<HashSet<_>>();
    if !enforce_service_keys {
        allowed_api_key_ids.clear();
    }
    let router_state = ServiceHttpState {
        app: state,
        service: Arc::new(service),
        allowed_api_key_ids: Arc::new(allowed_api_key_ids),
    };
    let keep_alive_state = router_state.clone();
    let trace_state = router_state.clone();
    Router::new()
        .route("/", get(handlers::root))
        .route("/health", get(handlers::health))
        .route("/ready", get(handlers::readiness))
        .route(
            "/v1/messages",
            post(handlers::claude_messages).fallback(handlers::claude_method_not_allowed),
        )
        .route(
            "/messages",
            post(handlers::claude_messages).fallback(handlers::claude_method_not_allowed),
        )
        .route(
            "/anthropic/v1/messages",
            post(handlers::claude_messages).fallback(handlers::claude_method_not_allowed),
        )
        .route(
            "/v1/messages/count_tokens",
            post(handlers::count_tokens).fallback(handlers::claude_method_not_allowed),
        )
        .route(
            "/messages/count_tokens",
            post(handlers::count_tokens).fallback(handlers::claude_method_not_allowed),
        )
        .route(
            "/anthropic/v1/messages/count_tokens",
            post(handlers::count_tokens).fallback(handlers::claude_method_not_allowed),
        )
        .route(
            "/v1/chat/completions",
            post(handlers::openai_chat).fallback(handlers::openai_method_not_allowed),
        )
        .route(
            "/chat/completions",
            post(handlers::openai_chat).fallback(handlers::openai_method_not_allowed),
        )
        .route(
            "/v1/responses",
            post(handlers::openai_chat).fallback(handlers::openai_method_not_allowed),
        )
        .route(
            "/responses",
            post(handlers::openai_chat).fallback(handlers::openai_method_not_allowed),
        )
        .route(
            "/v1/models",
            get(handlers::models).fallback(handlers::openai_models_method_not_allowed),
        )
        .route(
            "/models",
            get(handlers::models).fallback(handlers::openai_models_method_not_allowed),
        )
        .route("/api/event_logging/batch", post(handlers::event_logging))
        .fallback(handlers::not_found)
        .layer(DefaultBodyLimit::max(50 * 1024 * 1024))
        .layer(middleware::from_fn(catch_panics))
        .layer(middleware::from_fn_with_state(
            keep_alive_state,
            keep_alive_headers,
        ))
        .layer(middleware::from_fn(cors))
        .layer(middleware::from_fn_with_state(trace_state, trace_requests))
        .with_state(router_state)
}

async fn trace_requests(
    State(state): State<ServiceHttpState>,
    mut request: axum::extract::Request,
    next: Next,
) -> Response {
    let trace_id = format!("trace_{}", Uuid::new_v4().simple());
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let http_version = format!("{:?}", request.version());
    let user_agent = logged_user_agent(request.headers());
    let content_length = request
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("unknown")
        .to_owned();
    let service_id = state.service.id.clone();
    let service_name = state.service.name.clone();
    let started = Instant::now();
    request.extensions_mut().insert(RequestTrace {
        id: trace_id.clone(),
    });
    tracing::info!(
        event = "http.request.received",
        trace_id = %trace_id,
        service_id = %service_id,
        service_name = %service_name,
        http_method = %method,
        http_path = %path,
        http_version,
        user_agent = %user_agent,
        content_length,
        "client request received"
    );
    let span = tracing::info_span!(
        "client_request",
        trace_id = %trace_id,
        service_id = %service_id,
        service_name = %service_name,
        http_method = %method,
        http_path = %path,
        http_version = %http_version,
        user_agent = %user_agent
    );
    let mut response = next.run(request).instrument(span).await;
    let status = response.status();
    let response_ready = Instant::now();
    let duration_ms = response_ready.duration_since(started).as_millis() as u64;
    let response_request_id = response
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let error_code = response
        .headers()
        .get("x-crosslane-error-code")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let error_stage = response
        .headers()
        .get("x-crosslane-error-stage")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let upstream_status = response
        .headers()
        .get("x-crosslane-upstream-status")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let account_error = response
        .headers()
        .get("x-crosslane-account-error")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let handled_error = !error_code.is_empty();
    if status.is_server_error() && !handled_error {
        tracing::error!(
            event = "http.response.ready",
            trace_id = %trace_id,
            service_id = %service_id,
            service_name = %service_name,
            request_id = %response_request_id,
            http_method = %method,
            http_path = %path,
            user_agent = %user_agent,
            http_status = status.as_u16(),
            error_code = %error_code,
            error_stage = %error_stage,
            upstream_status = %upstream_status,
            account_error = %account_error,
            duration_ms,
            "unhandled server response headers ready"
        );
    } else if (status.is_client_error() || status.is_server_error()) && !handled_error {
        tracing::warn!(
            event = "http.response.ready",
            trace_id = %trace_id,
            service_id = %service_id,
            service_name = %service_name,
            request_id = %response_request_id,
            http_method = %method,
            http_path = %path,
            user_agent = %user_agent,
            http_status = status.as_u16(),
            error_code = %error_code,
            error_stage = %error_stage,
            upstream_status = %upstream_status,
            account_error = %account_error,
            duration_ms,
            "client response headers ready"
        );
    } else {
        tracing::info!(
            event = "http.response.ready",
            trace_id = %trace_id,
            service_id = %service_id,
            service_name = %service_name,
            request_id = %response_request_id,
            http_method = %method,
            http_path = %path,
            user_agent = %user_agent,
            http_status = status.as_u16(),
            duration_ms,
            "client response headers ready"
        );
    }
    if let Ok(value) = axum::http::HeaderValue::from_str(&trace_id) {
        response.headers_mut().insert(
            axum::http::HeaderName::from_static(TRACE_ID_HEADER),
            value.clone(),
        );
        response.headers_mut().insert(
            axum::http::HeaderName::from_static(REQUEST_ID_HEADER),
            value,
        );
    }
    timing::trace_response(
        response,
        timing::RequestCompletion {
            started,
            response_ready,
            trace_id,
            service_id,
            service_name,
            request_id: response_request_id,
            http_method: method.to_string(),
            http_path: path,
            user_agent,
            http_status: status.as_u16(),
            error_code,
            error_stage,
            upstream_status,
        },
    )
}

async fn cors(request: axum::extract::Request, next: Next) -> Response {
    let preflight = request.method() == axum::http::Method::OPTIONS;
    let mut response = if preflight {
        axum::http::StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(request).await
    };
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
        axum::http::HeaderValue::from_static("*"),
    );
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_METHODS,
        axum::http::HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        axum::http::header::ACCESS_CONTROL_ALLOW_HEADERS,
        axum::http::HeaderValue::from_static(
            "authorization, content-type, x-api-key, anthropic-api-key, anthropic-version, anthropic-beta",
        ),
    );
    headers.insert(
        axum::http::header::ACCESS_CONTROL_MAX_AGE,
        axum::http::HeaderValue::from_static("86400"),
    );
    headers.insert(
        axum::http::header::ACCESS_CONTROL_EXPOSE_HEADERS,
        axum::http::HeaderValue::from_static("x-trace-id, request-id"),
    );
    response
}

async fn keep_alive_headers(
    State(state): State<ServiceHttpState>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let http1 = matches!(
        request.version(),
        axum::http::Version::HTTP_10 | axum::http::Version::HTTP_11
    );
    let mut response = next.run(request).await;
    if http1 {
        let seconds = state
            .app
            .config
            .current()
            .server
            .keep_alive_timeout_ms
            .div_ceil(1_000)
            .max(1);
        if let Ok(value) = axum::http::HeaderValue::from_str(&format!("timeout={seconds}")) {
            response.headers_mut().insert(
                axum::http::header::HeaderName::from_static("keep-alive"),
                value,
            );
        }
    }
    response
}

async fn catch_panics(request: axum::extract::Request, next: Next) -> Response {
    let request_id = request_trace_id(&request);
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let user_agent = logged_user_agent(request.headers());
    match std::panic::AssertUnwindSafe(next.run(request))
        .catch_unwind()
        .await
    {
        Ok(response) => response,
        Err(_) => {
            tracing::error!(
                event = "http.request.panic",
                trace_id = %request_id,
                http_method = %method,
                http_path = %path,
                user_agent = %user_agent,
                http_status = 500u16,
                error_code = "internal_panic",
                error_stage = "request_handler",
                "request handler panicked"
            );
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "type": "error",
                    "error":{"type":"api_error","message":"Internal server error"},
                    "request_id": request_id
                })),
            )
                .into_response()
        }
    }
}

struct RunningService {
    config: ProxyServiceConfig,
    port: u16,
    cancel: tokio_util::sync::CancellationToken,
    task: tokio::task::JoinHandle<()>,
    error: Arc<RwLock<Option<String>>>,
}

/// Reconciles configured proxy listeners independently from the admin plane.
#[derive(Default)]
pub struct ProxyServiceManager {
    reconcile_lock: tokio::sync::Mutex<()>,
    running: tokio::sync::Mutex<HashMap<String, RunningService>>,
}

impl ProxyServiceManager {
    pub async fn reconcile(
        &self,
        state: Arc<AppState>,
        services: &[ProxyServiceConfig],
    ) -> Vec<(String, String)> {
        let _reconcile = self.reconcile_lock.lock().await;
        let config = state.config.current();
        let desired = services
            .iter()
            .filter(|service| service.enabled)
            .cloned()
            .map(|service| (service.id.clone(), service))
            .collect::<HashMap<_, _>>();

        let stopped = {
            let mut running = self.running.lock().await;
            let ids = running
                .iter()
                .filter_map(|(id, current)| {
                    (current.task.is_finished()
                        || desired.get(id).is_none_or(|next| {
                            listener_config_changed(&current.config, next)
                                || current.port != effective_port(&config, next)
                        }))
                    .then_some(id.clone())
                })
                .collect::<Vec<_>>();
            ids.into_iter()
                .filter_map(|id| running.remove(&id))
                .collect::<Vec<_>>()
        };
        for mut service in stopped {
            if service.task.is_finished() && desired.contains_key(&service.config.id) {
                let error = read_lock(&service.error)
                    .clone()
                    .unwrap_or_else(|| "listener task exited unexpectedly".into());
                tracing::warn!(
                    service_id = %service.config.id,
                    service_name = %service.config.name,
                    %error,
                    "proxy service stopped; scheduling restart"
                );
            }
            service.cancel.cancel();
            if tokio::time::timeout(Duration::from_secs(10), &mut service.task)
                .await
                .is_err()
            {
                service.task.abort();
            }
        }

        if std::env::var("CROSSLANE_DISABLE_HTTP").as_deref() == Ok("1") {
            if !desired.is_empty() {
                tracing::info!("proxy services disabled by CROSSLANE_DISABLE_HTTP");
            }
            return Vec::new();
        }

        let existing = self
            .running
            .lock()
            .await
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        let mut failures = Vec::new();
        for (id, service) in desired {
            if existing.contains(&id) {
                continue;
            }
            match start_service(Arc::clone(&state), service.clone()).await {
                Ok(running) => {
                    self.running.lock().await.insert(id, running);
                }
                Err(error) => {
                    tracing::error!(service_id = %service.id, %error, "proxy service failed to start");
                    failures.push((service.id, error.to_string()));
                }
            }
        }
        failures
    }

    pub async fn views(&self, config: &crosslane_core::config::Config) -> Vec<ProxyServiceView> {
        let running = self.running.lock().await;
        config
            .proxy_service
            .iter()
            .map(|service| {
                let runtime = running.get(&service.id);
                let is_running = runtime.is_some_and(|runtime| !runtime.task.is_finished());
                let error = runtime.and_then(|runtime| read_lock(&runtime.error).clone());
                ProxyServiceView {
                    id: service.id.clone(),
                    name: service.name.clone(),
                    host: service.host.clone(),
                    port: runtime
                        .filter(|_| is_running)
                        .map_or_else(|| effective_port(config, service), |runtime| runtime.port),
                    enabled: service.enabled,
                    skip_user_agent_check: service.skip_user_agent_check,
                    running: is_running,
                    api_key_ids: service.api_key_ids.clone(),
                    account_tag: service
                        .account_tag
                        .as_ref()
                        .and_then(ServiceAccountTags::single_tag)
                        .map(str::to_owned),
                    account_tags: service.selected_account_tags().to_vec(),
                    account_ids: service.account_ids.clone(),
                    excluded_account_ids: service.excluded_account_ids.clone(),
                    default_provider: service.default_provider.clone(),
                    allowed_providers: service.allowed_providers.clone(),
                    created_at: service.created_at,
                    error,
                }
            })
            .collect()
    }
}

fn listener_config_changed(current: &ProxyServiceConfig, next: &ProxyServiceConfig) -> bool {
    // UA policy and account-pool membership are read from the live
    // ConfigHandle for every request. Excluding them here applies control-plane
    // edits without cancelling active streaming requests.
    current.id != next.id
        || current.name != next.name
        || current.host != next.host
        || current.port != next.port
        || current.enabled != next.enabled
        || current.api_key_ids != next.api_key_ids
        || current.default_provider != next.default_provider
        || current.allowed_providers != next.allowed_providers
        || current.created_at != next.created_at
}

async fn start_service(
    state: Arc<AppState>,
    service: ProxyServiceConfig,
) -> anyhow::Result<RunningService> {
    let config = state.config.current();
    let port = effective_port(&config, &service);
    let address = resolve_address(&service.host, port).await?;
    let cancel = state.shutdown.child_token();
    let error = Arc::new(RwLock::new(None));
    let task_error = Arc::clone(&error);
    let task_service = service.clone();
    let service_id = service.id.clone();
    let service_name = service.name.clone();

    let task = if config.server.tls.enabled {
        let probe = std::net::TcpListener::bind(address)?;
        drop(probe);
        let tls =
            if let (Some(cert), Some(key)) = (&config.server.tls.cert, &config.server.tls.key) {
                axum_server::tls_rustls::RustlsConfig::from_pem(
                    cert.as_bytes().to_vec(),
                    key.as_bytes().to_vec(),
                )
                .await?
            } else {
                let cert = config.server.tls.cert_path.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("server.tls.cert_path or inline cert is required")
                })?;
                let key = config.server.tls.key_path.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("server.tls.key_path or inline key is required")
                })?;
                axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key).await?
            };
        state.install_tls_config(tls.clone());
        let handle = axum_server::Handle::new();
        let stop_handle = handle.clone();
        let stop = cancel.clone();
        tokio::spawn(async move {
            tokio::spawn(async move {
                stop.cancelled().await;
                stop_handle.graceful_shutdown(Some(Duration::from_secs(10)));
            });
            tracing::info!(%address, %service_id, %service_name, "proxy HTTPS service listening");
            if let Err(failure) = axum_server::bind_rustls(address, tls)
                .handle(handle)
                .serve(router_for_service(state, task_service, true).into_make_service())
                .await
            {
                *write_lock(&task_error) = Some(failure.to_string());
            }
        })
    } else {
        let listener = tokio::net::TcpListener::bind(address).await?;
        let serve_cancel = cancel.clone();
        tokio::spawn(async move {
            tracing::info!(%address, %service_id, %service_name, "proxy HTTP service listening");
            if let Err(failure) =
                axum::serve(listener, router_for_service(state, task_service, true))
                    .with_graceful_shutdown(serve_cancel.cancelled_owned())
                    .await
            {
                *write_lock(&task_error) = Some(failure.to_string());
            }
        })
    };
    Ok(RunningService {
        config: service,
        port,
        cancel,
        task,
        error,
    })
}

pub(crate) fn effective_port(
    config: &crosslane_core::config::Config,
    service: &ProxyServiceConfig,
) -> u16 {
    effective_port_with_override(config, service, http_port_override())
}

pub(crate) fn http_port_override() -> Option<u16> {
    std::env::var("CROSSLANE_HTTP_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|port| *port > 0)
}

pub(crate) fn effective_port_with_override(
    config: &crosslane_core::config::Config,
    service: &ProxyServiceConfig,
    override_port: Option<u16>,
) -> u16 {
    if service.port == config.server.port {
        override_port.unwrap_or(service.port)
    } else {
        service.port
    }
}

async fn resolve_address(host: &str, port: u16) -> anyhow::Result<std::net::SocketAddr> {
    tokio::net::lookup_host((host, port))
        .await?
        .next()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve listen host {host}"))
}

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::Mutex;

    use axum::body::{to_bytes, Body};
    use axum::http::{header, Method, Request, StatusCode};
    use crosslane_core::account::{Account, AuthMethod, Credentials, Usage};
    use crosslane_core::config::{ApiKeyConfig, ApiKeyFormat, Config, ProxyServiceConfig};
    use crosslane_core::paths::Paths;
    use crosslane_store::accounts::AccountStore;
    use crosslane_store::config_loader::ConfigHandle;
    use tower::ServiceExt;

    use super::*;

    #[derive(Clone)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    impl Write for LogCapture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("capture lock")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    pub(super) async fn test_state(mut config: Config) -> (tempfile::TempDir, Arc<AppState>) {
        config.server.port = 5580;
        let directory = tempfile::tempdir().expect("tempdir");
        let paths = Paths::from_env_values(
            Some(directory.path().to_str().expect("utf8")),
            None,
            None,
            None,
        );
        crosslane_store::bootstrap::ensure_layout(&paths)
            .await
            .expect("layout");
        let accounts = AccountStore::load(&paths.accounts_file)
            .await
            .expect("accounts");
        let state = Arc::new(AppState::new(paths, ConfigHandle::new(config), accounts));
        (directory, state)
    }

    pub(super) async fn body_json(response: Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    pub(super) fn discovered_model(
        id: &str,
        name: &str,
        max_input_tokens: Option<u32>,
        max_output_tokens: Option<u32>,
    ) -> crosslane_kiro::ModelInfo {
        crosslane_kiro::ModelInfo {
            model_id: id.into(),
            model_name: name.into(),
            description: "upstream description must not leak into protocol responses".into(),
            rate_multiplier: Some(1.0),
            token_limits: Some(crosslane_kiro::client::TokenLimits {
                max_input_tokens,
                max_output_tokens,
            }),
            additional_model_request_fields_schema: None,
        }
    }

    #[test]
    fn provider_routing_changes_replace_the_listener_snapshot() {
        let current = ProxyServiceConfig {
            id: "svc_test".into(),
            name: "test".into(),
            host: "127.0.0.1".into(),
            port: 5580,
            api_key_ids: vec!["ak_test".into()],
            ..ProxyServiceConfig::default()
        };
        let mut next = current.clone();
        next.default_provider = "copilot".into();
        next.allowed_providers = vec!["kiro".into(), "copilot".into()];

        assert!(listener_config_changed(&current, &next));
    }

    pub(super) fn account_with_usage(
        id: &str,
        email: &str,
        enabled: bool,
        usage: Option<Usage>,
    ) -> Account {
        Account {
            id: id.into(),
            email: email.into(),
            label: None,
            enabled,
            machine_id: "0".repeat(64),
            profile_arn: None,
            upstream_user_id: None,
            credentials: Credentials {
                access_token: format!("token-{id}"),
                refresh_token: None,
                client_id: None,
                client_secret: None,
                region: "us-east-1".into(),
                expires_at: i64::MAX,
                auth_method: AuthMethod::Idc,
            },
            usage,
            subscription: None,
            tags: Vec::new(),
            created_at: 0,
            credit_exhausted: false,
        }
    }

    async fn install_context_test_account(state: &Arc<AppState>, model: &str) {
        // Window validation now follows actual account/model resolution. The
        // oversized input must still fail before this dummy token is used.
        let account = account_with_usage(
            "context-test",
            "context@example.com",
            true,
            Some(Usage {
                current: 0.0,
                limit: 100.0,
                overage_cap: None,
                percent_used: 0.0,
                updated_at: 1,
                next_reset_date: None,
            }),
        );
        let pool = state.pool();
        pool.replace_accounts(vec![account]).await;
        pool.get("context-test")
            .await
            .expect("account")
            .set_supported_models([model.to_owned()])
            .await;
    }

    /// Register one enabled account that supports every given model.
    ///
    /// The model catalog is scoped to a service's account pool, so a dynamic
    /// catalog is only visible once some in-scope account reports it.
    async fn seed_catalog_account(state: &Arc<AppState>, model_ids: &[String]) {
        let pool = state.pool();
        pool.replace_accounts(vec![account_with_usage(
            "catalog-account",
            "catalog@example.com",
            true,
            None,
        )])
        .await;
        pool.get("catalog-account")
            .await
            .expect("catalog account")
            .set_supported_models(model_ids.iter().cloned())
            .await;
    }

    #[tokio::test]
    async fn every_response_has_a_unique_trace_id() {
        let (_directory, state) = test_state(Config::default()).await;
        let mut trace_ids = Vec::new();
        for _ in 0..2 {
            let response = router(Arc::clone(&state))
                .oneshot(
                    Request::get("/health")
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");
            let trace_id = response
                .headers()
                .get(TRACE_ID_HEADER)
                .expect("trace header")
                .to_str()
                .expect("trace header text")
                .to_owned();
            assert_eq!(
                response
                    .headers()
                    .get(REQUEST_ID_HEADER)
                    .expect("request id header"),
                trace_id.as_str()
            );
            assert!(trace_id.starts_with("trace_"));
            assert_eq!(trace_id.len(), 38);
            trace_ids.push(trace_id);
        }
        assert_ne!(trace_ids[0], trace_ids[1]);
    }

    #[test]
    fn logged_user_agent_is_bounded_and_redacts_key_like_values() {
        let mut headers = axum::http::HeaderMap::new();
        assert_eq!(logged_user_agent(&headers), "unknown");

        headers.insert(
            header::USER_AGENT,
            format!("client/1.0 {}", "x".repeat(300))
                .parse()
                .expect("header value"),
        );
        let agent = logged_user_agent(&headers);
        assert_eq!(agent.chars().count(), MAX_LOGGED_USER_AGENT_CHARS);
        assert!(agent.starts_with("client/1.0 "));
        assert!(agent.ends_with('…'));

        headers.insert(
            header::USER_AGENT,
            "client/1.0 token=secret".parse().expect("header value"),
        );
        assert_eq!(logged_user_agent(&headers), "[redacted]");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn request_completion_covers_body_read_success_failure_and_empty_responses() {
        let (_directory, state) = test_state(Config::default()).await;
        let captured = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::clone(&captured);
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(move || LogCapture(Arc::clone(&writer)))
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let slow_body = async_stream::stream! {
            tokio::time::sleep(Duration::from_millis(20)).await;
            yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"{"));
        };
        let broken_body = futures::stream::iter([
            Ok(bytes::Bytes::from_static(b"{")),
            Err(std::io::Error::other("fixture body failure")),
        ]);
        let requests = [
            (
                Request::get("/health").body(Body::empty()).unwrap(),
                StatusCode::OK,
            ),
            (
                Request::post("/v1/messages")
                    .body(Body::from_stream(slow_body))
                    .unwrap(),
                StatusCode::BAD_REQUEST,
            ),
            (
                Request::post("/v1/messages")
                    .body(Body::from_stream(broken_body))
                    .unwrap(),
                StatusCode::BAD_REQUEST,
            ),
            (
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/v1/messages")
                    .body(Body::empty())
                    .unwrap(),
                StatusCode::NO_CONTENT,
            ),
        ];
        let mut trace_ids = Vec::new();
        for (request, status) in requests {
            let response = router(state.clone()).oneshot(request).await.unwrap();
            assert_eq!(response.status(), status);
            trace_ids.push(
                response.headers()[TRACE_ID_HEADER]
                    .to_str()
                    .unwrap()
                    .to_owned(),
            );
            to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        }
        let output = captured.lock().unwrap().clone();
        let entries = std::str::from_utf8(&output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        for (index, trace_id) in trace_ids.iter().enumerate() {
            let completed = entries
                .iter()
                .filter(|entry| {
                    entry["fields"]["event"] == "http.response.completed"
                        && entry["fields"]["trace_id"] == *trace_id
                })
                .collect::<Vec<_>>();
            assert_eq!(completed.len(), 1);
            let fields = &completed[0]["fields"];
            assert_eq!(fields["outcome"], "completed");
            assert!(
                fields["duration_ms"].as_u64().unwrap()
                    >= fields["response_ready_ms"].as_u64().unwrap()
            );
            if index == 1 || index == 2 {
                let body_read = entries
                    .iter()
                    .find(|entry| {
                        entry["fields"]["event"] == "http.request.body_read"
                            && entry["fields"]["trace_id"] == *trace_id
                    })
                    .unwrap();
                assert_eq!(body_read["fields"]["body_bytes"], 1);
                assert_eq!(
                    body_read["fields"]["outcome"],
                    if index == 1 { "completed" } else { "failed" }
                );
                if index == 1 {
                    assert!(body_read["fields"]["body_read_ms"].as_u64().unwrap() >= 20);
                    assert!(fields["duration_ms"].as_u64().unwrap() >= 20);
                }
            }
            if index == 3 {
                assert_eq!(fields["response_bytes"], 0);
                assert!(fields.get("first_byte_ms").is_none());
            }
        }
    }

    #[tokio::test]
    async fn timing_preserves_request_body_limits_and_releases_failed_reservations() {
        const BODY_BUDGET_BYTES: usize = 128 * 1024 * 1024;
        let (_directory, state) = test_state(Config::default()).await;
        for path in [
            "/v1/messages",
            "/v1/messages/count_tokens",
            "/v1/chat/completions",
            "/v1/responses",
        ] {
            let occupied = state.body_budget.reserve(BODY_BUDGET_BYTES).unwrap();
            let response = router(state.clone())
                .oneshot(Request::post(path).body(Body::from("{}")).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
            assert!(body_json(response).await["error"]["message"]
                .as_str()
                .unwrap()
                .contains("memory budget"));
            drop(occupied);

            let broken = futures::stream::iter([
                Ok(bytes::Bytes::from_static(b"{")),
                Err(std::io::Error::other("fixture body failure")),
            ]);
            let response = router(state.clone())
                .oneshot(Request::post(path).body(Body::from_stream(broken)).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
            assert!(body_json(response).await["error"]["message"]
                .as_str()
                .unwrap()
                .contains("failed to read request body"));

            let chunk = bytes::Bytes::from(vec![b'x'; 1024 * 1024]);
            let oversized =
                futures::stream::iter((0..51).map(move |_| Ok::<_, std::io::Error>(chunk.clone())));
            let response = router(state.clone())
                .oneshot(
                    Request::post(path)
                        .body(Body::from_stream(oversized))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE, "{path}");
            let body = body_json(response).await;
            assert_eq!(
                body["error"]["type"],
                if path.contains("messages") {
                    "request_too_large"
                } else {
                    "invalid_request_error"
                },
                "{body}"
            );
            assert!(
                body["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("50 MiB"),
                "{body}"
            );
            drop(
                state
                    .body_budget
                    .reserve(BODY_BUDGET_BYTES)
                    .expect("all failed reads must release their body reservations"),
            );
        }
    }

    #[tokio::test]
    async fn timing_preserves_the_request_body_idle_deadline() {
        let (_directory, state) = test_state(Config::default()).await;
        let stalled = async_stream::stream! {
            yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"{"));
            std::future::pending::<()>().await;
        };
        let response = tokio::time::timeout(
            Duration::from_secs(20),
            router(state.clone()).oneshot(
                Request::post("/v1/chat/completions")
                    .body(Body::from_stream(stalled))
                    .unwrap(),
            ),
        )
        .await
        .expect("the inbound body read must retain its idle deadline")
        .unwrap();
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
        assert!(body_json(response).await["error"]["message"]
            .as_str()
            .unwrap()
            .contains("request body read timed out"));
        drop(
            state
                .body_budget
                .reserve(128 * 1024 * 1024)
                .expect("a timed-out read must release the partial body"),
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn request_failures_log_client_user_agent_at_warn_and_error() {
        let mut config = Config::default();
        config.server.max_connections = 1;
        let (_directory, state) = test_state(config).await;
        let captured = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::clone(&captured);
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(move || LogCapture(Arc::clone(&writer)))
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);

        let rejected = router(Arc::clone(&state))
            .oneshot(
                Request::post("/v1/messages")
                    .header(header::USER_AGENT, "claude-cli/2.1.260 (external, test)")
                    .body(Body::from("{"))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);

        let _held_connection = state.connections.try_acquire().expect("connection slot");
        let failed = router(state)
            .oneshot(
                Request::post("/v1/messages")
                    .header(header::USER_AGENT, "claude-cli/2.1.261 (external, test)")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"model":"test","max_tokens":1,"messages":[{"role":"user","content":"hello"}]}"#,
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(failed.status(), StatusCode::SERVICE_UNAVAILABLE);

        drop(guard);
        let output =
            String::from_utf8(captured.lock().expect("capture lock").clone()).expect("utf8 logs");
        let entries = output
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json log"))
            .collect::<Vec<_>>();
        for (level, event, agent) in [
            (
                "WARN",
                "proxy.request.rejected",
                "claude-cli/2.1.260 (external, test)",
            ),
            (
                "ERROR",
                "proxy.request.failed",
                "claude-cli/2.1.261 (external, test)",
            ),
        ] {
            assert!(
                entries.iter().any(|entry| {
                    entry["level"] == level
                        && entry["fields"]["event"] == event
                        && entry["fields"]["user_agent"] == agent
                }),
                "missing {level} event for {agent}: {output}"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn claude_client_errors_are_visible_in_request_logs_and_warn_events() {
        let (_directory, state) = test_state(Config::default()).await;
        let captured = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::clone(&captured);
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(move || LogCapture(Arc::clone(&writer)))
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        let user_agent = "claude-cli/2.1.260 (external, test)";
        let requests = [
            Request::post("/v1/messages")
                .header(header::USER_AGENT, user_agent)
                .body(Body::from("{"))
                .expect("invalid JSON request"),
            Request::get("/v1/models?limit=bad")
                .header(header::USER_AGENT, user_agent)
                .body(Body::empty())
                .expect("invalid model query"),
            Request::get("/v1/messages")
                .header(header::USER_AGENT, user_agent)
                .body(Body::empty())
                .expect("wrong method"),
            Request::get("/v1/messages/unknown")
                .header(header::USER_AGENT, user_agent)
                .body(Body::empty())
                .expect("unknown route"),
        ];
        let mut trace_ids = Vec::new();
        for (request, expected_status) in requests.into_iter().zip([
            StatusCode::BAD_REQUEST,
            StatusCode::BAD_REQUEST,
            StatusCode::METHOD_NOT_ALLOWED,
            StatusCode::NOT_FOUND,
        ]) {
            let response = router(Arc::clone(&state))
                .oneshot(request)
                .await
                .expect("response");
            assert_eq!(response.status(), expected_status);
            trace_ids.push(
                response
                    .headers()
                    .get(TRACE_ID_HEADER)
                    .expect("trace header")
                    .to_str()
                    .expect("trace ID")
                    .to_owned(),
            );
            // Complete the error response so this test checks one rejection
            // warning, rather than also simulating a client cancellation.
            to_bytes(response.into_body(), 1024 * 1024)
                .await
                .expect("error response body");
        }
        drop(guard);
        let request_logs = state.stats.snapshot(None).recent_requests;
        let output =
            String::from_utf8(captured.lock().expect("capture lock").clone()).expect("utf8 logs");
        let entries = output
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json log"))
            .collect::<Vec<_>>();
        for trace_id in trace_ids {
            assert!(
                request_logs
                    .iter()
                    .any(|request| request.trace_id == trace_id && request.user_agent == user_agent),
                "missing request record for {trace_id}"
            );
            assert!(
                entries.iter().any(|entry| {
                    entry["level"] == "WARN"
                        && entry["fields"]["event"] == "proxy.request.rejected"
                        && entry["fields"]["trace_id"] == trace_id
                        && entry["fields"]["user_agent"] == user_agent
                }),
                "missing WARN rejection event for {trace_id}: {output}"
            );
            assert_eq!(
                entries
                    .iter()
                    .filter(|entry| {
                        entry["level"] == "WARN" && entry["fields"]["trace_id"] == trace_id
                    })
                    .count(),
                1,
                "duplicate WARN events for {trace_id}: {output}"
            );
        }
    }

    #[tokio::test]
    async fn readiness_degrades_without_making_liveness_fail() {
        let (_directory, state) = test_state(Config::default()).await;
        let app = router(Arc::clone(&state));
        let health = app
            .clone()
            .oneshot(
                Request::get("/health")
                    .body(Body::empty())
                    .expect("health request"),
            )
            .await
            .expect("health response");
        let ready = app
            .oneshot(
                Request::get("/ready")
                    .body(Body::empty())
                    .expect("ready request"),
            )
            .await
            .expect("ready response");

        assert_eq!(health.status(), StatusCode::OK);
        assert_eq!(ready.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(!body_json(ready).await["ready"]
            .as_bool()
            .expect("ready flag"));
    }

    #[tokio::test]
    async fn service_http_state_resolves_an_isolated_effective_account_pool() {
        let (_directory, state) = test_state(Config::default()).await;
        let mut tagged = account_with_usage("acc_tagged", "tagged@example.com", true, None);
        tagged.tags = vec!["team-a".into()];
        let mut manual = account_with_usage("acc_manual", "manual@example.com", true, None);
        manual.tags = vec!["team-b".into()];
        let mut excluded = account_with_usage("acc_excluded", "excluded@example.com", true, None);
        excluded.tags = vec!["team-a".into()];
        state
            .pool()
            .replace_accounts(vec![tagged, manual, excluded])
            .await;
        let service = ServiceHttpState {
            app: state,
            service: Arc::new(ProxyServiceConfig {
                web_search_provider: None,
                crs_relay: None,
                id: "svc_team".into(),
                name: "team".into(),
                host: "127.0.0.1".into(),
                port: 5580,
                enabled: true,
                skip_user_agent_check: false,
                api_key_ids: Vec::new(),
                account_tag: Some("team-a".into()),
                account_ids: vec!["acc_manual".into()],
                excluded_account_ids: vec!["acc_excluded".into()],
                created_at: 0,
                default_provider: String::new(),
                allowed_providers: Vec::new(),
            }),
            allowed_api_key_ids: Arc::new(HashSet::new()),
        };

        let account_ids = service.account_ids().await;
        assert_eq!(
            account_ids.as_ref(),
            &HashSet::from(["acc_tagged".to_string(), "acc_manual".to_string()])
        );
    }

    #[tokio::test]
    async fn account_pool_edits_are_hot_reloaded_without_restarting_the_listener() {
        let service = ProxyServiceConfig {
            web_search_provider: None,
            crs_relay: None,
            id: "svc_team".into(),
            name: "team".into(),
            host: "127.0.0.1".into(),
            port: 5580,
            enabled: true,
            skip_user_agent_check: false,
            api_key_ids: Vec::new(),
            account_tag: Some("team-a".into()),
            account_ids: Vec::new(),
            excluded_account_ids: Vec::new(),
            created_at: 0,
            default_provider: String::new(),
            allowed_providers: Vec::new(),
        };
        let mut config = Config::default();
        config.proxy_service.push(service.clone());
        let (_directory, state) = test_state(config).await;
        let mut tagged = account_with_usage("acc_tagged", "tagged@example.com", true, None);
        tagged.tags = vec!["team-a".into()];
        let mut manual = account_with_usage("acc_manual", "manual@example.com", true, None);
        manual.tags = vec!["team-b".into()];
        state.pool().replace_accounts(vec![tagged, manual]).await;
        let http_state = ServiceHttpState {
            app: Arc::clone(&state),
            service: Arc::new(service.clone()),
            allowed_api_key_ids: Arc::new(HashSet::new()),
        };
        assert_eq!(
            http_state.account_ids().await.as_ref(),
            &HashSet::from(["acc_tagged".to_string()])
        );

        let mut next = state.config.current().as_ref().clone();
        next.proxy_service[0].account_ids = vec!["acc_manual".into()];
        next.proxy_service[0].excluded_account_ids = vec!["acc_tagged".into()];
        assert!(!listener_config_changed(&service, &next.proxy_service[0]));
        state.config.replace(next);

        assert_eq!(
            http_state.account_ids().await.as_ref(),
            &HashSet::from(["acc_manual".to_string()])
        );
    }

    #[tokio::test]
    async fn health_reports_total_accounts_and_aggregate_upstream_credits() {
        let (_directory, state) = test_state(Config::default()).await;
        let mut accounts = AccountStore::load(&state.paths.accounts_file)
            .await
            .expect("accounts");
        accounts
            .insert(account_with_usage(
                "acc_one",
                "one@example.com",
                true,
                Some(Usage {
                    current: 1.25,
                    limit: 10.0,
                    overage_cap: None,
                    percent_used: 12.5,
                    next_reset_date: None,
                    updated_at: 1,
                }),
            ))
            .expect("first account");
        accounts
            .insert(account_with_usage(
                "acc_two",
                "two@example.com",
                false,
                Some(Usage {
                    current: 2.75,
                    limit: 20.0,
                    overage_cap: None,
                    percent_used: 13.75,
                    next_reset_date: None,
                    updated_at: 2,
                }),
            ))
            .expect("second account");
        accounts
            .insert(account_with_usage(
                "acc_unknown",
                "unknown@example.com",
                true,
                None,
            ))
            .expect("account without usage");
        state
            .apply_account_file_reload(accounts)
            .await
            .expect("reload accounts");

        let response = router(state)
            .oneshot(
                Request::get("/health")
                    .body(Body::empty())
                    .expect("health request"),
            )
            .await
            .expect("health response");
        let health = body_json(response).await;

        assert_eq!(health["total_accounts"], 3);
        assert_eq!(health["used_credits"], 4.0);
        assert_eq!(health["total_credits"], 30.0);
    }

    #[tokio::test]
    async fn context_limit_errors_are_self_contained_for_intermediate_relays() {
        let mut config = Config::default();
        config.context.max_input_tokens = 128;
        config.context.safe_input_ratio = 1.0;
        config.context.auto_compact_on_overflow = false;
        let (_directory, state) = test_state(config).await;
        install_context_test_account(&state, "claude-sonnet-4.6").await;
        let response = router(state)
            .oneshot(
                Request::post("/v1/messages")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::USER_AGENT, "claude-cli/2.1.220 (external, test)")
                    .body(Body::from(
                        serde_json::json!({
                            "model": "claude-sonnet-4.6",
                            "max_tokens": 1,
                            "stream": true,
                            "messages": [{"role": "user", "content": "long ".repeat(500)}]
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(
            response.status(),
            if cfg!(feature = "crs") {
                StatusCode::OK
            } else {
                StatusCode::BAD_REQUEST
            }
        );
        let request_id = response
            .headers()
            .get(REQUEST_ID_HEADER)
            .expect("request id header")
            .to_str()
            .expect("request id header text")
            .to_owned();
        assert_eq!(
            response
                .headers()
                .get(TRACE_ID_HEADER)
                .expect("trace id header"),
            request_id.as_str()
        );
        let body = if cfg!(feature = "crs") {
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            let text = std::str::from_utf8(&bytes).unwrap();
            text.lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .map(|data| serde_json::from_str::<serde_json::Value>(data).unwrap())
                .find(|event| event["type"] == "error")
                .expect("SSE error event")
        } else {
            body_json(response).await
        };
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["request_id"], request_id);
        let message = body["error"]["message"].as_str().expect("message");
        assert!(message.starts_with("Current input exceeds this model's input budget:"));
        assert!(message.contains("tokens (limit 128)"));
        assert!(message.contains("try /compact"));
        assert!(message.contains("Esc Esc or /rewind"));
        assert_eq!(body["error"]["code"], "context_length_exceeded");
        assert_eq!(body["error"]["retryable"], false);
        assert_eq!(
            body["error"]["context"]["overflow_scope"],
            "current_message"
        );
        assert_eq!(body["error"]["context"]["maximum_input_tokens"], 128);
        #[cfg(feature = "crs")]
        {
            assert!(message.contains(&format!("request ID: {request_id}")));
            assert_eq!(body["error"]["status"], 400);
            assert!(body["error"].get("upstream_status").is_none());
        }
    }

    #[tokio::test]
    async fn ordinary_claude_tools_are_not_rejected_by_the_tool_search_budget() {
        let mut config = Config::default();
        config.context.max_tool_input_tokens = 1;
        let (_directory, state) = test_state(config).await;
        let response = router(state)
            .oneshot(
                Request::post("/v1/messages")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::USER_AGENT, "claude-cli/2.1.220 (external, test)")
                    .body(Body::from(
                        serde_json::json!({
                            "model": "claude-opus-5",
                            "max_tokens": 1,
                            "messages": [{"role": "user", "content": "Who are you?"}],
                            "tools": [{
                                "name": "lookup",
                                "description": "Look up a value in the local workspace",
                                "input_schema": {
                                    "type": "object",
                                    "properties": {"query": {"type": "string"}},
                                    "required": ["query"]
                                }
                            }]
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        // No test account is configured, so reaching the pool proves the
        // ordinary tool request passed local payload admission. Before the
        // fix this returned 413 from max_tool_input_tokens.
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn claude_accepts_more_than_the_legacy_128_tool_limit() {
        let (_directory, state) = test_state(Config::default()).await;
        let tools = (0..129)
            .map(|index| {
                serde_json::json!({
                    "name": format!("tool_{index}"),
                    "description": "small test tool",
                    "input_schema": {"type": "object"}
                })
            })
            .collect::<Vec<_>>();
        let response = router(state)
            .oneshot(
                Request::post("/v1/messages")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::USER_AGENT, "claude-cli/2.1.220 (external, test)")
                    .body(Body::from(
                        serde_json::json!({
                            "model": "claude-opus-5",
                            "max_tokens": 1,
                            "messages": [{"role": "user", "content": "hello"}],
                            "tools": tools
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        // No test account is configured, so reaching the pool proves that
        // the request passed the proxy's tool-count admission checks.
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn claude_tool_capacity_errors_do_not_masquerade_as_32mb_body_errors() {
        let (_directory, state) = test_state(Config::default()).await;
        let tools = (0..=crosslane_translate::validate::MAX_TOOLS)
            .map(|index| {
                serde_json::json!({
                    "name": format!("tool_{index}"),
                    "description": "small test tool",
                    "input_schema": {"type": "object"}
                })
            })
            .collect::<Vec<_>>();
        let response = router(state)
            .oneshot(
                Request::post("/v1/messages")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::USER_AGENT, "claude-cli/2.1.220 (external, test)")
                    .body(Body::from(
                        serde_json::json!({
                            "model": "claude-opus-5",
                            "max_tokens": 1,
                            "messages": [{"role": "user", "content": "hello"}],
                            "tools": tools
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.headers()["x-crosslane-error-code"],
            "tool_budget_exceeded"
        );
        let body = body_json(response).await;
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert!(body["error"]["message"]
            .as_str()
            .expect("message")
            .contains("too many tools"));
    }

    #[tokio::test]
    async fn aliases_methods_and_model_timestamps_match_protocol_contract() {
        let mut config = Config::default();
        config.models.dynamic_discovery = false;
        let (_directory, state) = test_state(config).await;
        seed_catalog_account(&state, &["claude-sonnet-4.6".into()]).await;
        for path in ["/v1/models", "/models"] {
            let response = router(Arc::clone(&state))
                .oneshot(
                    Request::get(path)
                        .header(header::USER_AGENT, "codex_cli_rs/0.147.0 (test)")
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::OK);
            let body = body_json(response).await;
            let created = body["data"][0]["created"].as_i64().expect("created");
            assert!(created > 1_700_000_000 && created < 100_000_000_000);
        }

        let response = router(state)
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/anthropic/v1/messages")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            response.headers().get(header::ALLOW).expect("allow"),
            "POST"
        );
        assert_eq!(body_json(response).await["type"], "error");
    }

    #[tokio::test]
    async fn model_catalog_does_not_fall_back_to_accounts_outside_the_service_pool() {
        let (_directory, state) = test_state(Config::default()).await;
        let mut outside = account_with_usage("acc_outside", "outside@example.com", true, None);
        outside.tags = vec!["team-b".into()];
        state.pool().replace_accounts(vec![outside]).await;
        let service = ProxyServiceConfig {
            web_search_provider: None,
            crs_relay: None,
            id: "svc_team_a".into(),
            name: "team-a".into(),
            host: "127.0.0.1".into(),
            port: 5580,
            enabled: true,
            skip_user_agent_check: false,
            api_key_ids: Vec::new(),
            account_tag: Some("team-a".into()),
            account_ids: Vec::new(),
            excluded_account_ids: Vec::new(),
            created_at: 0,
            default_provider: String::new(),
            allowed_providers: Vec::new(),
        };

        let response = router_for_service(state, service, false)
            .oneshot(
                Request::get("/v1/models")
                    .header(header::USER_AGENT, "codex_cli_rs/0.147.0 (test)")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_json(response).await["data"]
            .as_array()
            .expect("model data")
            .is_empty());
    }

    #[tokio::test]
    async fn model_catalog_never_borrows_another_services_dynamic_models() {
        let (_directory, state) = test_state(Config::default()).await;
        let mut inside = account_with_usage("acc_inside", "inside@example.com", true, None);
        inside.tags = vec!["team-a".into()];
        let mut outside = account_with_usage("acc_outside", "outside@example.com", true, None);
        outside.tags = vec!["team-b".into()];
        state.pool().replace_accounts(vec![inside, outside]).await;
        state
            .pool()
            .get("acc_outside")
            .await
            .expect("outside account")
            .set_supported_models(["outside-only".to_string()])
            .await;
        let model = |model_id: &str| crosslane_kiro::ModelInfo {
            model_id: model_id.into(),
            model_name: model_id.into(),
            description: String::new(),
            rate_multiplier: None,
            token_limits: None,
            additional_model_request_fields_schema: None,
        };
        state.models.finish_refresh(vec![model("outside-only")]);
        let service = ProxyServiceConfig {
            web_search_provider: None,
            crs_relay: None,
            id: "svc_team_a".into(),
            name: "team-a".into(),
            host: "127.0.0.1".into(),
            port: 5580,
            enabled: true,
            skip_user_agent_check: false,
            api_key_ids: Vec::new(),
            account_tag: Some("team-a".into()),
            account_ids: Vec::new(),
            excluded_account_ids: Vec::new(),
            created_at: 0,
            default_provider: String::new(),
            allowed_providers: Vec::new(),
        };

        let response = router_for_service(Arc::clone(&state), service.clone(), false)
            .oneshot(
                Request::get("/v1/models")
                    .header(header::USER_AGENT, "codex_cli_rs/0.147.0 (test)")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let data = body_json(response).await["data"]
            .as_array()
            .expect("model data")
            .clone();
        assert!(
            !data.is_empty(),
            "cold scoped catalog should use static fallback"
        );
        assert!(data.iter().all(|entry| entry["id"] != "outside-only"));

        let inside_runtime = state
            .pool()
            .get("acc_inside")
            .await
            .expect("inside account");
        inside_runtime
            .set_supported_models(["inside-only".to_string()])
            .await;
        state
            .models
            .finish_refresh(vec![model("inside-only"), model("outside-only")]);
        inside_runtime.account.write().await.enabled = false;
        let response = router_for_service(Arc::clone(&state), service.clone(), false)
            .oneshot(
                Request::get("/v1/models")
                    .header(header::USER_AGENT, "codex_cli_rs/0.147.0 (test)")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let disabled_data = body_json(response).await["data"]
            .as_array()
            .expect("model data")
            .clone();
        assert!(disabled_data.is_empty());

        inside_runtime.account.write().await.enabled = true;
        let response = router_for_service(state, service, false)
            .oneshot(
                Request::get("/v1/models")
                    .header(header::USER_AGENT, "codex_cli_rs/0.147.0 (test)")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let body = body_json(response).await;
        assert_eq!(body["data"].as_array().expect("model data").len(), 1);
        assert_eq!(body["data"][0]["id"], "inside-only");
    }

    #[tokio::test]
    async fn openai_model_list_uses_only_the_openai_response_contract() {
        let (_directory, state) = test_state(Config::default()).await;
        state.models.finish_refresh(vec![
            discovered_model(
                "claude-opus-5",
                "Claude Opus 5",
                Some(1_000_000),
                Some(64_000),
            ),
            discovered_model(
                "claude-sonnet-5",
                "Claude Sonnet 5",
                Some(200_000),
                Some(32_000),
            ),
        ]);
        seed_catalog_account(
            &state,
            &["claude-opus-5".to_string(), "claude-sonnet-5".to_string()],
        )
        .await;

        let response = router(state)
            .oneshot(
                Request::get("/v1/models?limit=1")
                    .header(header::USER_AGENT, "codex_cli_rs/0.147.0 (test)")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["object"], "list");
        let data = body["data"].as_array().expect("model data");
        assert_eq!(data.len(), 2, "OpenAI does not define list pagination");
        let entry = data[0].as_object().expect("model object");
        assert_eq!(entry.len(), 4);
        assert_eq!(entry["id"], "claude-opus-5");
        assert_eq!(entry["object"], "model");
        assert!(entry["created"].as_i64().is_some());
        assert_eq!(entry["owned_by"], "kiro");
        for anthropic_field in [
            "type",
            "display_name",
            "created_at",
            "capabilities",
            "max_input_tokens",
            "max_tokens",
        ] {
            assert!(!entry.contains_key(anthropic_field), "{anthropic_field}");
        }
        assert!(body.get("first_id").is_none());
        assert!(body.get("has_more").is_none());
        assert!(body.get("last_id").is_none());
    }

    #[tokio::test]
    async fn legacy_kiro_model_list_respects_api_key_model_scope() {
        let mut config = Config::default();
        config.models.dynamic_discovery = false;
        config.api_key.push(ApiKeyConfig {
            id: Some("ak_model_scope".into()),
            name: "model-scope".into(),
            key: "sk-model-scope".into(),
            format: ApiKeyFormat::Sk,
            enabled: true,
            skip_user_agent_check: false,
            credits_limit: None,
            allowed_providers: Vec::new(),
            allowed_models: vec!["kiro/claude-sonnet-4.6".into()],
        });
        let (_directory, state) = test_state(config).await;

        seed_catalog_account(&state, &["claude-sonnet-4.6".into()]).await;
        let response = router(state)
            .oneshot(
                Request::get("/v1/models")
                    .header(header::AUTHORIZATION, "Bearer sk-model-scope")
                    .header(header::USER_AGENT, "codex_cli_rs/0.147.0 (test)")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        let models = body["data"].as_array().expect("model data");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["id"], "claude-sonnet-4.6");
    }

    #[tokio::test]
    async fn unidentified_model_list_merges_openai_and_anthropic_fields() {
        let (_directory, state) = test_state(Config::default()).await;
        state.models.finish_refresh(vec![
            discovered_model("deepseek-3.2", "DeepSeek 3.2", Some(200_000), Some(16_384)),
            discovered_model(
                "claude-sonnet-5",
                "Claude Sonnet 5",
                Some(1_000_000),
                Some(64_000),
            ),
        ]);
        seed_catalog_account(
            &state,
            &["deepseek-3.2".to_string(), "claude-sonnet-5".to_string()],
        )
        .await;

        let response = router(Arc::clone(&state))
            .oneshot(
                Request::get("/v1/models")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body.as_object().expect("list response").len(), 5);
        assert_eq!(body["object"], "list");
        assert_eq!(body["first_id"], "anthropic.deepseek-3.2");
        assert_eq!(body["last_id"], "claude-sonnet-5");
        assert_eq!(body["has_more"], false);
        assert_eq!(body["data"].as_array().expect("model data").len(), 2);
        let entry = body["data"][0].as_object().expect("model object");
        assert_eq!(entry.len(), 10);
        assert_eq!(entry["id"], "anthropic.deepseek-3.2");
        assert_eq!(entry["object"], "model");
        assert!(entry["created"].as_i64().is_some());
        assert_eq!(entry["owned_by"], "kiro");
        assert_eq!(entry["type"], "model");
        assert!(entry["capabilities"].is_null());
        assert_eq!(entry["created_at"], "1970-01-01T00:00:00Z");
        assert_eq!(entry["display_name"], "DeepSeek 3.2");
        assert_eq!(entry["max_input_tokens"], 200_000);
        assert_eq!(entry["max_tokens"], 16_384);

        let paged = router(Arc::clone(&state))
            .oneshot(
                Request::get("/models?limit=1")
                    .header(header::USER_AGENT, "curl/8.0")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(paged.status(), StatusCode::OK);
        let paged = body_json(paged).await;
        assert_eq!(paged["data"].as_array().expect("model data").len(), 1);
        assert_eq!(paged["has_more"], true);

        let anthropic = router(state)
            .oneshot(
                Request::get("/v1/models?limit=1")
                    .header("anthropic-version", "2023-06-01")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(anthropic.status(), StatusCode::OK);
        let anthropic = body_json(anthropic).await;
        assert!(anthropic.get("object").is_none());
        assert_eq!(anthropic["data"][0]["type"], "model");
        assert!(anthropic["data"][0].get("object").is_none());
    }

    #[tokio::test]
    async fn anthropic_model_list_matches_schema_and_paginates_by_response_id() {
        let (_directory, state) = test_state(Config::default()).await;
        state.models.finish_refresh(vec![
            discovered_model("deepseek-3.2", "DeepSeek 3.2", Some(200_000), Some(16_384)),
            discovered_model("claude-sonnet-5", "Claude Sonnet 5", None, None),
            discovered_model(
                "minimax-m2.5",
                "MiniMax M2.5",
                Some(1_000_000),
                Some(64_000),
            ),
        ]);
        seed_catalog_account(
            &state,
            &[
                "deepseek-3.2".to_string(),
                "claude-sonnet-5".to_string(),
                "minimax-m2.5".to_string(),
            ],
        )
        .await;

        let first = router(Arc::clone(&state))
            .oneshot(
                Request::get("/v1/models?limit=1")
                    .header(header::USER_AGENT, "claude-code/2.1.263")
                    .header("anthropic-version", "2023-06-01")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(first.status(), StatusCode::OK);
        let first = body_json(first).await;
        assert!(first.get("object").is_none());
        assert_eq!(first.as_object().expect("list response").len(), 4);
        assert_eq!(first["first_id"], "anthropic.deepseek-3.2");
        assert_eq!(first["last_id"], "anthropic.deepseek-3.2");
        assert_eq!(first["has_more"], true);
        let entry = first["data"][0].as_object().expect("model object");
        assert_eq!(entry.len(), 7);
        assert_eq!(entry["type"], "model");
        assert_eq!(entry["id"], "anthropic.deepseek-3.2");
        assert_eq!(entry["display_name"], "DeepSeek 3.2");
        assert_eq!(entry["created_at"], "1970-01-01T00:00:00Z");
        assert!(entry["capabilities"].is_null());
        assert_eq!(entry["max_input_tokens"], 200_000);
        assert_eq!(entry["max_tokens"], 16_384);
        for openai_or_extension_field in ["object", "created", "owned_by", "description"] {
            assert!(
                !entry.contains_key(openai_or_extension_field),
                "{openai_or_extension_field}"
            );
        }

        let after = router(Arc::clone(&state))
            .oneshot(
                Request::get("/v1/models?limit=1&after_id=anthropic.deepseek-3.2")
                    .header(header::USER_AGENT, "claude-code/2.1.263")
                    .header("anthropic-version", "2023-06-01")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let after = body_json(after).await;
        assert_eq!(after["data"][0]["id"], "claude-sonnet-5");
        assert!(after["data"][0]["max_input_tokens"].is_null());
        assert!(after["data"][0]["max_tokens"].is_null());
        assert_eq!(after["has_more"], true);

        let before = router(state)
            .oneshot(
                Request::get("/v1/models?limit=1&before_id=anthropic.minimax-m2.5")
                    .header(header::USER_AGENT, "claude-code/2.1.263")
                    .header("anthropic-version", "2023-06-01")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let before = body_json(before).await;
        assert_eq!(before["data"][0]["id"], "claude-sonnet-5");
        assert_eq!(before["has_more"], true);
    }

    #[tokio::test]
    async fn anthropic_model_list_rejects_invalid_pagination() {
        let (_directory, state) = test_state(Config::default()).await;
        state.models.finish_refresh(vec![discovered_model(
            "claude-sonnet-5",
            "Claude Sonnet 5",
            None,
            None,
        )]);

        for query in [
            "limit=0",
            "limit=1001",
            "limit=invalid",
            "after_id=claude-sonnet-5&before_id=claude-sonnet-5",
            "after_id=missing",
        ] {
            let response = router(Arc::clone(&state))
                .oneshot(
                    Request::get(format!("/v1/models?{query}"))
                        .header(header::USER_AGENT, "claude-code/2.1.263")
                        .header("anthropic-version", "2023-06-01")
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
            let body = body_json(response).await;
            assert_eq!(body["type"], "error");
            assert_eq!(body["error"]["type"], "invalid_request_error");
        }
    }

    #[tokio::test]
    async fn anthropic_model_list_defaults_to_twenty_results() {
        let (_directory, state) = test_state(Config::default()).await;
        let model_ids = (0..21)
            .map(|index| format!("model-{index:02}"))
            .collect::<Vec<_>>();
        state.models.finish_refresh(
            model_ids
                .iter()
                .map(|id| discovered_model(id, id, None, None))
                .collect(),
        );
        seed_catalog_account(&state, &model_ids).await;

        let response = router(state)
            .oneshot(
                Request::get("/v1/models")
                    .header(header::USER_AGENT, "claude-code/2.1.263")
                    .header("anthropic-version", "2023-06-01")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["data"].as_array().expect("model data").len(), 20);
        assert_eq!(body["first_id"], "anthropic.model-00");
        assert_eq!(body["last_id"], "anthropic.model-19");
        assert_eq!(body["has_more"], true);
    }

    #[tokio::test]
    async fn client_allowlist_follows_protocol_for_every_alias_and_policy_scope() {
        for (case, globally_enforced, service_bypass, api_key_bypass, policy_enforced) in [
            ("enforced", true, false, false, true),
            ("global-disabled", false, false, false, false),
            ("service-bypass", true, true, false, false),
            ("api-key-bypass", true, false, true, false),
        ] {
            let mut config = Config::default();
            config.server.enforce_user_agent_check = globally_enforced;
            config.api_key.push(ApiKeyConfig {
                id: Some("ak_user_agent".into()),
                name: "user-agent".into(),
                key: "sk-user-agent".into(),
                format: ApiKeyFormat::Sk,
                enabled: true,
                skip_user_agent_check: api_key_bypass,
                credits_limit: None,
                ..ApiKeyConfig::default()
            });
            let service = ProxyServiceConfig {
                id: "svc_user_agent".into(),
                name: "user-agent".into(),
                host: "127.0.0.1".into(),
                port: 5581,
                enabled: true,
                skip_user_agent_check: service_bypass,
                api_key_ids: vec!["ak_user_agent".into()],
                account_tag: None,
                account_ids: Vec::new(),
                excluded_account_ids: Vec::new(),
                created_at: 0,
                ..ProxyServiceConfig::default()
            };
            config.proxy_service.push(service.clone());
            let (_directory, state) = test_state(config).await;
            for path in [
                "/v1/messages",
                "/messages",
                "/anthropic/v1/messages",
                "/v1/messages/count_tokens",
                "/messages/count_tokens",
                "/anthropic/v1/messages/count_tokens",
                "/v1/chat/completions",
                "/chat/completions",
                "/v1/responses",
                "/responses",
                "/v1/models",
                "/models",
            ] {
                let claude = path.contains("messages");
                let models = path.ends_with("models");
                for (client, agent) in [
                    ("claude", Some("claude-cli/2.1.235 (external, test)")),
                    ("claude", Some("claude-code/2.1.263")),
                    ("codex", Some("codex_cli_rs/0.147.0 (test)")),
                    ("other", Some("curl/8.0 codex_cli_rs/0.147.0")),
                    ("other", Some("curl/8.0 claude-code/2.1.263")),
                    ("other", Some("claude-code/")),
                    ("other", Some("claude-code/invalid")),
                    ("other", Some("claude-code/2.1.263 impersonator")),
                    ("missing", None),
                ] {
                    let mut request = Request::builder()
                        .method(if models { Method::GET } else { Method::POST })
                        .uri(path)
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(header::AUTHORIZATION, "Bearer sk-user-agent")
                        .header("originator", "codex_cli_rs");
                    if let Some(agent) = agent {
                        request = request.header(header::USER_AGENT, agent);
                    }
                    let body = if path.contains("responses") {
                        serde_json::json!({"model":"test","input":"hello"})
                    } else {
                        serde_json::json!({"model":"test","messages":[{"role":"user","content":"hello"}],"max_tokens":1})
                    };
                    let response = router_for_service(Arc::clone(&state), service.clone(), true)
                        .oneshot(request.body(Body::from(body.to_string())).unwrap())
                        .await
                        .unwrap();
                    let allowed = !policy_enforced
                        || models
                        || (claude && client == "claude")
                        || (!claude && client == "codex");
                    let expected = if !allowed {
                        StatusCode::BAD_REQUEST
                    } else if models || path.ends_with("count_tokens") {
                        StatusCode::OK
                    } else {
                        StatusCode::SERVICE_UNAVAILABLE
                    };
                    assert_eq!(
                        response.status(),
                        expected,
                        "case={case}, path={path}, client={client}"
                    );
                    assert!(response.headers().contains_key("request-id"));
                    let body = body_json(response).await;
                    if !allowed {
                        assert_eq!(body["error"]["type"], "invalid_request_error");
                        assert_eq!(body.get("type").is_some(), claude);
                        assert!(body["error"]["message"]
                            .as_str()
                            .unwrap()
                            .contains(if claude { "Claude Code" } else { "Codex" }));
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn user_agent_bypass_still_requires_a_valid_service_api_key() {
        let mut config = Config::default();
        config.api_key.push(ApiKeyConfig {
            id: Some("ak_bypass".into()),
            name: "bypass".into(),
            key: "sk-bypass".into(),
            format: ApiKeyFormat::Sk,
            enabled: true,
            skip_user_agent_check: true,
            credits_limit: None,
            ..ApiKeyConfig::default()
        });
        config.api_key.push(ApiKeyConfig {
            id: Some("ak_unbound_bypass".into()),
            name: "unbound-bypass".into(),
            key: "sk-unbound-bypass".into(),
            format: ApiKeyFormat::Sk,
            enabled: true,
            skip_user_agent_check: true,
            credits_limit: None,
            ..ApiKeyConfig::default()
        });
        let service = ProxyServiceConfig {
            id: "svc_bypass".into(),
            name: "bypass".into(),
            host: "127.0.0.1".into(),
            port: 5581,
            enabled: true,
            skip_user_agent_check: true,
            api_key_ids: vec!["ak_bypass".into()],
            account_tag: None,
            account_ids: Vec::new(),
            excluded_account_ids: Vec::new(),
            created_at: 0,
            ..ProxyServiceConfig::default()
        };
        config.proxy_service.push(service.clone());
        let (_directory, state) = test_state(config).await;

        for credential in ["invalid", "sk-unbound-bypass"] {
            for path in ["/v1/messages", "/v1/responses", "/v1/models"] {
                let request = Request::builder()
                    .method(if path.ends_with("models") {
                        Method::GET
                    } else {
                        Method::POST
                    })
                    .uri(path)
                    .header(header::AUTHORIZATION, format!("Bearer {credential}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({"model":"test","messages":[],"input":"hello"})
                            .to_string(),
                    ))
                    .expect("request");
                let response = router_for_service(Arc::clone(&state), service.clone(), true)
                    .oneshot(request)
                    .await
                    .expect("response");
                assert_eq!(
                    response.status(),
                    StatusCode::UNAUTHORIZED,
                    "credential={credential}, path={path}"
                );
            }
        }
    }

    #[tokio::test]
    async fn codex_products_are_recognized_without_accepting_substring_matches() {
        let mut config = Config::default();
        config.models.dynamic_discovery = false;
        let (_directory, state) = test_state(config).await;
        seed_catalog_account(&state, &["claude-sonnet-4.6".into()]).await;
        for agent in [
            "codex_cli_rs/0.147.0 (Mac OS 26.0; arm64) Terminal/1.0",
            "codex_exec/0.147.0",
            "codex_vscode/0.147.0",
            "codex-tui/0.147.0",
            "Codex/1.2026.901",
            "Codex Desktop/1.2026.901",
            "Codex App/1.2026.901",
        ] {
            let response = router(Arc::clone(&state))
                .oneshot(
                    Request::get("/v1/models")
                        .header(header::USER_AGENT, agent)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{agent}");
            let body = body_json(response).await;
            assert_eq!(body["object"], "list", "{agent}");
            assert!(body.get("first_id").is_none(), "{agent}");
            assert!(body["data"][0].get("type").is_none(), "{agent}");
        }
        for agent in [
            "not-codex/1.0",
            "codex_cli_rs/",
            "codex_cli_rs/invalid",
            "Mozilla/5.0 (Codex)",
            "codex_cli_rs-impersonator/1.0",
        ] {
            let response = router(Arc::clone(&state))
                .oneshot(
                    Request::get("/v1/models")
                        .header(header::USER_AGENT, agent)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{agent}");
            let body = body_json(response).await;
            assert_eq!(body["object"], "list", "{agent}");
            assert!(body.get("first_id").is_some(), "{agent}");
            assert_eq!(body["data"][0]["type"], "model", "{agent}");
        }
    }

    #[tokio::test]
    async fn claude_model_discovery_preserves_codex_ids_and_exposes_alias_metadata() {
        let mut config = Config::default();
        config.models.dynamic_discovery = false;
        let (_directory, state) = test_state(config).await;
        state
            .pool()
            .replace_accounts(vec![account_with_usage(
                "acc_stale",
                "stale@example.com",
                true,
                None,
            )])
            .await;
        state
            .pool()
            .get("acc_stale")
            .await
            .expect("stale account")
            .set_supported_models(["stale-dynamic-only".to_string()])
            .await;
        for (agent, expected) in [
            (
                "claude-cli/2.1.235 (external, test)",
                "anthropic.deepseek-3.2",
            ),
            ("claude-code/2.1.263", "anthropic.deepseek-3.2"),
            ("codex_cli_rs/0.147.0 (test)", "deepseek-3.2"),
        ] {
            let response = router(Arc::clone(&state))
                .oneshot(
                    Request::get("/v1/models?limit=1000")
                        .header(header::USER_AGENT, agent)
                        .header("anthropic-version", "2023-06-01")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = body_json(response).await;
            let entry = body["data"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["id"] == expected)
                .expect("model alias");
            if agent.starts_with("claude-") {
                assert_eq!(entry["display_name"], "DeepSeek 3.2");
                assert_eq!(entry["type"], "model");
                assert_eq!(entry["created_at"], "1970-01-01T00:00:00Z");
                assert!(entry["capabilities"].is_null());
                assert!(entry["max_input_tokens"].is_null());
                assert!(entry["max_tokens"].is_null());
                assert!(entry.get("description").is_none());
                assert_eq!(body["has_more"], false);
            }
        }
    }

    #[tokio::test]
    async fn claude_output_hints_pass_validation_for_generation_and_token_counting() {
        let (_directory, state) = test_state(Config::default()).await;
        for (path, stream) in [
            "/v1/messages",
            "/messages",
            "/anthropic/v1/messages",
            "/v1/messages/count_tokens",
            "/messages/count_tokens",
            "/anthropic/v1/messages/count_tokens",
        ]
        .into_iter()
        .flat_map(|path| [false, true].map(|stream| (path, stream)))
        {
            for config in [
                serde_json::json!({"format":{"type":"json_schema","schema":{"type":"object"}}}),
                serde_json::json!({"format":{"type":"text"}}),
                serde_json::json!({"task_budget":{"type":"tokens","total":64000}}),
            ] {
                let response = router(Arc::clone(&state)).oneshot(Request::post(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::USER_AGENT, "claude-cli/2.1.235 (external, test)")
                    .body(Body::from(serde_json::json!({"model":"claude-opus-5", "max_tokens":4096,
                        "stream":stream, "messages":[{"role":"user","content":"hello"}], "output_config":config
                    }).to_string())).unwrap()).await.unwrap();
                // Generation reaches account selection (this fixture has no
                // accounts); token counting completes entirely in the gateway.
                let counting = path.ends_with("count_tokens");
                assert_eq!(
                    response.status(),
                    if counting || cfg!(feature = "crs") && stream {
                        StatusCode::OK
                    } else {
                        StatusCode::SERVICE_UNAVAILABLE
                    },
                    "path={path}, stream={stream}, output_config={config}"
                );
                let body = if cfg!(feature = "crs") && stream && !counting {
                    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
                    let text = std::str::from_utf8(&bytes).unwrap();
                    text.lines()
                        .filter_map(|line| line.strip_prefix("data: "))
                        .map(|data| serde_json::from_str::<serde_json::Value>(data).unwrap())
                        .find(|event| event["type"] == "error")
                        .expect("SSE error event")
                } else {
                    body_json(response).await
                };
                if counting {
                    assert!(body["input_tokens"].as_u64().is_some());
                } else {
                    assert_eq!(body["type"], "error");
                    assert_ne!(body["error"]["type"], "invalid_request_error");
                }
            }
        }
    }

    #[tokio::test]
    async fn responses_methods_and_validation_use_openai_errors() {
        let (_directory, state) = test_state(Config::default()).await;
        for path in ["/v1/responses", "/responses"] {
            let response = router(Arc::clone(&state))
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
            assert_eq!(response.headers()[header::ALLOW], "POST");
            assert!(body_json(response).await.get("type").is_none());
            let response = router(Arc::clone(&state)).oneshot(Request::post(path)
                .header(header::USER_AGENT, "codex_cli_rs/0.147.0")
                .body(Body::from(r#"{"model":"test","input":"hello","previous_response_id":"resp_unknown"}"#)).unwrap()).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let error = body_json(response).await;
            assert_eq!(error["error"]["type"], "invalid_request_error");
            assert_eq!(
                error["error"]["message"],
                "unknown or expired previous_response_id"
            );
        }
    }

    #[tokio::test]
    async fn openai_overflow_remains_a_hard_context_error_when_auto_compact_is_enabled() {
        let mut config = Config::default();
        config.context.max_input_tokens = 1_000;
        config.context.auto_compact_on_overflow = true;
        let (_directory, state) = test_state(config).await;
        install_context_test_account(&state, "mapped-small").await;
        let response = router(state)
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header(header::USER_AGENT, "codex_cli_rs/0.147.0 (test)")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "model":"mapped-small",
                            "messages":[{"role":"user","content":"large input ".repeat(2000)}],
                            "max_tokens":1
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert!(body["error"]["message"]
            .as_str()
            .expect("message")
            .contains("prompt is too long"));
    }

    #[tokio::test]
    async fn api_key_challenge_and_bodyless_event_logging_are_stable() {
        let mut config = Config::default();
        config.api_key.push(ApiKeyConfig {
            id: Some("ak_test".into()),
            name: "test".into(),
            key: "sk-secret".into(),
            format: ApiKeyFormat::Sk,
            enabled: true,
            skip_user_agent_check: false,
            credits_limit: None,
            ..ApiKeyConfig::default()
        });
        let (_directory, state) = test_state(config).await;
        let unauthorized = router(Arc::clone(&state))
            .oneshot(
                Request::post("/api/event_logging/batch")
                    .body(Body::from("not-json"))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            unauthorized
                .headers()
                .get(header::WWW_AUTHENTICATE)
                .expect("challenge"),
            "Bearer"
        );

        let accepted = router(state)
            .oneshot(
                Request::post("/api/event_logging/batch")
                    .header(header::AUTHORIZATION, "Bearer sk-secret")
                    .body(Body::from("not-json-and-never-parsed"))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(accepted.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn failed_request_stats_hide_untrusted_models_and_bound_valid_aliases() {
        let mut config = Config::default();
        config.api_key.push(ApiKeyConfig {
            id: Some("ak_test".into()),
            name: "test".into(),
            key: "sk-secret".into(),
            format: ApiKeyFormat::Sk,
            enabled: true,
            skip_user_agent_check: false,
            credits_limit: None,
            ..ApiKeyConfig::default()
        });
        let (_directory, state) = test_state(config).await;
        let attacker_model = "attacker-controlled-model";
        let unauthorized = router(Arc::clone(&state))
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header(header::USER_AGENT, "codex_cli_rs/0.147.0 (test)")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({"model": attacker_model, "messages": []}).to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        let unauthorized_trace = unauthorized
            .headers()
            .get(TRACE_ID_HEADER)
            .expect("trace header")
            .to_str()
            .expect("trace header text")
            .to_owned();

        let rejected_user_agent_model = "rejected-user-agent-model";
        let rejected_user_agent = router(Arc::clone(&state))
            .oneshot(
                Request::post("/v1/messages")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, "Bearer sk-secret")
                    .body(Body::from(
                        serde_json::json!({
                            "model": rejected_user_agent_model,
                            "messages": []
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(rejected_user_agent.status(), StatusCode::BAD_REQUEST);

        for long_model in ["界".repeat(200), "x".repeat(200)] {
            let invalid = router(Arc::clone(&state))
                .oneshot(
                    Request::post("/v1/chat/completions")
                        .header(header::USER_AGENT, "codex_cli_rs/0.147.0 (test)")
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(header::AUTHORIZATION, "Bearer sk-secret")
                        .body(Body::from(
                            serde_json::json!({"model": long_model, "messages": []}).to_string(),
                        ))
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        }

        let snapshot = state.stats.snapshot(None);
        assert!(snapshot.by_model.contains_key("unknown"));
        assert!(!snapshot.by_model.contains_key(attacker_model));
        assert!(!snapshot.by_model.contains_key(rejected_user_agent_model));
        assert!(!snapshot.by_model.contains_key(&"界".repeat(128)));
        assert!(snapshot.by_model.contains_key("[invalid-model]"));
        assert!(snapshot.by_model.contains_key(&"x".repeat(128)));
        assert!(snapshot
            .recent_requests
            .iter()
            .all(|request| request.model.chars().count() <= 128));
        assert!(snapshot
            .recent_requests
            .iter()
            .any(|request| request.trace_id == unauthorized_trace));
    }

    #[tokio::test]
    async fn cors_preflight_and_count_tokens_follow_the_public_contract() {
        let (_directory, state) = test_state(Config::default()).await;
        let preflight = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/v1/messages")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("preflight");
        assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            preflight
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .expect("cors"),
            "*"
        );

        let counted = router(Arc::clone(&state))
            .oneshot(
                Request::post("/v1/messages/count_tokens")
                    .header(header::USER_AGENT, "claude-cli/1.0 (external, test)")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"model":"model","messages":[{"role":"user","content":"hello"}]}"#,
                    ))
                    .expect("request"),
            )
            .await
            .expect("count");
        assert_eq!(counted.status(), StatusCode::OK);
        assert!(body_json(counted).await["input_tokens"]
            .as_u64()
            .is_some_and(|tokens| tokens > 0));

        let invalid = router(Arc::clone(&state))
            .oneshot(
                Request::post("/v1/messages/count_tokens")
                    .header(header::USER_AGENT, "claude-cli/1.0 (external, test)")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .expect("request"),
            )
            .await
            .expect("invalid");
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        let stats = state.stats.snapshot(None);
        assert_eq!(stats.total.requests, 2);
        assert_eq!(stats.total.successes, 1);
        assert_eq!(stats.total.failures, 1);
    }

    #[tokio::test]
    async fn count_tokens_applies_existing_compaction_without_triggering_a_new_one() {
        let (_directory, state) = test_state(Config::default()).await;
        let old_context = "old context ".repeat(30_000);
        let request = serde_json::json!({
            "model":"model",
            "messages":[
                {"role":"user","content":old_context.clone()},
                {"role":"assistant","content":[
                    {"type":"compaction","content":"durable summary"},
                    {"type":"text","text":"continued response"}
                ]},
                {"role":"user","content":"new request"}
            ],
            "context_management":{"edits":[{
                "type":"compact_20260112",
                "trigger":{"type":"input_tokens","value":50_000}
            }]}
        });
        let counted = router(Arc::clone(&state))
            .oneshot(
                Request::post("/v1/messages/count_tokens")
                    .header(header::USER_AGENT, "claude-cli/1.0 (external, test)")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(request.to_string()))
                    .expect("request"),
            )
            .await
            .expect("count");
        assert_eq!(counted.status(), StatusCode::OK);
        let counted = body_json(counted).await;
        assert!(
            counted["context_management"]["original_input_tokens"]
                .as_u64()
                .expect("original")
                > counted["input_tokens"].as_u64().expect("effective")
        );

        let trigger_only = serde_json::json!({
            "model":"model",
            "messages":[{"role":"user","content":old_context}],
            "context_management":{"edits":[{
                "type":"compact_next",
                "trigger":{"type":"input_tokens","value":50_000}
            }]}
        });
        let counted = router(Arc::clone(&state))
            .oneshot(
                Request::post("/v1/messages/count_tokens")
                    .header(header::USER_AGENT, "claude-cli/1.0 (external, test)")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(trigger_only.to_string()))
                    .expect("request"),
            )
            .await
            .expect("count");
        let counted = body_json(counted).await;
        assert_eq!(
            counted["context_management"]["original_input_tokens"],
            counted["input_tokens"]
        );
    }

    #[tokio::test]
    async fn count_tokens_normalizes_failed_compaction_blocks() {
        let (_directory, state) = test_state(Config::default()).await;
        let effective = serde_json::json!({
            "model":"model",
            "messages":[
                {"role":"user","content":"first"},
                {"role":"user","content":"second"}
            ]
        });
        let with_null = serde_json::json!({
            "model":"model",
            "messages":[
                {"role":"user","content":"first"},
                {"role":"assistant","content":[{
                    "type":"compaction","content":null
                }]},
                {"role":"user","content":"second"}
            ]
        });
        let with_missing = serde_json::json!({
            "model":"model",
            "messages":[
                {"role":"user","content":"first"},
                {"role":"assistant","content":[{"type":"compaction"}]},
                {"role":"user","content":"second"}
            ]
        });

        let mut counts = Vec::new();
        for request in [effective, with_null, with_missing] {
            let response = router(Arc::clone(&state))
                .oneshot(
                    Request::post("/v1/messages/count_tokens")
                        .header(header::USER_AGENT, "claude-cli/1.0 (external, test)")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(request.to_string()))
                        .expect("request"),
                )
                .await
                .expect("count");
            assert_eq!(response.status(), StatusCode::OK);
            counts.push(body_json(response).await["input_tokens"].clone());
        }
        assert_eq!(counts[0], counts[1]);
        assert_eq!(counts[0], counts[2]);

        let empty = serde_json::json!({
            "model":"model",
            "messages":[
                {"role":"user","content":"first"},
                {"role":"assistant","content":[{
                    "type":"compaction","content":""
                }]},
                {"role":"user","content":"second"}
            ]
        });
        let response = router(Arc::clone(&state))
            .oneshot(
                Request::post("/v1/messages/count_tokens")
                    .header(header::USER_AGENT, "claude-cli/1.0 (external, test)")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(empty.to_string()))
                    .expect("request"),
            )
            .await
            .expect("count");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(body_json(response).await["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("must not be empty")));
    }

    #[tokio::test]
    async fn count_tokens_accepts_claude_code_context_edits() {
        let (_directory, state) = test_state(Config::default()).await;
        let clear_thinking = serde_json::json!({
            "model":"claude-opus-5",
            "messages":[{"role":"user","content":"hello"}],
            "context_management":{"edits":[{
                "type":"clear_thinking_20251015",
                "keep":"all"
            }]}
        });
        let counted = router(Arc::clone(&state))
            .oneshot(
                Request::post("/v1/messages/count_tokens")
                    .header(header::USER_AGENT, "claude-cli/2.1.220 (external, test)")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("anthropic-beta", "context-management-2025-06-27")
                    .body(Body::from(clear_thinking.to_string()))
                    .expect("request"),
            )
            .await
            .expect("count");
        assert_eq!(counted.status(), StatusCode::OK);
        let counted = body_json(counted).await;
        assert_eq!(
            counted["context_management"]["original_input_tokens"],
            counted["input_tokens"]
        );

        let clear_tools = serde_json::json!({
            "model":"claude-opus-5",
            "messages":[
                {"role":"user","content":"read the file"},
                {"role":"assistant","content":[{
                    "type":"tool_use","id":"toolu_old","name":"Read",
                    "input":{"path":"/tmp/large"}
                }]},
                {"role":"user","content":[{
                    "type":"tool_result","tool_use_id":"toolu_old",
                    "content":"old output ".repeat(10_000)
                }]},
                {"role":"user","content":"continue"}
            ],
            "context_management":{"edits":[{
                "type":"clear_tool_uses_20250919",
                "trigger":{"type":"tool_uses","value":1},
                "keep":{"type":"tool_uses","value":0},
                "clear_tool_inputs":true
            }]}
        });
        let counted = router(Arc::clone(&state))
            .oneshot(
                Request::post("/v1/messages/count_tokens")
                    .header(header::USER_AGENT, "claude-cli/2.1.220 (external, test)")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("anthropic-beta", "context-management-2025-06-27")
                    .body(Body::from(clear_tools.to_string()))
                    .expect("request"),
            )
            .await
            .expect("count");
        assert_eq!(counted.status(), StatusCode::OK);
        let counted = body_json(counted).await;
        assert!(
            counted["context_management"]["original_input_tokens"]
                .as_u64()
                .expect("original")
                > counted["input_tokens"].as_u64().expect("effective")
        );
    }

    #[tokio::test]
    async fn user_agent_policy_hot_reload_keeps_the_proxy_listener_running() {
        let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port");
        let port = probe.local_addr().expect("address").port();
        drop(probe);
        let mut config = Config::default();
        config.api_key.push(ApiKeyConfig {
            id: Some("ak_hot_policy".into()),
            name: "hot-policy".into(),
            key: "sk-hot-policy".into(),
            format: ApiKeyFormat::Sk,
            enabled: true,
            skip_user_agent_check: false,
            credits_limit: None,
            ..ApiKeyConfig::default()
        });
        config.proxy_service.push(ProxyServiceConfig {
            id: "svc_hot_policy".into(),
            name: "hot-policy".into(),
            host: "127.0.0.1".into(),
            port,
            enabled: true,
            skip_user_agent_check: false,
            api_key_ids: vec!["ak_hot_policy".into()],
            account_tag: None,
            account_ids: Vec::new(),
            excluded_account_ids: Vec::new(),
            created_at: 0,
            ..ProxyServiceConfig::default()
        });
        let (_directory, state) = test_state(config.clone()).await;
        assert!(state.reconcile_proxy_services(&config).await.is_empty());
        let listener_cancel = state
            .proxy_services
            .running
            .lock()
            .await
            .get("svc_hot_policy")
            .expect("running service")
            .cancel
            .clone();
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/v1/responses");
        let request = serde_json::json!({"model":"test","input":"hello"});

        let rejected = client
            .post(&url)
            .bearer_auth("sk-hot-policy")
            .header(header::USER_AGENT, "curl/8.0")
            .json(&request)
            .send()
            .await
            .expect("strict request");
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);

        let mut service_bypass = config.clone();
        service_bypass.proxy_service[0].skip_user_agent_check = true;
        state
            .apply_config_transaction(&service_bypass)
            .await
            .expect("service policy reload");
        assert!(!listener_cancel.is_cancelled());
        let service_allowed = client
            .post(&url)
            .bearer_auth("sk-hot-policy")
            .header(header::USER_AGENT, "curl/8.0")
            .json(&request)
            .send()
            .await
            .expect("service bypass request");
        assert_eq!(service_allowed.status(), StatusCode::SERVICE_UNAVAILABLE);

        let mut key_bypass = config.clone();
        key_bypass.api_key[0].skip_user_agent_check = true;
        state
            .apply_config_transaction(&key_bypass)
            .await
            .expect("API key policy reload");
        assert!(!listener_cancel.is_cancelled());
        let key_allowed = client
            .post(&url)
            .bearer_auth("sk-hot-policy")
            .header(header::USER_AGENT, "curl/8.0")
            .json(&request)
            .send()
            .await
            .expect("API key bypass request");
        assert_eq!(key_allowed.status(), StatusCode::SERVICE_UNAVAILABLE);

        state.shutdown.cancel();
        state
            .proxy_services
            .reconcile(Arc::clone(&state), &[])
            .await;
    }

    #[tokio::test]
    async fn reconcile_restarts_a_finished_proxy_listener() {
        let (_directory, state) = test_state(Config::default()).await;
        let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port");
        let port = probe.local_addr().expect("address").port();
        drop(probe);
        let service = ProxyServiceConfig {
            id: "svc_restart".into(),
            name: "restart".into(),
            host: "127.0.0.1".into(),
            port,
            enabled: true,
            skip_user_agent_check: false,
            api_key_ids: Vec::new(),
            account_tag: None,
            account_ids: Vec::new(),
            excluded_account_ids: Vec::new(),
            created_at: 0,
            ..ProxyServiceConfig::default()
        };
        let finished = tokio::spawn(async {});
        while !finished.is_finished() {
            tokio::task::yield_now().await;
        }
        state.proxy_services.running.lock().await.insert(
            service.id.clone(),
            RunningService {
                config: service.clone(),
                port,
                cancel: tokio_util::sync::CancellationToken::new(),
                task: finished,
                error: Arc::new(RwLock::new(Some("accept loop failed".into()))),
            },
        );

        let failures = state
            .proxy_services
            .reconcile(Arc::clone(&state), std::slice::from_ref(&service))
            .await;
        assert!(failures.is_empty(), "{failures:?}");
        let mut config = state.config.current().as_ref().clone();
        config.proxy_service.push(service.clone());
        state.config.replace(config);
        let views = state.proxy_services.views(&state.config.current()).await;
        assert!(views[0].running);
        let health = reqwest::get(format!("http://127.0.0.1:{port}/health"))
            .await
            .expect("restarted listener");
        assert_eq!(health.status(), StatusCode::OK);

        state.shutdown.cancel();
        state
            .proxy_services
            .reconcile(Arc::clone(&state), &[])
            .await;
    }
}
