use std::sync::Arc;
use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use crosslane_core::config::{Config, ProxyServiceConfig};
use crosslane_core::provider::{ProviderId, ProviderModel, ProviderProtocol};
use crosslane_provider::{ProviderError, ProviderRequest, ProviderResponseBody};
use crosslane_translate::model::{
    map_model_for_provider, resolve_model_with_family_fallback, ModelMappingContext, ModelRoute,
};
use futures::StreamExt;
use serde_json::Value;

use super::{ApiError, ErrorFormat, RequestDiagnostics, RequestLog, ServiceHttpState};
use crate::meter::{AuthenticatedApiKey, CreditReservation, ProviderBilling, UsageRecord};
use crate::state::{AdmissionGuard, AppState};

#[derive(Clone)]
pub(super) struct SelectedProviderRoute {
    pub provider_id: ProviderId,
    pub provider_kind: String,
    pub original_model: String,
    pub upstream_model: String,
    pub mapping_rule: Option<String>,
    /// A provider-aware rule already selected this Kiro model. The legacy
    /// executor must not draw or apply the rule a second time.
    pub lock_kiro_mapping: bool,
    pub enable_model_fallback: bool,
    /// An actual search owns its Kiro continuation and must fail without fallback.
    pub web_search_operation: bool,
    pub failover_timeout_ms: u64,
    pub body: Bytes,
    pub automatic: bool,
    pub account_pool: Option<ProxyServiceConfig>,
    pub account_ids: Option<Arc<std::collections::HashSet<String>>>,
}

pub(super) fn request_provider_hint(
    config: &Config,
    service: &ProxyServiceConfig,
    body: &[u8],
) -> String {
    let default_provider = config.default_provider_for_service(service);
    let model = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    model
        .as_deref()
        .and_then(|model| model.split_once('/').map(|(provider, _)| provider))
        .filter(|provider| {
            config
                .effective_providers()
                .iter()
                .any(|candidate| candidate.id == *provider)
        })
        .unwrap_or(default_provider)
        .to_owned()
}

/// Resolves an explicit `provider/model` prefix or the service default, then
/// applies the provider-scoped mapping engine and both authorization layers.
pub(super) fn select_provider_route(
    config: &Config,
    service: &ProxyServiceConfig,
    authenticated_key: Option<&AuthenticatedApiKey>,
    body: &Bytes,
    format: ErrorFormat,
) -> Result<SelectedProviderRoute, ApiError> {
    let default_provider = config.default_provider_for_service(service);
    let mut value: Value = serde_json::from_slice(body).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "Invalid JSON in request body",
            format,
        )
        .with_provider_id(default_provider)
    })?;
    let object = value.as_object_mut().ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "request body must be a JSON object",
            format,
        )
        .with_provider_id(default_provider)
    })?;
    let original_model = object
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ApiError::new(StatusCode::BAD_REQUEST, "model is required", format)
                .with_provider_id(default_provider)
        })?
        .trim()
        .to_owned();
    if original_model.is_empty() {
        return Err(
            ApiError::new(StatusCode::BAD_REQUEST, "model must not be empty", format)
                .with_provider_id(default_provider),
        );
    }

    let providers = config.effective_providers();
    let explicit = original_model.split_once('/').and_then(|(prefix, model)| {
        providers
            .iter()
            .any(|provider| provider.id == prefix)
            .then_some((prefix, model))
    });
    let (initial_provider, unqualified_model) =
        explicit.unwrap_or((default_provider, original_model.as_str()));
    ensure_model_enabled(config, unqualified_model, format)?;
    authorize_provider(config, service, authenticated_key, initial_provider, format)?;
    let provider = providers
        .iter()
        .find(|provider| provider.id == initial_provider)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("unknown provider {initial_provider}"),
                format,
            )
            .with_provider_id(initial_provider)
        })?;
    if !provider.enabled {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("provider {} is disabled", provider.id),
            format,
        )
        .with_provider_id(initial_provider));
    }

    // The compatibility Kiro executor still applies its mapping after account
    // selection so quota-scoped rules retain their exact historical behavior.
    // Do not even sample the shared mapping engine for Kiro unless a rule can
    // leave this provider; pure Kiro load-balancing must be evaluated exactly
    // once by the legacy account-aware path.
    let has_cross_provider_rule = config.model_mapping.iter().any(|rule| {
        rule.enabled
            && (if rule.providers.is_empty() {
                initial_provider == "kiro"
            } else {
                rule.providers
                    .iter()
                    .any(|provider| provider == initial_provider)
            })
            && rule.target_models.iter().any(|target| {
                target.split_once('/').is_some_and(|(target_provider, _)| {
                    target_provider != initial_provider
                        && providers
                            .iter()
                            .any(|provider| provider.id == target_provider)
                })
            })
    });
    // New provider adapters use the shared provider-aware engine here.
    let route = if provider.kind == "kiro" && has_cross_provider_rule {
        let candidate = map_model_for_provider(
            unqualified_model,
            &config.model_mapping,
            ModelMappingContext {
                provider_id: initial_provider,
                service_id: Some(&service.id),
                api_key_id: authenticated_key.map(|key| key.id.as_str()),
                remaining_percent: None,
            },
            "",
        );
        let crosses_provider = candidate.mapped.split_once('/').is_some_and(|(target, _)| {
            target != initial_provider && providers.iter().any(|provider| provider.id == target)
        });
        let rule_can_cross_provider = candidate.rule_index.is_some_and(|index| {
            config.model_mapping[index]
                .target_models
                .iter()
                .any(|target| {
                    target.split_once('/').is_some_and(|(target_provider, _)| {
                        target_provider != initial_provider
                            && providers
                                .iter()
                                .any(|provider| provider.id == target_provider)
                    })
                })
        });
        if crosses_provider || rule_can_cross_provider {
            candidate
        } else {
            ModelRoute {
                original: unqualified_model.into(),
                mapped: unqualified_model.into(),
                rule: None,
                rule_index: None,
            }
        }
    } else if provider.kind == "kiro" {
        ModelRoute {
            original: unqualified_model.into(),
            mapped: unqualified_model.into(),
            rule: None,
            rule_index: None,
        }
    } else {
        map_model_for_provider(
            unqualified_model,
            &config.model_mapping,
            ModelMappingContext {
                provider_id: initial_provider,
                service_id: Some(&service.id),
                api_key_id: authenticated_key.map(|key| key.id.as_str()),
                remaining_percent: None,
            },
            &provider.routing.default_model_id,
        )
    };
    let mut selected_provider = initial_provider;
    let mut upstream_model = route.mapped.as_str();
    if let Some((target_provider, target_model)) = route.mapped.split_once('/') {
        if providers
            .iter()
            .any(|provider| provider.id == target_provider)
        {
            if target_provider != initial_provider
                && !provider.routing.allow_cross_provider_fallback
            {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "model mapping {} crosses from {} to {} but cross-provider routing is disabled",
                        route.rule.as_deref().unwrap_or("<unnamed>"),
                        initial_provider,
                        target_provider
                    ),
                    format,
                )
                .with_provider_id(initial_provider));
            }
            selected_provider = target_provider;
            upstream_model = target_model;
            authorize_provider(
                config,
                service,
                authenticated_key,
                selected_provider,
                format,
            )?;
        }
    }
    let selected = providers
        .iter()
        .find(|provider| provider.id == selected_provider)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("unknown provider {selected_provider}"),
                format,
            )
            .with_provider_id(selected_provider)
        })?;
    ensure_model_enabled(config, upstream_model, format)?;
    // Kiro resolves after account selection; Copilot resolves against its live
    // catalog before execution. Authorize their final wire models in those
    // paths; other adapters finish routing here.
    if !matches!(selected.kind.as_str(), "kiro" | "copilot") {
        authorize_model(authenticated_key, selected_provider, upstream_model, format)?;
    }
    if !selected.enabled {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("provider {} is disabled", selected.id),
            format,
        )
        .with_provider_id(selected_provider));
    }
    let lock_kiro_mapping = selected.kind == "kiro"
        && (selected_provider != initial_provider
            || route.rule_index.is_some_and(|index| {
                config.model_mapping[index]
                    .target_models
                    .iter()
                    .any(|target| {
                        target.split_once('/').is_some_and(|(target_provider, _)| {
                            target_provider != initial_provider
                                && providers
                                    .iter()
                                    .any(|provider| provider.id == target_provider)
                        })
                    })
            }));
    object.insert("model".into(), Value::String(upstream_model.to_owned()));
    let body = serde_json::to_vec(&value)
        .map(Bytes::from)
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to serialize routed request: {error}"),
                format,
            )
            .with_provider_id(selected_provider)
        })?;
    Ok(SelectedProviderRoute {
        provider_id: ProviderId::parse(selected_provider).map_err(|error| {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string(), format)
                .with_provider_id(selected_provider)
        })?,
        provider_kind: selected.kind.clone(),
        original_model,
        upstream_model: upstream_model.to_owned(),
        mapping_rule: route.rule,
        lock_kiro_mapping,
        enable_model_fallback: selected.routing.enable_model_fallback,
        web_search_operation: false,
        failover_timeout_ms: selected.routing.failover_timeout_ms,
        body,
        automatic: false,
        account_pool: None,
        account_ids: None,
    })
}

