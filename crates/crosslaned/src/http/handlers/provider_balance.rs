//! Automatic routing of unqualified, unmapped models across Kiro and Copilot.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::StatusCode;
use axum::response::Response;
use crosslane_core::config::{Config, ProxyServiceConfig, ServiceAccountTags};
use crosslane_core::provider::{ProviderId, ProviderProtocol};
use crosslane_pool::{account_credit_state, AccountCreditState, AccountHealth};
use crosslane_translate::model::{model_glob_matches, resolve_model_alias};
use hyper::body::{Body as HttpBody, Frame, SizeHint};
use serde_json::Value;

use super::provider_gateway::{
    authorize_model, ensure_model_enabled, select_provider_route, ProviderUsageScanner,
    SelectedProviderRoute,
};
use super::{ApiError, ErrorFormat, ServiceHttpState};
use crate::meter::AuthenticatedApiKey;
use crate::provider_routing::{ProviderLease, ProviderLoad, ProviderScheduler};

pub(super) struct RoutePlan {
    routes: Vec<(SelectedProviderRoute, ProviderLoad)>,
    automatic: bool,
    model: String,
}

impl RoutePlan {
    pub(super) fn is_automatic(&self) -> bool {
        self.automatic
    }

    pub(super) fn for_token_count(mut self) -> Self {
        // Local Kiro counts cannot establish generation health or reset its
        // circuit. Native count endpoints also have independent availability.
        self.model.push_str(":count_tokens");
        self
    }
    fn pinned(route: SelectedProviderRoute) -> Self {
        let load = ProviderLoad {
            id: route.provider_id.to_string(),
            capacity: 1,
            active: 0,
        };
        Self {
            model: route.upstream_model.clone(),
            routes: vec![(route, load)],
            automatic: false,
        }
    }
}

fn provider_allowed(
    service: &ProxyServiceConfig,
    key: Option<&AuthenticatedApiKey>,
    id: &str,
) -> bool {
    let allowed = |values: &[String]| {
        if values.is_empty() {
            id == "kiro"
        } else {
            values.iter().any(|value| value == id)
        }
    };
    allowed(&service.allowed_providers) && key.is_none_or(|key| allowed(&key.allowed_providers))
}

fn has_mapping(
    config: &Config,
    service: &ProxyServiceConfig,
    key: Option<&AuthenticatedApiKey>,
    model: &str,
) -> bool {
    config.model_mapping.iter().any(|rule| {
        rule.enabled
            && rule.source_models.iter().any(|pattern| {
                model_glob_matches(pattern, model)
                    || model_glob_matches(
                        pattern,
                        model.strip_prefix("anthropic.").unwrap_or(model),
                    )
            })
            && (rule.service_ids.is_empty() || rule.service_ids.contains(&service.id))
            && rule
                .api_key_ids
                .as_ref()
                .is_none_or(|ids| ids.is_empty() || key.is_some_and(|key| ids.contains(&key.id)))
            && (if rule.providers.is_empty() {
                service.allowed_providers.is_empty()
                    || service.allowed_providers.iter().any(|id| id == "kiro")
            } else {
                rule.providers
                    .iter()
                    .any(|id| service.allowed_providers.contains(id))
            })
    })
}

