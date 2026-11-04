//! Authenticated REST facade for the existing Unix-socket control plane.

mod accounts;
mod batches;
mod enterprise;
mod openapi;
mod services;
mod views;

pub(crate) use enterprise::handle_rpc as handle_kiro_rpc;

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{DefaultBodyLimit, MatchedPath, Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use crosslane_core::config::{AdminRestConfig, AdminTokenRole};
use crosslane_ipc::protocol::{method, Request as RpcRequest, Response as RpcResponse, RpcError};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::state::AppState;

#[derive(Clone)]
pub(super) struct RestState {
    app: Arc<AppState>,
    requests: Arc<Semaphore>,
    account_creates: Arc<Semaphore>,
    batches: Arc<batches::BatchRegistry>,
    enterprise: Arc<enterprise::Control>,
}

/// Shared across listener generations so rebinding retains jobs, limits and
/// canonical-user exclusion for operations still running on retired sockets.
pub(crate) struct SharedState {
    requests: Arc<Semaphore>,
    account_creates: Arc<Semaphore>,
    batches: Arc<batches::BatchRegistry>,
    enterprise: Arc<enterprise::Control>,
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            requests: Arc::new(Semaphore::new(16)),
            account_creates: Arc::new(Semaphore::new(8)),
            batches: Arc::new(batches::BatchRegistry::default()),
            enterprise: Arc::new(enterprise::Control::default()),
        }
    }
}

type ApiResult<T = Value> = Result<T, ApiError>;

#[derive(Clone)]
struct RequestAudit {
    request_id: String,
    token_id: String,
    role: &'static str,
    source: &'static str,
}

impl RestState {
    fn new(app: Arc<AppState>) -> Self {
        Self {
            requests: Arc::clone(&app.admin_rest.requests),
            account_creates: Arc::clone(&app.admin_rest.account_creates),
            batches: Arc::clone(&app.admin_rest.batches),
            enterprise: Arc::clone(&app.admin_rest.enterprise),
            app,
        }
    }
}

pub(crate) fn router(app: Arc<AppState>) -> Router {
    let state = RestState::new(app);
    Router::new()
        .nest("/kiro/enterprise", enterprise::router())
        .route("/openapi.json", get(openapi::schema))
        .route("/accounts", get(accounts::list).post(accounts::create))
        .route("/account-batches", get(batches::list).post(batches::create))
        .route("/account-batches/:batch", get(batches::show))
        .route("/account-batches/:batch/cancel", post(batches::cancel))
        .route(
            "/accounts/:provider/:account",
            get(accounts::show).delete(accounts::remove),
        )
        .route(
            "/accounts/:provider/:account/enabled",
            post(accounts::set_enabled),
        )
        .route("/accounts/:provider/:account/tags", post(accounts::tags))
        .route(
            "/accounts/:provider/:account/refresh",
            post(accounts::refresh),
        )
        .route("/accounts/:provider/:account/probe", post(accounts::probe))
        .route(
            "/accounts/:provider/:account/reset-health",
            post(accounts::reset_health),
        )
        .route(
            "/accounts/:provider/:account/services",
            get(accounts::services),
        )
        .route("/services", get(services::list).post(services::create))
        .route(
            "/services/:service",
            get(services::show)
                .patch(services::set_enabled)
                .delete(services::remove),
        )
        .route(
            "/services/:service/accounts",
            get(services::accounts)
                .post(services::bind_accounts)
                .delete(services::unbind_accounts),
        )
        .route("/services/:service/api-keys", get(services::api_keys))
        .route("/providers", get(providers))
        .route("/providers/:provider", get(provider))
        .route("/models", get(models))
        .route("/runtime/status", get(status))
        .route("/runtime/stats", get(stats))
        .route("/runtime/tasks", get(tasks))
        .route("/runtime/tasks/:task/run", post(run_task))
        .route("/api-keys", get(api_keys))
        .route("/api-keys/:key/reset-usage", post(reset_key_usage))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .with_state(state)
}

struct TokenIdentity {
    id: String,
    role: AdminTokenRole,
}

fn token_identity(auth: &AdminRestConfig, token: &str) -> Option<TokenIdentity> {
    let digest = format!("{:x}", Sha256::digest(token.as_bytes()));
    let matches =
        |expected: &str| crate::meter::constant_time_eq(&expected.to_ascii_lowercase(), &digest);
    if !auth.token_sha256.is_empty() && matches(&auth.token_sha256) {
        return Some(TokenIdentity {
            id: "legacy-admin".into(),
            role: AdminTokenRole::Admin,
        });
    }
    if auth
        .read_token_sha256
        .as_ref()
        .is_some_and(|hash| matches(hash))
    {
        return Some(TokenIdentity {
            id: "legacy-read-only".into(),
            role: AdminTokenRole::ReadOnly,
        });
    }
    auth.tokens
        .iter()
        .find(|entry| entry.enabled && matches(&entry.token_sha256))
        .map(|entry| TokenIdentity {
            id: entry.id.clone(),
            role: entry.role,
        })
}