pub(super) fn ensure_model_enabled(
    config: &Config,
    model: &str,
    format: ErrorFormat,
) -> Result<(), ApiError> {
    let unqualified = config.unqualified_model(model);
    if config.models.is_disabled(unqualified) {
        let mut error = ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("model_disabled: model {unqualified} is globally disabled"),
            format,
        );
        error.error_code = "model_disabled";
        error.error_stage = "model_policy";
        return Err(error);
    }
    Ok(())
}

fn resolve_copilot_route(
    route: &mut SelectedProviderRoute,
    models: &[ProviderModel],
    config: &Config,
    authenticated_key: Option<&AuthenticatedApiKey>,
    format: ErrorFormat,
) -> Result<(), ApiError> {
    let available = models
        .iter()
        .map(|model| model.id.clone())
        .collect::<Vec<_>>();
    let resolved = resolve_model_with_family_fallback(
        &route.upstream_model,
        &available,
        route.enable_model_fallback,
    )
    .ok_or_else(|| {
        let mut error = ApiError::new(
            StatusCode::BAD_REQUEST,
            format!(
                "model_not_available: model {} has no unambiguous match in provider {}'s catalog",
                route.upstream_model, route.provider_id
            ),
            format,
        )
        .with_provider_id(route.provider_id.as_str());
        error.error_code = "model_not_available";
        error.error_stage = "model_resolution";
        error
    })?;
    ensure_model_enabled(config, &resolved, format)?;
    authorize_model(
        authenticated_key,
        route.provider_id.as_str(),
        &resolved,
        format,
    )?;
    let mut value: Value = serde_json::from_slice(&route.body).map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to decode routed request: {error}"),
            format,
        )
    })?;
    value["model"] = Value::String(resolved.clone());
    route.body = serde_json::to_vec(&value)
        .map(Bytes::from)
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to serialize resolved request: {error}"),
                format,
            )
        })?;
    route.upstream_model = resolved;
    Ok(())
}

fn authorize_provider(
    config: &Config,
    service: &ProxyServiceConfig,
    authenticated_key: Option<&AuthenticatedApiKey>,
    provider_id: &str,
    format: ErrorFormat,
) -> Result<(), ApiError> {
    let service_allowed = config.allowed_providers_for_service(service);
    let key_allowed = authenticated_key.map_or_else(
        || service_allowed.clone(),
        |key| effective_scope(&key.allowed_providers),
    );
    if !service_allowed.contains(&provider_id) || !key_allowed.contains(&provider_id) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            format!("provider {provider_id} is not allowed for this service and API key"),
            format,
        )
        .with_provider_id(provider_id));
    }
    Ok(())
}

pub(super) fn authorize_model(
    authenticated_key: Option<&AuthenticatedApiKey>,
    provider_id: &str,
    model: &str,
    format: ErrorFormat,
) -> Result<(), ApiError> {
    let Some(key) = authenticated_key else {
        return Ok(());
    };
    if key.allowed_models.is_empty() {
        return Ok(());
    }
    let qualified = format!("{provider_id}/{model}");
    if super::model_is_allowed(&key.allowed_models, provider_id, model) {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::FORBIDDEN,
            format!("model {qualified} is not allowed for this API key"),
            format,
        )
        .with_provider_id(provider_id))
    }
}

