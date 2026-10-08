use super::{
    handle_claude, handle_openai, json, log_model, now_secs, request_trace_id,
    sanitize_error_message, ApiError, AppState, Arc, BTreeSet, Body, Bytes, Duration, ErrorFormat,
    HeaderMap, Instant, IntoResponse, Json, Request, RequestDiagnostics, RequestLog, Response,
    ServiceHttpState, State, StatusCode, StreamExt, UpstreamAttemptLog, Uuid, Value,
    MAX_ATTEMPT_LOG_SUMMARY_CHARS, MAX_STATS_MODEL_CHARS, UNKNOWN_STATS_MODEL,
};

pub async fn root() -> Json<Value> {
    Json(json!({"name":"crosslane","status":"ok","version":env!("CARGO_PKG_VERSION")}))
}

#[derive(Default)]
struct ServiceAccountHealth {
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
    used_credits: f64,
    total_credits: f64,
    errors: Vec<String>,
}

async fn service_account_health(service: &ServiceHttpState) -> ServiceAccountHealth {
    let config = service.app.config.current();
    let current_service = service.current_service();
    let allowed = config
        .allowed_providers_for_service(&current_service)
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let mut health = ServiceAccountHealth {
        provider_scope: allowed.iter().cloned().collect(),
        ..ServiceAccountHealth::default()
    };
    for provider in config
        .effective_providers()
        .into_iter()
        .filter(|provider| allowed.contains(&provider.id))
    {
        match provider.kind.as_str() {
            "kiro" => {
                let pool = service.app.pool();
                // Keep the service's effective account pool authoritative: a
                // multi-provider rollup must not leak accounts the service
                // excludes by tag, manual binding, or exclusion list.
                let account_ids = service.account_ids().await;
                let accounts = pool
                    .snapshot()
                    .await
                    .into_iter()
                    .filter(|account| account_ids.contains(&account.id))
                    .collect::<Vec<_>>();
                health.total += accounts.len();
                if provider.enabled {
                    let counts = pool.scheduling_counts_scoped(&account_ids).await;
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
                let (used, total) = accounts
                    .iter()
                    .filter_map(|account| account.usage.as_ref())
                    .fold((0.0, 0.0), |(used, total), usage| {
                        (
                            used + usage.current,
                            total + crosslane_pool::effective_credit_limit(usage, &config.pool),
                        )
                    });
                health.used_credits += used;
                health.total_credits += total;
            }
            "copilot" => {
                let id = match crosslane_core::provider::ProviderId::parse(provider.id.clone()) {
                    Ok(id) => id,
                    Err(error) => {
                        health.errors.push(error.to_string());
                        continue;
                    }
                };
                let Some(runtime) = service.app.providers.copilot(&id).await else {
                    health
                        .errors
                        .push(format!("provider {} runtime is unavailable", provider.id));
                    continue;
                };
                for account in runtime.account_summaries().await {
                    if !current_service.includes_provider_account(
                        &provider.id,
                        &account.id,
                        &account.tags,
                    ) {
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

pub async fn health(State(service): State<ServiceHttpState>) -> Json<Value> {
    let accounts = service_account_health(&service).await;
    Json(json!({
        "status":"ok",
        "service_id":service.service.id,
        "service_name":service.service.name,
        "provider_scope":accounts.provider_scope,
        "total_accounts":accounts.total,
        "available_accounts":accounts.available,
        "protected_accounts":accounts.protected,
        "cooling_accounts":accounts.cooling,
        "exhausted_accounts":accounts.exhausted,
        "banned_accounts":accounts.banned,
        "refreshing_accounts":accounts.refreshing,
        "disabled_accounts":accounts.disabled,
        "unavailable_accounts":accounts.unavailable,
        "provider_errors":accounts.errors,
        "used_credits":accounts.used_credits,
        "total_credits":accounts.total_credits,
        "uptime_secs":service.app.uptime_secs()
    }))
}

pub async fn readiness(State(service): State<ServiceHttpState>) -> Response {
    let accounts = service_account_health(&service).await;
    let mut reasons = service.app.task_registry.readiness_issues(&service.app);
    if accounts.available == 0 {
        reasons.push(
            if service.app.config.current().provider.is_empty() {
                "no account is currently available"
            } else {
                "no account is currently available in this service's provider scope"
            }
            .into(),
        );
    }
    reasons.extend(accounts.errors.iter().cloned());
    if let Some(error) = service.app.meter.recovery_error() {
        reasons.push(format!("metering recovery required: {error}"));
    }
    let ready = reasons.is_empty();
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(json!({
            "status":if ready { "ready" } else { "not_ready" },
            "ready":ready,
            "reasons":reasons,
            "service_id":service.service.id,
            "service_name":service.service.name,
            "provider_scope":accounts.provider_scope,
            "available_accounts":accounts.available,
            "unavailable_accounts":accounts.unavailable,
            "uptime_secs":service.app.uptime_secs()
        })),
    )
        .into_response()
}

pub async fn claude_messages(
    State(service): State<ServiceHttpState>,
    request: Request,
) -> Response {
    let state = Arc::clone(&service.app);
    let path = request.uri().path().to_string();
    let trace_id = request_trace_id(&request);
    let started = Instant::now();
    let default_provider = state
        .config
        .current()
        .default_provider_for_service(&service.service)
        .to_owned();
    let connection_guard = match state.connections.try_acquire() {
        Some(guard) => guard,
        None => {
            let error =
                ApiError::overloaded(ErrorFormat::Claude).with_provider_id(&default_provider);
            record_failed_request(&state, &trace_id, &path, "", started, &error);
            return error.with_request_id(&trace_id).into_response();
        }
    };
    let admission_guard = match state.admission.try_acquire() {
        Some(guard) => guard,
        None => {
            let error =
                ApiError::overloaded(ErrorFormat::Claude).with_provider_id(&default_provider);
            record_failed_request(&state, &trace_id, &path, "", started, &error);
            return error.with_request_id(&trace_id).into_response();
        }
    };
    let (headers, body, _body_reservations) =
        match read_bounded_body(&state, request, ErrorFormat::Claude).await {
            Ok(body) => body,
            Err(error) => {
                let error = error.with_provider_id_if_empty(&default_provider);
                record_failed_request(&state, &trace_id, &path, "", started, &error);
                return error.with_request_id(&trace_id).into_response();
            }
        };
    let model = request_model_hint(&body);
    tracing::debug!(
        trace_id = %trace_id,
        protocol = "claude",
        body_bytes = body.len(),
        model = %model,
        "client request body read"
    );
    match handle_claude(
        service,
        trace_id.clone(),
        path.clone(),
        headers,
        body,
        connection_guard,
        admission_guard,
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            record_failed_request(&state, &trace_id, &path, &model, started, &error);
            error.with_request_id(&trace_id).into_response()
        }
    }
}

pub async fn openai_chat(State(service): State<ServiceHttpState>, request: Request) -> Response {
    let state = Arc::clone(&service.app);
    let path = request.uri().path().to_string();
    let trace_id = request_trace_id(&request);
    let started = Instant::now();
    let default_provider = state
        .config
        .current()
        .default_provider_for_service(&service.service)
        .to_owned();
    let connection_guard = match state.connections.try_acquire() {
        Some(guard) => guard,
        None => {
            let error =
                ApiError::overloaded(ErrorFormat::OpenAi).with_provider_id(&default_provider);
            record_failed_request(&state, &trace_id, &path, "", started, &error);
            return error.with_request_id(&trace_id).into_response();
        }
    };
    let admission_guard = match state.admission.try_acquire() {
        Some(guard) => guard,
        None => {
            let error =
                ApiError::overloaded(ErrorFormat::OpenAi).with_provider_id(&default_provider);
            record_failed_request(&state, &trace_id, &path, "", started, &error);
            return error.with_request_id(&trace_id).into_response();
        }
    };
    let (headers, body, _body_reservations) =
        match read_bounded_body(&state, request, ErrorFormat::OpenAi).await {
            Ok(body) => body,
            Err(error) => {
                let error = error.with_provider_id_if_empty(&default_provider);
                record_failed_request(&state, &trace_id, &path, "", started, &error);
                return error.with_request_id(&trace_id).into_response();
            }
        };
    let model = request_model_hint(&body);
    tracing::debug!(
        trace_id = %trace_id,
        protocol = "openai",
        body_bytes = body.len(),
        model = %model,
        "client request body read"
    );
    match handle_openai(
        service,
        trace_id.clone(),
        path.clone(),
        headers,
        body,
        connection_guard,
        admission_guard,
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            record_failed_request(&state, &trace_id, &path, &model, started, &error);
            error.with_request_id(&trace_id).into_response()
        }
    }
}

pub(super) async fn read_bounded_body(
    state: &Arc<AppState>,
    request: Request,
    format: ErrorFormat,
) -> Result<(HeaderMap, Bytes, crate::state::BodyGuard), ApiError> {
    const MAX_BODY_BYTES: usize = 50 * 1024 * 1024;
    let (parts, body) = request.into_parts();
    let mut stream = Body::into_data_stream(body);
    let mut bytes = bytes::BytesMut::new();
    let mut reservation = state
        .body_budget
        .reserve(0)
        .ok_or_else(|| ApiError::overloaded(format))?;
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(15), stream.next())
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::REQUEST_TIMEOUT,
                "request body read timed out",
                format,
            )
        })?
    {
        let chunk = chunk.map_err(|error| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("failed to read request body: {error}"),
                format,
            )
        })?;
        if bytes.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body exceeds the 50 MiB limit",
                format,
            ));
        }
        if !reservation.reserve_more(chunk.len()) {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "request body memory budget exceeded",
                format,
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((parts.headers, bytes.freeze(), reservation))
}