pub(super) async fn plan(
    service: &ServiceHttpState,
    key: Option<&AuthenticatedApiKey>,
    body: &Bytes,
    protocol: ProviderProtocol,
    format: ErrorFormat,
) -> Result<RoutePlan, ApiError> {
    let config = service.app.config.current();
    let pool = service.current_service();
    let pinned = |pool: &ProxyServiceConfig| {
        select_provider_route(&config, pool, key, body, format).map(RoutePlan::pinned)
    };
    let value = serde_json::from_slice::<Value>(body).ok();
    let Some(model) = value
        .as_ref()
        .and_then(|value| value.get("model"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
    else {
        return pinned(&pool);
    };
    let providers = config.effective_providers();
    if model
        .split_once('/')
        .is_some_and(|(prefix, _)| providers.iter().any(|provider| provider.id == prefix))
    {
        return pinned(&pool);
    }
    // Responses continuation IDs belong to the provider that created them.
    // Unknown IDs retain the existing validation path instead of being retried.
    if let Some(previous) = super::super::client_context::previous_response_id(body) {
        let owner = super::super::responses::ResponsesSessionOwner::new(
            pool.id.clone(),
            key.map(|key| key.id.clone()),
        );
        let provider = if service
            .app
            .responses_sessions
            .get(&previous, &owner)
            .is_some()
        {
            Some("kiro")
        } else {
            providers
                .iter()
                .find(|provider| {
                    service
                        .app
                        .response_client_contexts
                        .get(&owner, &provider.id, &previous)
                        .is_some()
                })
                .map(|provider| provider.id.as_str())
        };
        let mut continuation = pool.clone();
        if let Some(provider) = provider {
            continuation.default_provider = provider.into();
        }
        return pinned(&continuation);
    }
    let exposed_kind = |kind| {
        providers.iter().any(|provider| {
            provider.kind == kind
                && config
                    .allowed_providers_for_service(&pool)
                    .contains(&provider.id.as_str())
        })
    };
    let requested_key = crosslane_core::model_policy::model_key(model);
    let changes_model = |overridden: &str| {
        !overridden.trim().is_empty()
            && crosslane_core::model_policy::model_key(overridden) != requested_key
    };
    if !exposed_kind("kiro")
        || !exposed_kind("copilot")
        || has_mapping(&config, &pool, key, model)
        || providers.iter().any(|provider| {
            provider.enabled
                && provider_allowed(&pool, key, &provider.id)
                && changes_model(&provider.routing.default_model_id)
        })
    {
        return pinned(&pool);
    }
    // Keep native batched chat semantics. The Kiro batch coordinator releases
    // its parent admission slot and re-enters routing for each n=1 candidate;
    // reserving the parent here would skew load and block a one-slot service.
    if protocol == ProviderProtocol::OpenAiChat
        && value
            .as_ref()
            .and_then(|value| value.get("n"))
            .and_then(Value::as_u64)
            .is_some_and(|n| n > 1)
    {
        return pinned(&pool);
    }
    ensure_model_enabled(&config, model, format)?;
    let mut routes = Vec::new();
    for provider in providers.iter().filter(|provider| {
        provider.enabled
            && matches!(provider.kind.as_str(), "kiro" | "copilot")
            && provider_allowed(&pool, key, &provider.id)
    }) {
        let id = ProviderId::parse(&provider.id).map_err(|error| {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string(), format)
        })?;
        let mut frozen = pool.clone();
        frozen.default_provider = provider.id.clone();
        // A request freezes membership across discovery and failover. Empty
        // tags plus explicit qualified IDs avoid inheriting newly added accounts.
        frozen.account_tag = Some(ServiceAccountTags::Many(Vec::new()));
        frozen.account_ids.clear();
        let mut kiro_ids = HashSet::new();
        let snapshot = if provider.kind == "kiro" {
            let account_pool = service.app.pool();
            let mut available = Vec::new();
            let mut matching = Vec::new();
            for account in account_pool.snapshot().await {
                if !pool.includes_account(&account)
                    || !account.enabled
                    || account_credit_state(&account, &config.pool) != AccountCreditState::Available
                {
                    continue;
                }
                let Some(runtime) = account_pool.get(&account.id).await else {
                    continue;
                };
                if !matches!(
                    runtime.health(),
                    AccountHealth::Available | AccountHealth::Cooling
                ) {
                    continue;
                }
                let models = runtime.supported_models().await;
                available.extend(models.iter().cloned());
                let cooling =
                    runtime.health() == AccountHealth::Cooling && !runtime.cooling_expired().await;
                matching.push((account.id, runtime, models, cooling));
            }
            resolve_model_alias(model, &available).map(|wire| {
                let maximum = config.pool.max_concurrent_per_account.max(1);
                let mut active = 0;
                for (id, runtime, models, cooling) in matching {
                    if !models.contains(&wire) {
                        continue;
                    }
                    active += if cooling { maximum } else { runtime.active() };
                    frozen.account_ids.push(format!("kiro/{id}"));
                    kiro_ids.insert(id);
                }
                (wire, kiro_ids.len().saturating_mul(maximum), active)
            })
        } else if let Some(runtime) = service.app.providers.copilot(&id).await {
            for account in runtime.account_summaries().await {
                if pool.includes_provider_account(&provider.id, &account.id, &account.tags) {
                    frozen
                        .account_ids
                        .push(format!("{}/{}", provider.id, account.id));
                }
            }
            runtime.routing_snapshot(&frozen, model, protocol).await
        } else {
            None
        };
        let Some((wire, capacity, active)) = snapshot.filter(|(_, capacity, _)| *capacity > 0)
        else {
            continue;
        };
        if authorize_model(key, &provider.id, &wire, format).is_err() {
            continue;
        }
        ensure_model_enabled(&config, &wire, format)?;
        // Both executors apply the chosen wire model. Share the original bytes
        // across candidates rather than copying a large request for each one.
        let route = SelectedProviderRoute {
            provider_id: id,
            provider_kind: provider.kind.clone(),
            original_model: model.into(),
            upstream_model: wire,
            mapping_rule: None,
            lock_kiro_mapping: provider.kind == "kiro",
            enable_model_fallback: false,
            web_search_operation: false,
            failover_timeout_ms: provider.routing.failover_timeout_ms,
            body: body.clone(),
            automatic: true,
            account_pool: Some(frozen),
            account_ids: (provider.kind == "kiro").then(|| Arc::new(kiro_ids)),
        };
        routes.push((
            route,
            ProviderLoad {
                id: provider.id.clone(),
                capacity,
                active,
            },
        ));
    }
    if routes.is_empty() {
        let mut fallback = pool.clone();
        if !provider_allowed(&pool, key, config.default_provider_for_service(&pool)) {
            if let Some(provider) = providers
                .iter()
                .find(|provider| provider_allowed(&pool, key, &provider.id))
            {
                fallback.default_provider = provider.id.clone();
            }
        }
        return pinned(&fallback);
    }
    routes.sort_by_key(|(route, _)| {
        route.provider_id.as_str() != config.default_provider_for_service(&pool)
    });
    Ok(RoutePlan {
        model: crosslane_core::model_policy::model_key(&routes[0].0.upstream_model),
        routes,
        automatic: true,
    })
}

pub(super) fn failover_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 401..=403 | 408 | 429 | 500..=599)
}