fn effective_scope(values: &[String]) -> Vec<&str> {
    if values.is_empty() {
        vec!["kiro"]
    } else {
        values.iter().map(String::as_str).collect()
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_provider(
    service: ServiceHttpState,
    mut route: SelectedProviderRoute,
    protocol: ProviderProtocol,
    path: String,
    headers: HeaderMap,
    trace_id: String,
    authenticated_key: Option<AuthenticatedApiKey>,
    connection_guard: AdmissionGuard,
    admission_guard: AdmissionGuard,
    format: ErrorFormat,
    count_tokens: bool,
    client_context: crosslane_core::client_context::ClientContext,
) -> Result<Response, ApiError> {
    let started = Instant::now();
    let api_key_id = authenticated_key.as_ref().map(|key| key.id.clone());
    let user_agent = super::logged_user_agent(&headers);
    let error_provider = route.provider_id.to_string();
    let stores_response = protocol == ProviderProtocol::OpenAiResponses
        && super::super::client_context::stores_response(&route.body);
    let session_owner = super::super::responses::ResponsesSessionOwner::new(
        service.service.id.clone(),
        api_key_id.clone(),
    );
    let adapter = service
        .app
        .providers
        .registry()
        .get(&route.provider_id)
        .await
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("provider {} is not registered", route.provider_id),
                format,
            )
            .with_provider_id(&error_provider)
        })?;
    if route.provider_kind == "copilot" && !route.automatic {
        let models = adapter.models(false).await.map_err(|error| {
            provider_api_error(
                error,
                format,
                &error_provider,
                &route.original_model,
                &route.upstream_model,
                route.mapping_rule.as_deref(),
            )
        })?;
        resolve_copilot_route(
            &mut route,
            &models,
            &service.app.config.current(),
            authenticated_key.as_ref(),
            format,
        )?;
    }
    let reservation = service
        .app
        .meter
        .reserve(api_key_id.as_deref(), 0.0)
        .map_err(|error| super::meter_error(error, format).with_provider_id(&error_provider))?;
    let request = ProviderRequest {
        protocol,
        model: route.upstream_model.clone(),
        original_model: Some(
            service
                .app
                .config
                .current()
                .unqualified_model(&route.original_model)
                .to_owned(),
        ),
        path: path.clone(),
        headers,
        body: route.body,
        trace_id: trace_id.clone(),
        service_id: service.service.id.clone(),
        account_pool: Some(
            route
                .account_pool
                .clone()
                .unwrap_or_else(|| service.current_service()),
        ),
        api_key_id: api_key_id.clone(),
        client_context: client_context.clone(),
    };
    let error_model = route.upstream_model.clone();
    let error_original_model = route.original_model.clone();
    let error_mapping_rule = route.mapping_rule.clone();
    #[cfg(feature = "crs")]
    let result =
        super::super::crs::execute_with_effort_recovery(&adapter, request, count_tokens).await;
    #[cfg(not(feature = "crs"))]
    let result = if count_tokens {
        adapter.count_tokens(request).await
    } else {
        adapter.execute(request).await
    };
    let mut response = result.map_err(|error| {
        provider_api_error(
            error,
            format,
            &error_provider,
            &error_original_model,
            &error_model,
            error_mapping_rule.as_deref(),
        )
    })?;
    // Native adapters stream JSON bodies too. Inspect a bounded JSON response
    // before exposing its HTTP 200, so a protocol error can still fail over.
    // Larger responses keep streaming with their original bytes and permit.
    if route.automatic
        && response.status.is_success()
        && response
            .headers
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/json"))
    {
        response.body = inspect_json_response(response.body)
            .await
            .map_err(|error| {
                provider_api_error(
                    error,
                    format,
                    &error_provider,
                    &error_original_model,
                    &error_model,
                    error_mapping_rule.as_deref(),
                )
            })?;
    }
    // Failed attempts in an automatic route are recorded by the coordinator,
    // not as additional client requests. Keep pinned-provider responses intact.
    let protocol_error = if route.automatic && response.status.is_success() {
        match &response.body {
            ProviderResponseBody::Full(bytes) => ProviderUsage::from_bytes(bytes).error,
            _ => None,
        }
    } else {
        None
    };
    if route.automatic
        && (super::provider_balance::failover_status(response.status) || protocol_error.is_some())
    {
        let mut error = provider_api_error(
            ProviderError::new(
                crosslane_provider::ProviderErrorKind::Upstream,
                if protocol_error.is_some() {
                    StatusCode::BAD_GATEWAY
                } else {
                    response.status
                },
                protocol_error.unwrap_or_else(|| {
                    format!(
                        "provider {} returned HTTP {}",
                        route.provider_id,
                        response.status.as_u16()
                    )
                }),
            ),
            format,
            &error_provider,
            &error_original_model,
            &error_model,
            error_mapping_rule.as_deref(),
        );
        error.account_error = true;
        error.log_context.account_id = response.account_id.clone().unwrap_or_default();
        return Err(error);
    }
    let status = response.status;
    let account_id = response.account_id.unwrap_or_default();
    let request_id = response
        .upstream_request_id
        .unwrap_or_else(|| trace_id.clone());
    let mut builder = Response::builder().status(status);
    for (name, value) in &response.headers {
        builder = builder.header(name, value);
    }
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        builder = builder.header("request-id", value);
    }
    let state = Arc::clone(&service.app);
    let metadata = ProviderRequestMetadata {
        provider_id: route.provider_id.to_string(),
        original_model: route.original_model,
        model: route.upstream_model,
        mapping_rule: route.mapping_rule,
        account_id,
        path,
        trace_id,
        request_id,
        user_agent,
        status: status.as_u16(),
        started,
        api_key_id,
        client_context,
        session_owner,
        stores_response,
    };
    let body = match response.body {
        ProviderResponseBody::Full(bytes) => {
            let usage = ProviderUsage::from_bytes(&bytes);
            record_provider_request(&state, &metadata, usage, None, reservation).await;
            drop((connection_guard, admission_guard));
            Body::from(bytes)
        }
        ProviderResponseBody::Stream(mut upstream) => {
            let stream = async_stream::stream! {
                let _connection_guard = connection_guard;
                let _admission_guard = admission_guard;
                let mut stream_error = None;
                let mut usage = ProviderUsageScanner::default();
                while let Some(chunk) = upstream.next().await {
                    match chunk {
                        Ok(bytes) => {
                            usage.push(&bytes);
                            // Persist attribution before the completed event
                            // reaches the client: it can immediately issue a
                            // continuation without waiting for transport EOF.
                            remember_provider_client_context(&state, &metadata, &mut usage.usage, metadata.status);
                            yield Ok::<_, ProviderError>(bytes)
                        },
                        Err(error) => {
                            stream_error = Some(error.to_string());
                            yield Err(error);
                            break;
                        }
                    }
                }
                record_provider_request(
                    &state,
                    &metadata,
                    usage.finish(),
                    stream_error,
                    reservation,
                )
                .await;
            };
            Body::from_stream(stream)
        }
    };
    builder.body(body).map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to construct provider response: {error}"),
            format,
        )
        .with_provider_id(&error_provider)
    })
}

