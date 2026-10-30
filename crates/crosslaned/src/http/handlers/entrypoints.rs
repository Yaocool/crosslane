#[cfg(feature = "crs")]
use super::super::crs;
use super::{
    handle_claude, handle_openai, log_model, logged_user_agent, now_secs, request_trace_id,
    sanitize_error_message, ApiError, AppState, Arc, BTreeSet, Body, Bytes, Duration, ErrorFormat,
    HeaderMap, Instant, IntoResponse, Request, RequestDiagnostics, RequestLog, Response,
    ServiceHttpState, State, StatusCode, StreamExt, UpstreamAttemptLog, Uuid, Value,
    MAX_ATTEMPT_LOG_SUMMARY_CHARS, MAX_STATS_MODEL_CHARS, UNKNOWN_STATS_MODEL,
};

pub async fn claude_messages(
    State(service): State<ServiceHttpState>,
    request: Request,
) -> Response {
    let state = Arc::clone(&service.app);
    let path = request.uri().path().to_string();
    let trace_id = request_trace_id(&request);
    let user_agent = logged_user_agent(request.headers());
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
            record_failed_request(&state, &trace_id, &path, &user_agent, "", started, &error);
            return error.with_request_id(&trace_id).into_response();
        }
    };
    let admission_guard = match state.admission.try_acquire() {
        Some(guard) => guard,
        None => {
            let error =
                ApiError::overloaded(ErrorFormat::Claude).with_provider_id(&default_provider);
            record_failed_request(&state, &trace_id, &path, &user_agent, "", started, &error);
            return error.with_request_id(&trace_id).into_response();
        }
    };
    let (headers, body, _body_reservations) =
        match read_bounded_body(&state, request, ErrorFormat::Claude).await {
            Ok(body) => body,
            Err(error) => {
                let error = error.with_provider_id_if_empty(&default_provider);
                record_failed_request(&state, &trace_id, &path, &user_agent, "", started, &error);
                return error.with_request_id(&trace_id).into_response();
            }
        };
    let model = request_model_hint(&body);
    #[cfg(feature = "crs")]
    let stream_requested = crs::stream_requested(&body);
    #[cfg(feature = "crs")]
    let ccr_relay =
        service.current_service().crs_relay == Some(crosslane_core::config::CrsRelayMode::Ccr);
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
        Ok(response) => {
            #[cfg(feature = "crs")]
            {
                if ccr_relay {
                    crs::adapt_ccr_response(response, &trace_id, &model).await
                } else if stream_requested {
                    crs::adapt_claude_stream_response(response, &trace_id, &model).await
                } else {
                    crs::adapt_claude_nonstream_response(response, &trace_id).await
                }
            }
            #[cfg(not(feature = "crs"))]
            {
                response
            }
        }
        Err(error) => {
            #[cfg(feature = "crs")]
            let use_sse = !ccr_relay
                && stream_requested
                && crs::uses_sse_error(error.status, error.upstream_status);
            #[cfg(feature = "crs")]
            let use_json_context = !ccr_relay
                && !stream_requested
                && crs::uses_json_context_error(
                    error.status,
                    error.error_code,
                    error.error_stage,
                    error.upstream_status,
                );
            #[cfg(feature = "crs")]
            let error = if use_sse || use_json_context {
                ApiError {
                    client_status: Some(200),
                    ..error
                }
            } else {
                error
            };
            record_failed_request(
                &state,
                &trace_id,
                &path,
                &user_agent,
                &model,
                started,
                &error,
            );
            let response = error.with_request_id(&trace_id).into_response();
            #[cfg(feature = "crs")]
            if use_sse {
                return crs::adapt_claude_stream_response(response, &trace_id, &model).await;
            }
            #[cfg(feature = "crs")]
            if use_json_context {
                return crs::adapt_claude_nonstream_response(response, &trace_id).await;
            }
            response
        }
    }
}

pub async fn openai_chat(State(service): State<ServiceHttpState>, request: Request) -> Response {
    let state = Arc::clone(&service.app);
    let path = request.uri().path().to_string();
    let trace_id = request_trace_id(&request);
    let user_agent = logged_user_agent(request.headers());
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
            record_failed_request(&state, &trace_id, &path, &user_agent, "", started, &error);
            return error.with_request_id(&trace_id).into_response();
        }
    };
    let admission_guard = match state.admission.try_acquire() {
        Some(guard) => guard,
        None => {
            let error =
                ApiError::overloaded(ErrorFormat::OpenAi).with_provider_id(&default_provider);
            record_failed_request(&state, &trace_id, &path, &user_agent, "", started, &error);
            return error.with_request_id(&trace_id).into_response();
        }
    };
    let (headers, body, _body_reservations) =
        match read_bounded_body(&state, request, ErrorFormat::OpenAi).await {
            Ok(body) => body,
            Err(error) => {
                let error = error.with_provider_id_if_empty(&default_provider);
                record_failed_request(&state, &trace_id, &path, &user_agent, "", started, &error);
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
            record_failed_request(
                &state,
                &trace_id,
                &path,
                &user_agent,
                &model,
                started,
                &error,
            );
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
    let trace_id = request_trace_id(&request);
    let started = Instant::now();
    let mut body_bytes = 0usize;
    let result = async {
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
            body_bytes = body_bytes.saturating_add(chunk.len());
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
    .await;
    tracing::info!(
        event = "http.request.body_read",
        trace_id,
        body_read_ms = started.elapsed().as_millis() as u64,
        body_bytes,
        outcome = if result.is_ok() {
            "completed"
        } else {
            "failed"
        },
        "client request body read finished"
    );
    result
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
    user_agent: &str,
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
            user_agent,
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
            user_agent,
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
    let log = RequestLog {
        timestamp: now_secs(),
        trace_id: trace_id.into(),
        request_id,
        path: path.into(),
        user_agent: user_agent.into(),
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
            client_context: error.client_context.as_deref().cloned(),
            client_status: error.client_status.unwrap_or(error.status.as_u16()),
            upstream_status,
            error_code: error.error_code.to_owned(),
            error_stage: error.error_stage.to_owned(),
            account_error: error.account_error,
            context_overflow: error.context_overflow.as_deref().cloned(),
            ..RequestDiagnostics::default()
        },
    };
    #[cfg(feature = "crs")]
    let log = {
        let mut log = log;
        if let Some(usage) = error.log_context.confirmed_usage.as_ref() {
            log.input_tokens = usage.input_tokens;
            log.output_tokens = usage.output_tokens;
            log.credits = usage.credits;
            if usage.credits > 0.0 {
                log.diagnostics.credits_source = "server".into();
            }
        }
        log
    };
    state.stats.record(log);
}