fn request_model_hint(body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value.get("model").and_then(Value::as_str).map(|model| {
                log_model(model)
                    .chars()
                    .take(MAX_STATS_MODEL_CHARS)
                    .collect()
            })
        })
        .unwrap_or_default()
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct AttemptDiagnostics {
    pub(super) account_ids: String,
    pub(super) account_names: String,
    pub(super) available_models: String,
    pub(super) available_model_count: usize,
    pub(super) errors: String,
}

pub(super) fn attempt_diagnostics(attempts: &[UpstreamAttemptLog]) -> AttemptDiagnostics {
    let account_ids = attempts
        .iter()
        .filter_map(|attempt| {
            (!attempt.account_id.is_empty()).then_some(attempt.account_id.as_str())
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(",");
    let account_names = attempts
        .iter()
        .filter_map(|attempt| {
            (!attempt.account_name.is_empty()).then_some(attempt.account_name.as_str())
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(",");
    let available_models = attempts
        .iter()
        .flat_map(|attempt| attempt.available_models.iter().map(String::as_str))
        .collect::<BTreeSet<_>>();
    let available_model_count = available_models.len();
    let available_models = available_models.into_iter().collect::<Vec<_>>().join(",");
    let errors = attempts
        .iter()
        .map(|attempt| {
            format!(
                "attempt={} account={} endpoint={} status={} error={}",
                attempt.attempt,
                attempt.account_id,
                attempt.endpoint,
                attempt
                    .status
                    .map_or_else(|| "-".into(), |status| status.to_string()),
                sanitize_error_message(&attempt.error)
            )
        })
        .collect::<Vec<_>>()
        .join(" | ");
    AttemptDiagnostics {
        account_ids: bounded_log_summary(account_ids),
        account_names: bounded_log_summary(account_names),
        available_models: bounded_log_summary(available_models),
        available_model_count,
        errors: bounded_log_summary(errors),
    }
}

fn bounded_log_summary(value: String) -> String {
    if value.chars().count() <= MAX_ATTEMPT_LOG_SUMMARY_CHARS {
        return value;
    }
    let mut output = value
        .chars()
        .take(MAX_ATTEMPT_LOG_SUMMARY_CHARS)
        .collect::<String>();
    output.push('…');
    output
}

pub(super) fn record_failed_request(
    state: &Arc<AppState>,
    trace_id: &str,
    path: &str,
    model: &str,
    started: Instant,
    error: &ApiError,
) {
    let model = if error.suppress_model_stats {
        UNKNOWN_STATS_MODEL.to_owned()
    } else {
        log_model(model)
            .chars()
            .take(MAX_STATS_MODEL_CHARS)
            .collect()
    };
    let safe_error = sanitize_error_message(&error.message);
    let duration_ms = started.elapsed().as_millis() as u64;
    let model_path = error
        .log_context
        .model_path
        .iter()
        .map(|model| log_model(model))
        .collect::<Vec<_>>();
    let mapped_model = log_model(&error.log_context.mapped_model);
    let kiro_model = log_model(&error.log_context.kiro_model);
    let request_id = format!("req_{}", Uuid::new_v4().simple());
    let attempts = attempt_diagnostics(&error.log_context.attempts);
    let upstream_status = error.upstream_status.or_else(|| {
        error
            .log_context
            .attempts
            .iter()
            .rev()
            .find_map(|attempt| attempt.status)
    });
    if error.status.is_server_error() {
        tracing::error!(
            event = "proxy.request.failed",
            trace_id,
            request_id,
            http_path = path,
            model = %model,
            mapped_model,
            kiro_model,
            model_path = %model_path.join(" -> "),
            mapping_rule = error.log_context.model_mapping_rule.as_deref().unwrap_or("none"),
            account_id = %error.log_context.account_id,
            account_name = %error.log_context.account_name,
            endpoint = %error.log_context.endpoint,
            upstream_attempts = error.log_context.attempts.len(),
            attempted_account_ids = %attempts.account_ids,
            attempted_account_names = %attempts.account_names,
            available_model_count = attempts.available_model_count,
            available_models = %attempts.available_models,
            attempt_errors = %attempts.errors,
            http_status = error.status.as_u16(),
            upstream_status = upstream_status.unwrap_or_default(),
            error_code = error.error_code,
            error_stage = error.error_stage,
            account_error = error.account_error,
            context_overflow = ?error.context_overflow,
            duration_ms,
            error = %safe_error,
            "client request failed"
        );
    } else {
        tracing::warn!(
            event = "proxy.request.rejected",
            trace_id,
            request_id,
            http_path = path,
            model = %model,
            mapped_model,
            kiro_model,
            model_path = %model_path.join(" -> "),
            mapping_rule = error.log_context.model_mapping_rule.as_deref().unwrap_or("none"),
            account_id = %error.log_context.account_id,
            account_name = %error.log_context.account_name,
            endpoint = %error.log_context.endpoint,
            upstream_attempts = error.log_context.attempts.len(),
            attempted_account_ids = %attempts.account_ids,
            attempted_account_names = %attempts.account_names,
            available_model_count = attempts.available_model_count,
            available_models = %attempts.available_models,
            attempt_errors = %attempts.errors,
            http_status = error.status.as_u16(),
            upstream_status = upstream_status.unwrap_or_default(),
            error_code = error.error_code,
            error_stage = error.error_stage,
            account_error = error.account_error,
            context_overflow = ?error.context_overflow,
            duration_ms,
            error = %safe_error,
            "client request rejected"
        );
    }
    state.stats.record(RequestLog {
        timestamp: now_secs(),
        trace_id: trace_id.into(),
        request_id,
        path: path.into(),
        provider_id: if error.log_context.provider_id.is_empty() {
            "kiro".into()
        } else {
            error.log_context.provider_id.clone()
        },
        model: if error.log_context.mapped_model.is_empty() {
            model.clone()
        } else {
            mapped_model
        },
        original_model: model,
        kiro_model,
        account_id: error.log_context.account_id.clone(),
        account_name: error.log_context.account_name.clone(),
        endpoint: error.log_context.endpoint.clone(),
        model_path,
        model_mapping_rule: error.log_context.model_mapping_rule.clone(),
        attempts: error.log_context.attempts.clone(),
        duration_ms,
        status: error.status.as_u16(),
        input_tokens: 0,
        output_tokens: 0,
        credits: 0.0,
        error: Some(safe_error),
        diagnostics: RequestDiagnostics {
            client_status: error.client_status.unwrap_or(error.status.as_u16()),
            upstream_status,
            error_code: error.error_code.to_owned(),
            error_stage: error.error_stage.to_owned(),
            account_error: error.account_error,
            context_overflow: error.context_overflow.as_deref().cloned(),
            ..RequestDiagnostics::default()
        },
    });
}