async fn inspect_json_response(
    body: ProviderResponseBody,
) -> Result<ProviderResponseBody, ProviderError> {
    const INSPECTION_LIMIT: usize = 1024 * 1024;
    let ProviderResponseBody::Stream(mut upstream) = body else {
        return Ok(body);
    };
    let mut buffered = Vec::new();
    while let Some(chunk) = upstream.next().await {
        let bytes = chunk?;
        if buffered.len().saturating_add(bytes.len()) > INSPECTION_LIMIT {
            let stream = async_stream::try_stream! {
                if !buffered.is_empty() {
                    yield Bytes::from(buffered);
                }
                yield bytes;
                while let Some(chunk) = upstream.next().await {
                    yield chunk?;
                }
            };
            return Ok(ProviderResponseBody::Stream(Box::pin(stream)));
        }
        buffered.extend_from_slice(&bytes);
    }
    Ok(ProviderResponseBody::Full(Bytes::from(buffered)))
}

struct ProviderRequestMetadata {
    provider_id: String,
    original_model: String,
    model: String,
    mapping_rule: Option<String>,
    account_id: String,
    path: String,
    trace_id: String,
    request_id: String,
    user_agent: String,
    status: u16,
    started: Instant,
    api_key_id: Option<String>,
    client_context: crosslane_core::client_context::ClientContext,
    session_owner: super::super::responses::ResponsesSessionOwner,
    stores_response: bool,
}

#[derive(Clone, Debug, Default)]
struct ProviderUsage {
    reported: bool,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    reasoning_tokens: u64,
    billing: Option<ProviderBilling>,
    error: Option<String>,
    completed_response_id: Option<String>,
}

impl ProviderUsage {
    fn from_bytes(bytes: &[u8]) -> Self {
        serde_json::from_slice::<Value>(bytes)
            .ok()
            .map_or_else(Self::default, |value| Self::from_value(&value))
    }

    fn from_value(value: &Value) -> Self {
        // Local token estimates are returned to the caller, but are not
        // provider-reported generation usage.
        if value.get("estimated").and_then(Value::as_bool) == Some(true) {
            return Self::default();
        }
        let mut usage = Self {
            billing: copilot_billing(value),
            error: provider_response_error(value),
            completed_response_id: completed_response_id(value),
            ..Self::default()
        };
        if [
            value.get("crosslane_usage_source"),
            value.pointer("/message/crosslane_usage_source"),
            value.pointer("/response/crosslane_usage_source"),
        ]
        .into_iter()
        .flatten()
        .any(|source| source == "unreported")
        {
            return usage;
        }
        usage.merge_object(value);
        for candidate in [
            value.get("usage"),
            value.pointer("/message/usage"),
            value.pointer("/response/usage"),
        ]
        .into_iter()
        .flatten()
        .filter(|candidate| candidate.is_object())
        {
            usage.reported = true;
            usage.merge_object(candidate);
        }
        usage
    }

    fn merge_object(&mut self, value: &Value) {
        let number = |names: &[&str]| {
            names
                .iter()
                .find_map(|name| value.get(name).and_then(Value::as_u64))
        };
        if let Some(tokens) = number(&["input_tokens", "prompt_tokens"]) {
            self.reported = true;
            self.input_tokens = self.input_tokens.max(tokens);
        }
        if let Some(tokens) = number(&["output_tokens", "completion_tokens"]) {
            self.reported = true;
            self.output_tokens = self.output_tokens.max(tokens);
        }
        if let Some(tokens) = number(&[
            "cache_read_input_tokens",
            "cache_read_tokens",
            "cached_tokens",
        ]) {
            self.reported = true;
            self.cache_read_tokens = self.cache_read_tokens.max(tokens);
        }
        if let Some(tokens) = number(&["cache_creation_input_tokens", "cache_write_tokens"]) {
            self.reported = true;
            self.cache_write_tokens = self.cache_write_tokens.max(tokens);
        }
        if let Some(tokens) = number(&["reasoning_tokens"]) {
            self.reported = true;
            self.reasoning_tokens = self.reasoning_tokens.max(tokens);
        }
        if let Some(details) = value
            .get("input_tokens_details")
            .or_else(|| value.get("prompt_tokens_details"))
        {
            if let Some(tokens) = details.get("cached_tokens").and_then(Value::as_u64) {
                self.reported = true;
                self.cache_read_tokens = self.cache_read_tokens.max(tokens);
            }
        }
        if let Some(details) = value
            .get("output_tokens_details")
            .or_else(|| value.get("completion_tokens_details"))
        {
            if let Some(tokens) = details.get("reasoning_tokens").and_then(Value::as_u64) {
                self.reported = true;
                self.reasoning_tokens = self.reasoning_tokens.max(tokens);
            }
        }
    }

    fn merge(&mut self, other: Self) {
        self.reported |= other.reported;
        self.input_tokens = self.input_tokens.max(other.input_tokens);
        self.output_tokens = self.output_tokens.max(other.output_tokens);
        self.cache_read_tokens = self.cache_read_tokens.max(other.cache_read_tokens);
        self.cache_write_tokens = self.cache_write_tokens.max(other.cache_write_tokens);
        self.reasoning_tokens = self.reasoning_tokens.max(other.reasoning_tokens);
        if other.billing.is_some() {
            self.billing = other.billing;
        }
        if other.error.is_some() {
            self.error = other.error;
        }
        if other.completed_response_id.is_some() {
            self.completed_response_id = other.completed_response_id;
        }
    }
}

fn completed_response_id(value: &Value) -> Option<String> {
    let response = value.get("response").unwrap_or(value);
    if response.get("store").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    if !matches!(
        response.get("status").and_then(Value::as_str),
        Some("completed" | "incomplete")
    ) {
        return None;
    }
    let id = response.get("id")?.as_str()?;
    (!id.is_empty() && id.len() <= 256).then(|| id.to_owned())
}