async fn authenticate(
    State(state): State<RestState>,
    mut request: Request,
    next: Next,
) -> Response {
    let started = Instant::now();
    let request_id = format!("admin_{}", Uuid::new_v4().simple());
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_owned())
        .unwrap_or_else(|| "/admin/v1/*".into());
    let method = request.method().clone();
    let config = request
        .extensions()
        .get::<crate::status_http::ListenerConfig>()
        .map(|snapshot| Arc::clone(&snapshot.0))
        .unwrap_or_else(|| state.app.config.current());
    let auth = &config.admin.rest;
    let identity = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token)
        .filter(|value| {
            !value.is_empty() && value.len() <= 4096 && !value.chars().any(char::is_whitespace)
        })
        .and_then(|token| token_identity(auth, token));
    let role = identity.as_ref().map(|identity| identity.role);
    let mut response = if !auth.enabled {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "admin REST API is disabled",
        )
        .into_response()
    } else if role.is_none() {
        let mut response = ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "valid admin Bearer token required",
        )
        .into_response();
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Bearer realm=\"crosslane-admin\""),
        );
        response
    } else if role == Some(AdminTokenRole::ReadOnly)
        && !matches!(method, Method::GET | Method::HEAD)
    {
        ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "read-only token cannot perform this operation",
        )
        .into_response()
    } else if let Ok(_permit) = state.requests.clone().try_acquire_owned() {
        if let Some(identity) = &identity {
            request.extensions_mut().insert(RequestAudit {
                request_id: request_id.clone(),
                token_id: identity.id.clone(),
                role: identity.role.as_str(),
                source: "rest",
            });
        }
        next.run(request).await
    } else {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            "admin request limit reached",
        )
        .into_response()
    };
    if response.status() == StatusCode::METHOD_NOT_ALLOWED {
        let allow = response.headers().get(header::ALLOW).cloned();
        response = method_not_allowed().into_response();
        if let Some(allow) = allow {
            response.headers_mut().insert(header::ALLOW, allow);
        }
    }
    response.headers_mut().insert(
        "x-request-id",
        HeaderValue::from_str(&request_id).expect("generated request ID"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    // Route templates and fixed role labels only: never log credentials, bodies, or query strings.
    tracing::info!(event = "admin.http.request", %request_id, method = %method, %route,
        role = role.map(AdminTokenRole::as_str).unwrap_or("unauthenticated"),
        token_id=identity.as_ref().map(|identity| identity.id.as_str()).unwrap_or("unauthenticated"), status = response.status().as_u16(),
        duration_ms = started.elapsed().as_millis() as u64, "admin REST request completed");
    response
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
    fn bad(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }
    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }
    fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "management operation failed",
        )
    }
}

impl From<RpcError> for ApiError {
    fn from(error: RpcError) -> Self {
        let status = if error.code == 400
            && error
                .message
                .contains("service account pool is unavailable")
        {
            StatusCode::SERVICE_UNAVAILABLE
        } else if error.code == 400
            && (error.message.contains("not found:")
                || error.message.starts_with("unknown provider"))
        {
            StatusCode::NOT_FOUND
        } else if error.code == 400
            && (error.message.contains("bound to proxy service")
                || error.message.contains("already exists")
                || error.message.contains("failed to start")
                || error.message.contains("update rejected"))
        {
            StatusCode::CONFLICT
        } else {
            u16::try_from(error.code)
                .ok()
                .and_then(|code| StatusCode::from_u16(code).ok())
                .filter(|code| code.is_client_error() || code.is_server_error())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
        };
        let code = match status {
            StatusCode::NOT_FOUND => "not_found",
            StatusCode::CONFLICT => "conflict",
            StatusCode::BAD_GATEWAY => "upstream_failed",
            status if status.is_server_error() => "operation_failed",
            _ => "invalid_request",
        };
        Self::new(
            status,
            code,
            crosslane_translate::sanitize_error_message(&error.message),
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({"error":{"code":self.code,"message":self.message}})),
        )
            .into_response()
    }
}