fn retryable(error: &ApiError) -> bool {
    if matches!(
        error.error_stage,
        "authentication"
            | "authorization"
            | "metering"
            | "model_policy"
            | "request_body"
            | "request_budget"
            | "context_validation"
    ) {
        return false;
    }
    error.error_code == "model_not_available"
        || (error.account_error && matches!(error.status.as_u16(), 401..=403))
        || (error.upstream_status.is_some_and(|status| {
            failover_status(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY))
        }) && !matches!(
            error.error_stage,
            "request_validation" | "request_budget" | "context_validation"
        ))
        || (error.error_stage == "upstream_dispatch"
            && matches!(error.status.as_u16(), 429 | 502..=504))
}

pub(super) async fn execute<F, Fut, T>(
    scheduler: &ProviderScheduler,
    service: &str,
    trace_id: &str,
    mut plan: RoutePlan,
    format: ErrorFormat,
    mut dispatch: F,
) -> Result<(Response, T), ApiError>
where
    F: FnMut(SelectedProviderRoute) -> Fut,
    Fut: Future<Output = Result<(Response, T), ApiError>>,
{
    if !plan.automatic {
        return dispatch(plan.routes.remove(0).0).await;
    }
    let mut last_error = None;
    while !plan.routes.is_empty() {
        let loads = plan
            .routes
            .iter()
            .map(|(_, load)| load.clone())
            .collect::<Vec<_>>();
        let Some((index, lease)) = scheduler.acquire(service, &plan.model, &loads) else {
            break;
        };
        let (route, _) = plan.routes.remove(index);
        tracing::info!(event = "upstream.provider.selected", trace_id, service_id = service,
            provider_id = %route.provider_id, model = %route.upstream_model,
            available_providers = loads.len(), "automatic provider route selected");
        let provider = route.provider_id.to_string();
        let deadline = route.failover_timeout_ms;
        let result = if deadline == 0 {
            dispatch(route).await
        } else {
            tokio::time::timeout(Duration::from_millis(deadline), dispatch(route))
                .await
                .unwrap_or_else(|_| {
                    let mut error = ApiError::new(
                        StatusCode::GATEWAY_TIMEOUT,
                        "provider did not return a response before its failover deadline",
                        format,
                    )
                    .with_provider_id(&provider);
                    error.error_code = "upstream_timeout";
                    error.error_stage = "upstream_dispatch";
                    Err(error)
                })
        };
        match result {
            Ok((response, context)) => {
                let (parts, body) = response.into_parts();
                let scanner = parts
                    .headers
                    .get("content-type")
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value.starts_with("text/event-stream"))
                    .then(ProviderUsageScanner::default);
                let successful = parts.status.is_success();
                return Ok((
                    Response::from_parts(
                        parts,
                        Body::new(RoutedBody {
                            body,
                            lease: Some(lease),
                            scanner,
                            successful,
                        }),
                    ),
                    context,
                ));
            }
            Err(error) if retryable(&error) => {
                lease.failure();
                tracing::warn!(
                    event = "upstream.provider.failover",
                    trace_id,
                    service_id = service,
                    provider_id = provider,
                    error_code = error.error_code,
                    status = error.status.as_u16(),
                    remaining_providers = plan.routes.len(),
                    "provider failed; trying another compatible provider"
                );
                last_error = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        ApiError::retryable("compatible providers are recovering, please retry", format)
    }))
}