fn provider_response_error(value: &Value) -> Option<String> {
    let error = value
        .get("error")
        .or_else(|| value.pointer("/response/error"))
        .filter(|error| !error.is_null());
    let failed = matches!(
        value.get("type").and_then(Value::as_str),
        Some("error" | "response.failed")
    ) || value.get("status").and_then(Value::as_str) == Some("failed");
    if error.is_none() && !failed {
        return None;
    }
    Some(
        error
            .and_then(|error| {
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .or_else(|| error.as_str())
            })
            .or_else(|| value.get("message").and_then(Value::as_str))
            .unwrap_or("upstream returned a protocol error")
            .to_owned(),
    )
}

fn copilot_billing(value: &Value) -> Option<ProviderBilling> {
    let raw = value
        .get("copilot_usage")
        .or_else(|| value.pointer("/response/copilot_usage"))?
        .as_object()?;
    let raw = Value::Object(raw.clone());
    Some(ProviderBilling {
        schema_version: 1,
        source: "copilot_usage".into(),
        unit: raw
            .get("total_nano_aiu")
            .is_some()
            .then(|| "nano_aiu".into()),
        amount: raw.get("total_nano_aiu").and_then(Value::as_u64),
        raw,
    })
}

#[derive(Default)]
pub(super) struct ProviderUsageScanner {
    pending: Vec<u8>,
    event_data: Vec<u8>,
    line_overflowed: bool,
    event_overflowed: bool,
    usage: ProviderUsage,
}

impl ProviderUsageScanner {
    const MAX_LINE_BYTES: usize = 128 * 1024;
    const MAX_EVENT_BYTES: usize = 128 * 1024;

    pub(super) fn push(&mut self, bytes: &[u8]) {
        for byte in bytes {
            if self.pending.len() < Self::MAX_LINE_BYTES {
                self.pending.push(*byte);
            } else {
                self.line_overflowed = true;
            }
            if *byte == b'\n' {
                if !self.line_overflowed {
                    let line = std::mem::take(&mut self.pending);
                    self.consume_line(&line[..line.len().saturating_sub(1)]);
                } else {
                    self.pending.clear();
                    self.event_data.clear();
                    self.event_overflowed = true;
                }
                self.line_overflowed = false;
            }
        }
    }

    fn consume_line(&mut self, line: &[u8]) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            self.consume_event();
            return;
        }
        if line.starts_with(b":") {
            return;
        }
        if let Some(payload) = line.strip_prefix(b"data:") {
            let payload = payload.strip_prefix(b" ").unwrap_or(payload);
            // Some compatible upstreams omit the blank line between events.
            // A complete prior JSON value is therefore an implicit boundary;
            // an incomplete value remains a standards-compliant multiline
            // data field and is joined with a newline below.
            if !self.event_data.is_empty()
                && serde_json::from_slice::<Value>(&self.event_data).is_ok()
            {
                self.consume_event();
            }
            let extra = usize::from(!self.event_data.is_empty());
            if self
                .event_data
                .len()
                .saturating_add(payload.len())
                .saturating_add(extra)
                > Self::MAX_EVENT_BYTES
            {
                self.event_data.clear();
                self.event_overflowed = true;
                return;
            }
            if !self.event_data.is_empty() {
                self.event_data.push(b'\n');
            }
            self.event_data.extend_from_slice(payload);
            return;
        }
        // Non-SSE JSON responses are also streamed by reqwest; observe a
        // complete line without rewriting the body sent to the client.
        if let Ok(value) = serde_json::from_slice::<Value>(line) {
            self.usage.merge(ProviderUsage::from_value(&value));
        }
    }

    fn consume_event(&mut self) {
        if self.event_overflowed {
            self.event_data.clear();
            self.event_overflowed = false;
            return;
        }
        if self.event_data.is_empty() || self.event_data == b"[DONE]" {
            self.event_data.clear();
            return;
        }
        if let Ok(value) = serde_json::from_slice::<Value>(&self.event_data) {
            self.usage.merge(ProviderUsage::from_value(&value));
        }
        self.event_data.clear();
    }

    fn finish(mut self) -> ProviderUsage {
        if !self.pending.is_empty() && !self.line_overflowed {
            let pending = std::mem::take(&mut self.pending);
            self.consume_line(&pending);
        }
        self.consume_event();
        self.usage
    }

    pub(super) fn failed(self) -> bool {
        self.finish().error.is_some()
    }
}

fn remember_provider_client_context(
    state: &AppState,
    metadata: &ProviderRequestMetadata,
    usage: &mut ProviderUsage,
    status: u16,
) {
    if metadata.stores_response && (200..300).contains(&status) && usage.error.is_none() {
        if let Some(response_id) = usage.completed_response_id.take() {
            state.response_client_contexts.insert(
                metadata.session_owner.clone(),
                metadata.provider_id.clone(),
                response_id,
                metadata.client_context.clone(),
            );
        }
    }
}

async fn record_provider_request(
    state: &Arc<AppState>,
    metadata: &ProviderRequestMetadata,
    mut usage: ProviderUsage,
    stream_error: Option<String>,
    reservation: CreditReservation,
) {
    let stream_error = stream_error.or_else(|| usage.error.take());
    let status = if stream_error.is_some() && metadata.status < 400 {
        502
    } else {
        metadata.status
    };
    remember_provider_client_context(state, metadata, &mut usage, status);
    if let Err(error) = reservation
        .settle(UsageRecord {
            timestamp: crate::meter::now_secs(),
            provider_id: metadata.provider_id.clone(),
            account_id: (!metadata.account_id.is_empty()).then(|| metadata.account_id.clone()),
            model: metadata.model.clone(),
            original_model: Some(metadata.original_model.clone()),
            kiro_model: None,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            credits: 0.0,
            cache_read_tokens: Some(usage.cache_read_tokens),
            cache_write_tokens: Some(usage.cache_write_tokens),
            reasoning_tokens: Some(usage.reasoning_tokens),
            token_usage_source: if usage.reported {
                "provider"
            } else {
                "unreported"
            }
            .into(),
            provider_billing: usage.billing,
            path: metadata.path.clone(),
        })
        .await
    {
        tracing::error!(
            provider_id = %metadata.provider_id,
            api_key_id = metadata.api_key_id.as_deref().unwrap_or("anonymous"),
            user_agent = %metadata.user_agent,
            %error,
            "failed to persist provider usage"
        );
    }
    state.stats.record(RequestLog {
        timestamp: crate::meter::now_secs(),
        trace_id: metadata.trace_id.clone(),
        request_id: metadata.request_id.clone(),
        path: metadata.path.clone(),
        user_agent: metadata.user_agent.clone(),
        provider_id: metadata.provider_id.clone(),
        model: metadata.model.clone(),
        original_model: metadata.original_model.clone(),
        kiro_model: if metadata.provider_id == "kiro" {
            metadata.model.clone()
        } else {
            String::new()
        },
        account_id: format!("{}/{}", metadata.provider_id, metadata.account_id),
        account_name: metadata.account_id.clone(),
        endpoint: metadata.provider_id.clone(),
        model_path: vec![
            metadata.original_model.clone(),
            format!("{}/{}", metadata.provider_id, metadata.model),
        ],
        model_mapping_rule: metadata.mapping_rule.clone(),
        attempts: Vec::new(),
        duration_ms: metadata.started.elapsed().as_millis() as u64,
        status,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        credits: 0.0,
        error: stream_error.or_else(|| {
            (metadata.status >= 400).then(|| format!("provider returned HTTP {}", metadata.status))
        }),
        diagnostics: RequestDiagnostics {
            client_context: Some(metadata.client_context.clone()),
            client_status: status,
            upstream_status: Some(metadata.status),
            error_stage: if status >= 400 {
                "provider_upstream".into()
            } else {
                String::new()
            },
            ..RequestDiagnostics::default()
        },
    });
    tracing::info!(
        event = "proxy.response.completed",
        trace_id = %metadata.trace_id,
        request_id = %metadata.request_id,
        provider_id = %metadata.provider_id,
        account_id = %metadata.account_id,
        model = %metadata.model,
        http_status = status,
        input_tokens = usage.input_tokens,
        output_tokens = usage.output_tokens,
        duration_ms = metadata.started.elapsed().as_millis() as u64,
        "provider response completed"
    );
}