fn ok(data: impl serde::Serialize) -> Response {
    Json(json!({"data":data})).into_response()
}
fn created(data: impl serde::Serialize) -> Response {
    (StatusCode::CREATED, Json(json!({"data":data}))).into_response()
}

async fn rpc(app: &Arc<AppState>, name: &str, params: Value) -> ApiResult {
    match super::handlers::dispatch(app, RpcRequest::new(0, name, params)).await {
        RpcResponse::Ok { result, .. } => Ok(result),
        RpcResponse::Err { error, .. } => Err(error.into()),
    }
}

async fn body<T: DeserializeOwned>(request: Request) -> ApiResult<T> {
    use axum::extract::FromRequest;
    Json::<T>::from_request(request, &())
        .await
        .map(|Json(value)| value)
        .map_err(|rejection| {
            let status = if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                StatusCode::PAYLOAD_TOO_LARGE
            } else {
                StatusCode::BAD_REQUEST
            };
            ApiError::new(
                status,
                "invalid_request",
                if status == StatusCode::PAYLOAD_TOO_LARGE {
                    "request body exceeds 2 MiB"
                } else {
                    "invalid JSON body, content type, or request fields"
                },
            )
        })
}

fn query<T: DeserializeOwned>(request: &Request) -> ApiResult<T> {
    axum::extract::Query::<T>::try_from_uri(request.uri())
        .map(|query| query.0)
        .map_err(|_| ApiError::bad("invalid query parameters"))
}

async fn not_found() -> ApiError {
    ApiError::not_found("admin route not found")
}
fn method_not_allowed() -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "HTTP method is not supported for this resource",
    )
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderQuery {
    provider: Option<String>,
}

async fn providers(State(state): State<RestState>, request: Request) -> ApiResult<Response> {
    let filter: ProviderQuery = query(&request)?;
    Ok(ok(rpc(
        &state.app,
        method::V2_PROVIDER_LIST,
        json!({"provider":filter.provider.unwrap_or_else(|| "all".into())}),
    )
    .await?))
}

async fn provider(
    State(state): State<RestState>,
    axum::extract::Path(provider): axum::extract::Path<String>,
) -> ApiResult<Response> {
    Ok(ok(rpc(
        &state.app,
        method::V2_PROVIDER_SHOW,
        json!({"provider":provider}),
    )
    .await?))
}

async fn models(State(state): State<RestState>, request: Request) -> ApiResult<Response> {
    let filter: ProviderQuery = query(&request)?;
    Ok(ok(rpc(
        &state.app,
        method::V2_MODELS,
        json!({"provider":filter.provider.unwrap_or_else(|| "all".into())}),
    )
    .await?))
}

async fn status(State(state): State<RestState>, request: Request) -> ApiResult<Response> {
    let filter: ProviderQuery = query(&request)?;
    Ok(ok(rpc(
        &state.app,
        method::STATUS,
        json!({"provider":filter.provider}),
    )
    .await?))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct StatsQuery {
    provider: Option<String>,
    since_secs: Option<u64>,
    start_secs: Option<i64>,
    end_secs: Option<i64>,
    #[serde(default)]
    detail: bool,
}

async fn stats(State(state): State<RestState>, request: Request) -> ApiResult<Response> {
    let filter: StatsQuery = query(&request)?;
    Ok(ok(rpc(&state.app, method::STATS, json!({"provider":filter.provider,"since_secs":filter.since_secs,"start_secs":filter.start_secs,"end_secs":filter.end_secs,"detail":filter.detail})).await?))
}

async fn tasks(State(state): State<RestState>) -> ApiResult<Response> {
    Ok(ok(rpc(&state.app, method::TASKS, Value::Null).await?))
}
async fn run_task(
    State(state): State<RestState>,
    axum::extract::Path(task): axum::extract::Path<String>,
    request: Request,
) -> ApiResult<Response> {
    let filter: ProviderQuery = query(&request)?;
    Ok(ok(rpc(
        &state.app,
        method::TASK_RUN,
        json!({"name":task,"provider":filter.provider}),
    )
    .await?))
}
async fn api_keys(State(state): State<RestState>, request: Request) -> ApiResult<Response> {
    let filter: ProviderQuery = query(&request)?;
    Ok(ok(rpc(
        &state.app,
        method::APIKEY_LIST,
        json!({"provider":filter.provider}),
    )
    .await?))
}
async fn reset_key_usage(
    State(state): State<RestState>,
    axum::extract::Path(key): axum::extract::Path<String>,
) -> ApiResult<Response> {
    Ok(ok(rpc(
        &state.app,
        method::APIKEY_RESET_USAGE,
        json!({"id":key}),
    )
    .await?))
}