struct RoutedBody {
    body: Body,
    lease: Option<ProviderLease>,
    scanner: Option<ProviderUsageScanner>,
    successful: bool,
}

impl RoutedBody {
    fn finish(&mut self, failed: bool) {
        if let Some(lease) = self.lease.take() {
            if failed
                || self
                    .scanner
                    .take()
                    .is_some_and(ProviderUsageScanner::failed)
            {
                lease.failure();
            } else if self.successful {
                lease.success();
            }
        }
    }
}

impl HttpBody for RoutedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let frame = Pin::new(&mut self.body).poll_frame(cx);
        match &frame {
            Poll::Ready(Some(Ok(frame))) => {
                if let (Some(scanner), Some(data)) = (&mut self.scanner, frame.data_ref()) {
                    scanner.push(data);
                }
                if self.body.is_end_stream() {
                    self.finish(false);
                }
            }
            Poll::Ready(Some(Err(_))) => self.finish(true),
            Poll::Ready(None) => self.finish(false),
            Poll::Pending => {}
        }
        frame
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_stalled_attempt_expires_and_uses_the_other_provider() {
        let scheduler = ProviderScheduler::default();
        let config = Config {
            provider: vec![
                crosslane_core::config::ProviderConfig::default(),
                crosslane_core::config::ProviderConfig {
                    id: "copilot".into(),
                    kind: "copilot".into(),
                    ..Default::default()
                },
            ],
            ..Config::default()
        };
        let mut pool = ProxyServiceConfig {
            allowed_providers: vec!["kiro".into(), "copilot".into()],
            ..Default::default()
        };
        let mut routes = Vec::new();
        for id in ["kiro", "copilot"] {
            pool.default_provider = id.into();
            let mut route = select_provider_route(
                &config,
                &pool,
                None,
                &Bytes::from_static(br#"{"model":"claude-sonnet-4-6"}"#),
                ErrorFormat::OpenAi,
            )
            .unwrap_or_else(|_| panic!("test route"));
            route.automatic = true;
            route.failover_timeout_ms = 10;
            routes.push((
                route,
                ProviderLoad {
                    id: id.into(),
                    capacity: 1,
                    active: 0,
                },
            ));
        }
        let plan = RoutePlan {
            routes,
            automatic: true,
            model: "claude-sonnet-4-6".into(),
        };
        let (response, ()) = execute(
            &scheduler,
            "service",
            "trace",
            plan,
            ErrorFormat::OpenAi,
            |route| async move {
                if route.provider_id.as_str() == "kiro" {
                    std::future::pending::<()>().await;
                }
                Ok((Response::new(Body::from("from copilot")), ()))
            },
        )
        .await
        .unwrap_or_else(|_| panic!("failover should succeed"));
        let body = axum::body::to_bytes(response.into_body(), 1_024)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), b"from copilot");
        assert!(scheduler
            .acquire(
                "service",
                "claude-sonnet-4-6",
                &[ProviderLoad {
                    id: "kiro".into(),
                    capacity: 1,
                    active: 0
                }]
            )
            .is_none());
    }

    #[test]
    fn client_key_quota_and_request_errors_are_not_provider_outages() {
        let quota =
            super::super::meter_error(crate::meter::MeterError::LimitExceeded, ErrorFormat::OpenAi);
        assert!(!retryable(&quota));
        assert!(!retryable(&ApiError::new(
            StatusCode::BAD_REQUEST,
            "max_tokens must be greater than 0",
            ErrorFormat::OpenAi
        )));
        assert!(!retryable(&ApiError::new(
            StatusCode::FORBIDDEN,
            "model is not allowed for this API key",
            ErrorFormat::OpenAi
        )));
        let pool_quota = super::super::upstream_error(
            super::super::ExecuteError::Pool(crosslane_pool::PoolError::CreditsExhausted),
            ErrorFormat::OpenAi,
        );
        assert!(retryable(&pool_quota));
    }
}