fn provider_api_error(
    error: ProviderError,
    format: ErrorFormat,
    provider_id: &str,
    original_model: &str,
    model: &str,
    mapping_rule: Option<&str>,
) -> ApiError {
    let upstream_failure = matches!(
        error.kind,
        crosslane_provider::ProviderErrorKind::Authentication
            | crosslane_provider::ProviderErrorKind::Authorization
            | crosslane_provider::ProviderErrorKind::RateLimited
            | crosslane_provider::ProviderErrorKind::Unavailable
            | crosslane_provider::ProviderErrorKind::Transport
            | crosslane_provider::ProviderErrorKind::Upstream
    );
    let upstream_status = error.status;
    let mut api = ApiError::new(upstream_status, error.message, format);
    if upstream_failure {
        api.error_stage = "upstream_dispatch";
    }
    api.retry_after = error.retryable;
    api.account_error = error.account_error;
    api.upstream_status = Some(upstream_status.as_u16());
    if error.kind == crosslane_provider::ProviderErrorKind::Overloaded {
        api.error_code = "server_overloaded";
        api.error_stage = "admission";
        api.upstream_status = None;
    }
    #[cfg(feature = "crs")]
    if format == ErrorFormat::Claude
        && matches!(
            upstream_status,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        )
    {
        api.error_code = "upstream_authentication_failed";
    }
    if error.upstream_code.as_deref() == Some("model_disabled") {
        api.error_code = "model_disabled";
        api.error_stage = "model_policy";
        api.upstream_status = None;
    }
    api.log_context.provider_id = provider_id.into();
    api.log_context.endpoint = provider_id.into();
    api.log_context.mapped_model = model.into();
    api.log_context.model_path = vec![original_model.into(), format!("{provider_id}/{model}")];
    api.log_context.model_mapping_rule = mapping_rule.map(str::to_owned);
    api
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosslane_core::config::ProviderConfig;

    #[tokio::test]
    async fn json_inspection_preserves_large_responses_and_transport_failures() {
        let pieces = vec![
            Bytes::from(vec![b'a'; 800_000]),
            Bytes::from(vec![b'b'; 800_000]),
            Bytes::from_static(b"tail"),
        ];
        let expected = pieces.concat();
        let upstream = futures::stream::iter(pieces.into_iter().map(Ok));
        let body = inspect_json_response(ProviderResponseBody::Stream(Box::pin(upstream)))
            .await
            .unwrap();
        let ProviderResponseBody::Stream(mut body) = body else {
            panic!("large responses must retain bounded streaming");
        };
        let mut actual = Vec::new();
        while let Some(chunk) = body.next().await {
            actual.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(actual, expected);

        let upstream = futures::stream::iter([
            Ok(Bytes::from_static(b"{\"error\":")),
            Err(ProviderError::new(
                crosslane_provider::ProviderErrorKind::Transport,
                StatusCode::BAD_GATEWAY,
                "connection lost",
            )),
        ]);
        let result = inspect_json_response(ProviderResponseBody::Stream(Box::pin(upstream))).await;
        assert!(matches!(result, Err(error) if error.status == StatusCode::BAD_GATEWAY));
    }

    #[cfg(feature = "crs")]
    #[test]
    fn crs_preserves_supplier_errors_without_http_status_mapping() {
        use crosslane_provider::ProviderErrorKind;

        let opaque = provider_api_error(
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                StatusCode::BAD_REQUEST,
                "Internal Server Error",
            ),
            ErrorFormat::Claude,
            "copilot",
            "model",
            "model",
            None,
        );
        assert_eq!(opaque.status, StatusCode::BAD_REQUEST);
        assert_eq!(opaque.error_code, "invalid_request");
        assert_eq!(opaque.upstream_status, Some(400));

        let actionable = provider_api_error(
            ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                StatusCode::BAD_REQUEST,
                "prompt is too long: 200001 tokens > 200000",
            ),
            ErrorFormat::Claude,
            "copilot",
            "model",
            "model",
            None,
        );
        assert_eq!(actionable.status, StatusCode::BAD_REQUEST);
    }

    fn service() -> ProxyServiceConfig {
        ProxyServiceConfig {
            web_search_provider: None,
            crs_relay: None,
            id: "svc".into(),
            name: "svc".into(),
            host: "127.0.0.1".into(),
            port: 5580,
            enabled: true,
            skip_user_agent_check: true,
            api_key_ids: vec!["ak".into()],
            default_provider: "copilot".into(),
            allowed_providers: vec!["kiro".into(), "copilot".into()],
            created_at: 0,
            account_tag: None,
            account_ids: Vec::new(),
            excluded_account_ids: Vec::new(),
        }
    }

    fn config() -> Config {
        Config {
            provider: vec![
                ProviderConfig::default(),
                ProviderConfig {
                    id: "copilot".into(),
                    kind: "copilot".into(),
                    enabled: true,
                    ..ProviderConfig::default()
                },
            ],
            ..Config::default()
        }
    }

    #[test]
    fn explicit_prefix_and_scoped_mapping_select_provider() {
        let mut config = config();
        config
            .model_mapping
            .push(crosslane_core::config::ModelMappingRule {
                name: "fast".into(),
                enabled: true,
                kind: "alias".into(),
                source_models: vec!["team-fast".into()],
                target_models: vec!["gpt-test".into()],
                providers: vec!["copilot".into()],
                service_ids: vec![],
                priority: 0,
                weights: None,
                max_remaining_credit_percent: None,
                api_key_ids: None,
                schedule: None,
            });
        let key = AuthenticatedApiKey {
            id: "ak".into(),
            skip_user_agent_check: true,
            allowed_providers: vec!["copilot".into()],
            allowed_models: vec!["copilot/*".into()],
        };
        let route = select_provider_route(
            &config,
            &service(),
            Some(&key),
            &Bytes::from_static(br#"{"model":"copilot/team-fast"}"#),
            ErrorFormat::OpenAi,
        )
        .unwrap_or_else(|_| panic!("provider route should resolve"));
        assert_eq!(route.provider_id.as_str(), "copilot");
        assert_eq!(route.upstream_model, "gpt-test");
        assert_eq!(route.mapping_rule.as_deref(), Some("fast"));
    }

    #[test]
    fn key_provider_scope_cannot_be_bypassed_by_model_prefix() {
        let key = AuthenticatedApiKey {
            id: "ak".into(),
            skip_user_agent_check: true,
            allowed_providers: vec!["kiro".into()],
            allowed_models: Vec::new(),
        };
        assert!(select_provider_route(
            &config(),
            &service(),
            Some(&key),
            &Bytes::from_static(br#"{"model":"copilot/gpt-test"}"#),
            ErrorFormat::OpenAi,
        )
        .is_err());
    }

    #[test]
    fn kiro_model_scope_is_deferred_until_account_aware_resolution() {
        let key = AuthenticatedApiKey {
            id: "ak".into(),
            skip_user_agent_check: true,
            allowed_providers: vec!["kiro".into()],
            allowed_models: vec!["kiro/claude-sonnet-*".into()],
        };
        let mut service = service();
        service.default_provider = "kiro".into();
        let route = select_provider_route(
            &config(),
            &service,
            Some(&key),
            &Bytes::from_static(br#"{"model":"team-sonnet"}"#),
            ErrorFormat::Claude,
        )
        .unwrap_or_else(|_| panic!("Kiro authorization must wait for final model resolution"));
        assert_eq!(route.upstream_model, "team-sonnet");
        assert!(super::super::model_is_allowed(
            &key.allowed_models,
            "kiro",
            "claude-sonnet-4-6"
        ));
        assert!(!super::super::model_is_allowed(
            &key.allowed_models,
            "kiro",
            "claude-opus-4-6"
        ));
    }

    #[test]
    fn explicitly_enabled_cross_provider_mapping_rechecks_target_permissions() {
        let mut config = config();
        config.provider[0].routing.allow_cross_provider_fallback = true;
        config
            .model_mapping
            .push(crosslane_core::config::ModelMappingRule {
                name: "kiro-to-copilot".into(),
                source_models: vec!["team-copilot".into()],
                target_models: vec!["copilot/gpt-test".into()],
                providers: vec!["kiro".into()],
                ..crosslane_core::config::ModelMappingRule::default()
            });
        let key = AuthenticatedApiKey {
            id: "ak".into(),
            skip_user_agent_check: true,
            allowed_providers: vec!["kiro".into(), "copilot".into()],
            allowed_models: vec!["copilot/gpt-*".into()],
        };
        let mut service = service();
        service.default_provider = "kiro".into();
        let route = match select_provider_route(
            &config,
            &service,
            Some(&key),
            &Bytes::from_static(br#"{"model":"team-copilot"}"#),
            ErrorFormat::OpenAi,
        ) {
            Ok(route) => route,
            Err(_) => panic!("explicitly allowed cross-provider route should resolve"),
        };
        assert_eq!(route.provider_id.as_str(), "copilot");
        assert_eq!(route.upstream_model, "gpt-test");

        let denied = AuthenticatedApiKey {
            allowed_providers: vec!["kiro".into()],
            ..key
        };
        assert!(select_provider_route(
            &config,
            &service,
            Some(&denied),
            &Bytes::from_static(br#"{"model":"team-copilot"}"#),
            ErrorFormat::OpenAi,
        )
        .is_err());
    }

    #[test]
    fn mixed_cross_provider_mapping_locks_a_locally_selected_kiro_target() {
        let mut config = config();
        config.provider[0].routing.allow_cross_provider_fallback = true;
        config
            .model_mapping
            .push(crosslane_core::config::ModelMappingRule {
                name: "mixed".into(),
                kind: "loadbalance".into(),
                source_models: vec!["team-mixed".into()],
                target_models: vec!["kiro/claude-sonnet-test".into(), "copilot/gpt-test".into()],
                providers: vec!["kiro".into()],
                weights: Some(vec![1, 0]),
                ..crosslane_core::config::ModelMappingRule::default()
            });
        let key = AuthenticatedApiKey {
            id: "ak".into(),
            skip_user_agent_check: true,
            allowed_providers: vec!["kiro".into(), "copilot".into()],
            allowed_models: Vec::new(),
        };
        let mut service = service();
        service.default_provider = "kiro".into();

        let route = select_provider_route(
            &config,
            &service,
            Some(&key),
            &Bytes::from_static(br#"{"model":"team-mixed"}"#),
            ErrorFormat::OpenAi,
        )
        .unwrap_or_else(|_| panic!("the local weighted target should resolve"));

        assert_eq!(route.provider_id.as_str(), "kiro");
        assert_eq!(route.upstream_model, "claude-sonnet-test");
        assert!(route.lock_kiro_mapping);
        assert_eq!(route.mapping_rule.as_deref(), Some("mixed"));
    }

    #[test]
    fn cross_provider_mapping_into_kiro_is_locked_before_legacy_execution() {
        let mut config = config();
        config.provider[1].routing.allow_cross_provider_fallback = true;
        config
            .model_mapping
            .push(crosslane_core::config::ModelMappingRule {
                name: "copilot-to-kiro".into(),
                source_models: vec!["team-kiro".into()],
                target_models: vec!["kiro/claude-sonnet-test".into()],
                providers: vec!["copilot".into()],
                ..crosslane_core::config::ModelMappingRule::default()
            });
        let key = AuthenticatedApiKey {
            id: "ak".into(),
            skip_user_agent_check: true,
            allowed_providers: vec!["kiro".into(), "copilot".into()],
            allowed_models: Vec::new(),
        };

        let route = select_provider_route(
            &config,
            &service(),
            Some(&key),
            &Bytes::from_static(br#"{"model":"team-kiro"}"#),
            ErrorFormat::OpenAi,
        )
        .unwrap_or_else(|_| panic!("cross-provider Kiro target should resolve"));

        assert_eq!(route.provider_id.as_str(), "kiro");
        assert_eq!(route.upstream_model, "claude-sonnet-test");
        assert!(route.lock_kiro_mapping);
        assert_eq!(route.mapping_rule.as_deref(), Some("copilot-to-kiro"));
    }

    #[test]
    fn legacy_kiro_route_remains_unlocked_and_unqualified() {
        let mut config = Config::default();
        config
            .model_mapping
            .push(crosslane_core::config::ModelMappingRule {
                name: "legacy-local".into(),
                kind: "loadbalance".into(),
                source_models: vec!["claude-sonnet-test".into()],
                target_models: vec!["claude-sonnet-4.6".into(), "claude-sonnet-4.5".into()],
                providers: vec!["kiro".into()],
                weights: Some(vec![1, 1]),
                ..crosslane_core::config::ModelMappingRule::default()
            });
        let service = ProxyServiceConfig {
            web_search_provider: None,
            crs_relay: None,
            id: "legacy".into(),
            name: "legacy".into(),
            host: "127.0.0.1".into(),
            port: 5580,
            enabled: true,
            skip_user_agent_check: true,
            api_key_ids: Vec::new(),
            default_provider: String::new(),
            allowed_providers: Vec::new(),
            created_at: 0,
            account_tag: None,
            account_ids: Vec::new(),
            excluded_account_ids: Vec::new(),
        };

        let route = select_provider_route(
            &config,
            &service,
            None,
            &Bytes::from_static(br#"{"model":"claude-sonnet-test"}"#),
            ErrorFormat::Claude,
        )
        .unwrap_or_else(|_| panic!("legacy Kiro route"));

        assert_eq!(route.provider_id.as_str(), "kiro");
        assert_eq!(route.original_model, "claude-sonnet-test");
        assert_eq!(route.upstream_model, "claude-sonnet-test");
        assert!(!route.lock_kiro_mapping);
        assert!(route.mapping_rule.is_none());
    }

    #[test]
    fn usage_scanner_handles_split_openai_and_anthropic_sse_events() {
        let mut scanner = ProviderUsageScanner::default();
        scanner.push(b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12,\"cache_read_input_tokens\":3}}}\n");
        scanner.push(b"data: {\"type\":\"message_delta\",\"usage\":{\"output_");
        scanner.push(b"tokens\":7}}\n");
        scanner.push(b"data: {\"usage\":{\"prompt_tokens\":15,\"completion_tokens\":9,\"completion_tokens_details\":{\"reasoning_tokens\":2}}}\n\n");
        let usage = scanner.finish();
        assert!(usage.reported);
        assert_eq!(usage.input_tokens, 15);
        assert_eq!(usage.output_tokens, 9);
        assert_eq!(usage.cache_read_tokens, 3);
        assert_eq!(usage.reasoning_tokens, 2);
    }

    #[test]
    fn usage_scanner_handles_crlf_multiline_events_and_reported_zeroes() {
        let mut scanner = ProviderUsageScanner::default();
        scanner.push(b": keepalive\r\ndata: {\"usage\":{\r\n");
        scanner.push(b"data: \"prompt_tokens\":0,\"completion_tokens\":0}}\r\n\r\n");
        let usage = scanner.finish();
        assert!(usage.reported);
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 0);
    }

    #[test]
    fn usage_scanner_keeps_copilot_billing_from_a_separate_event() {
        let mut scanner = ProviderUsageScanner::default();
        scanner.push(
            br#"data: {"usage":{"prompt_tokens":12,"completion_tokens":7}}

data: {"choices":[],"copilot_usage":{"total_nano_aiu":12345,"cost_per_batch":9}}

"#,
        );
        let usage = scanner.finish();
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.output_tokens, 7);
        let billing = usage.billing.expect("Copilot billing payload");
        assert_eq!(billing.unit.as_deref(), Some("nano_aiu"));
        assert_eq!(billing.amount, Some(12_345));
        assert_eq!(billing.raw["cost_per_batch"], 9);
    }

    #[test]
    fn usage_scanner_finds_responses_nested_copilot_billing() {
        let usage = ProviderUsage::from_bytes(
            br#"{"response":{"usage":{"input_tokens":1,"output_tokens":2},"copilot_usage":{"total_nano_aiu":3}}}"#,
        );
        assert_eq!(usage.billing.expect("nested billing").amount, Some(3));
    }

    #[test]
    fn usage_scanner_records_protocol_failures_and_excludes_local_estimates() {
        let estimate = ProviderUsage::from_bytes(br#"{"input_tokens":42,"estimated":true}"#);
        assert!(!estimate.reported);
        assert_eq!(estimate.input_tokens, 0);
        let scaffold = ProviderUsage::from_bytes(br#"{"message":{"usage":{"input_tokens":0,"output_tokens":0},"crosslane_usage_source":"unreported"},"copilot_usage":{"total_nano_aiu":8}}"#);
        assert!(!scaffold.reported);
        assert_eq!(scaffold.billing.unwrap().amount, Some(8));
        for payload in [
            r#"{"type":"error","error":{"message":"failed"}}"#,
            r#"{"type":"error","message":"failed","code":"rate_limit"}"#,
            r#"{"type":"response.failed","response":{"error":{"message":"failed"}}}"#,
        ] {
            let mut scanner = ProviderUsageScanner::default();
            scanner.push(format!("data: {payload}\n\n").as_bytes());
            assert_eq!(scanner.finish().error.as_deref(), Some("failed"));
        }
        let usage = ProviderUsage::from_bytes(
            br#"{"usage":{"prompt_tokens":12,"prompt_tokens_details":{"cached_tokens":8}}}"#,
        );
        assert_eq!(usage.cache_read_tokens, 8);
    }
}
