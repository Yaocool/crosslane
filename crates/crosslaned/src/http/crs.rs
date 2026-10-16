//! Response compatibility for Claude Relay Service's Console/CCR relay paths.
//!
//! Preserve supplier error statuses in sanitized diagnostics and HTTP responses.
//! Streaming errors use HTTP 200 SSE so CRS Console retains their details.
//! Local non-streaming context rejections use HTTP 200 error JSON, with the
//! semantic status in error.status and x-crosslane-error-status.
//! Errors after an SSE response starts remain in the already committed stream.

use std::time::{Duration, Instant};

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::BytesMut;
use crosslane_translate::error::log_model;
use crosslane_translate::{error_envelope, sanitize_error_message, ErrorFormat};
use futures::StreamExt;
use serde_json::Value;

mod capability;
mod stream;
mod tools;
pub(super) use capability::execute_with_effort_recovery;
pub(super) use stream::ClaudeStreamState;
pub(super) use tools::normalize_tool_changes;

const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_PREFLIGHT_BYTES: usize = 64 * 1024;
const PREFLIGHT_WAIT: Duration = Duration::from_secs(5);
pub(super) const CONTENT_FILTERED_ERROR: &str =
    "Upstream content policy declined this request (CONTENT_FILTERED).";

/// A candidate and the final response adapter share one inspection window.
/// Extensions stay inside Crosslane and cannot be supplied as client headers.
#[derive(Clone, Copy)]
struct CandidatePreflightDeadline(tokio::time::Instant);

pub(super) fn enrich_error_envelope(
    envelope: &mut Value,
    code: &str,
    upstream_status: Option<u16>,
    request_id: Option<&str>,
) {
    let code = safe_code(code);
    envelope["error"]["code"] = Value::String(code.to_owned());
    if let Some(status) = upstream_status {
        envelope["error"]["upstream_status"] = Value::from(status);
    }
    // Keep the sanitized cause separate: normalizing a rendered context error
    // again must not interpret digits in its trace ID as token counts.
    let input_recovery = if code == "context_length_exceeded" && upstream_status.is_none() {
        serde_json::from_value::<crate::stats::ContextOverflowDiagnostics>(
            envelope["error"]["context"].clone(),
        )
        .ok()
        .and_then(|context| super::context_recovery::input_overflow_recovery(&context))
    } else {
        None
    };
    let detail = input_recovery.as_ref().map_or_else(
        || sanitize_error_message(error_message(envelope).unwrap_or_default()),
        |recovery| recovery.detail.clone(),
    );
    let cause = if is_opaque_message(&detail) {
        match code {
            "upstream_authentication_failed" => "Upstream credentials were rejected.",
            "upstream_idle_timeout" => "The upstream response timed out.",
            "upstream_transport_interrupted" => "The upstream connection was interrupted.",
            _ => "The service could not complete this request.",
        }
        .to_owned()
    } else if code == "context_length_exceeded"
        && upstream_status.is_some()
        && detail == "prompt is too long: context length exceeded"
    {
        "prompt is too long: upstream input limit exceeded; the exact limit was not reported."
            .to_owned()
    } else {
        detail.clone()
    };
    let hint = if let Some(recovery) = &input_recovery {
        recovery.hint
    } else if code == "context_length_exceeded"
        && envelope["error"]["context"]["minimum_input_tokens"]
            .as_u64()
            .zip(envelope["error"]["context"]["maximum_input_tokens"].as_u64())
            .is_some_and(|(minimum, maximum)| minimum > maximum)
    {
        super::context_recovery::LEGACY_INPUT_OVERFLOW_HINT
    } else {
        error_hint(code)
    };
    let mut message = cause.chars().take(512).collect::<String>();
    if envelope["error"]["recovery"] == "compaction_retry_failed" {
        message.push_str(" Compaction was attempted, but the retry still failed.");
    }
    message.push(' ');
    message.push_str(hint);
    message.push_str(&format!(" [code: {code}"));
    if let Some(stage) = envelope["error"]["stage"]
        .as_str()
        .filter(|stage| valid_code(stage))
    {
        message.push_str(&format!(", stage: {stage}"));
    }
    if let Some(status) = upstream_status {
        message.push_str(&format!(", upstream HTTP {status}"));
    }
    if envelope["error"]["upstream_reason"] == "CONTENT_LENGTH_EXCEEDS_THRESHOLD" {
        message.push_str(", reason: CONTENT_LENGTH_EXCEEDS_THRESHOLD");
    }
    if let Some(request_id) = request_id {
        message.push_str(&format!(", request ID: {request_id}"));
    }
    message.push(']');
    envelope["error"]["detail"] = Value::String(detail);
    envelope["error"]["hint"] = Value::String(hint.to_owned());
    envelope["error"]["message"] = Value::String(message);
    envelope["error"]["retryable"] = Value::Bool(matches!(
        code,
        "upstream_unavailable"
            | "upstream_rate_limited"
            | "upstream_overloaded"
            | "upstream_idle_timeout"
            | "upstream_transport_interrupted"
            | "upstream_event_stream_empty"
            | "upstream_event_stream_corrupt"
            | "upstream_model_unavailable"
            | "model_unavailable"
    ));
}

fn error_hint(code: &str) -> &'static str {
    match code {
        "upstream_content_filtered" => "Review the conversation content; the provider declined this request. Retrying the same request is not recommended.",
        "context_length_exceeded" => {
            "Run /compact, reduce large tool results or attachments, or start a new session."
        }
        "compaction_failed" => {
            "The original context is intact. Retry compact or process the source in smaller sections; no incomplete checkpoint was saved."
        }
        "request_payload_exceeded" | "request_body_too_large" => {
            "Reduce request content, attachments, or tool definitions."
        }
        "tool_budget_exceeded" | "tool_catalog_too_large" => {
            "Reduce enabled tools or tool schema size; enable Tool Search where supported."
        }
        "invalid_tool_protocol" | "unsupported_tool_protocol" => {
            "Check tool definitions and tool_use/tool_result pairing."
        }
        super::tool_names::TOOL_NAME_MISMATCH_CODE => {
            "Share the request ID with the service administrator to check the rejected upstream tool name and current tool declarations."
        }
        "model_not_available" | "model_disabled" => {
            "Choose an available model or ask the service administrator to check model routing."
        }
        "upstream_authentication_failed" | "upstream_quota_exhausted" => {
            "Ask the service administrator to check upstream access or quota using the request ID."
        }
        "upstream_rate_limited" | "upstream_overloaded" => "Wait briefly and retry.",
        "invalid_request" => {
            "Check the request parameters; share the request ID if the problem persists."
        }
        _ => "Retry; if the problem persists, share the request ID with the service administrator.",
    }
}

pub(super) fn context_rejection_reason(message: &str) -> Option<&'static str> {
    let lower = message.to_ascii_lowercase();
    (!is_opaque_message(&sanitize_error_message(message))
        && (lower.contains("content_length_exceeds_threshold")
            || lower.contains("input content length exceeds threshold")))
    .then_some("CONTENT_LENGTH_EXCEEDS_THRESHOLD")
}

pub(super) fn uses_sse_error(status: StatusCode, upstream_status: Option<u16>) -> bool {
    // Supplier failures need SSE even for auth/payload statuses. Local client
    // authentication, authorization and body limits retain their HTTP status.
    matches!(status.as_u16(), 400 | 404 | 408 | 422 | 429 | 500..=599)
        || upstream_status.is_some_and(|status| (400..=599).contains(&status))
}

pub(super) fn stream_requested(body: &[u8]) -> bool {
    // Ignore the large messages/tools rather than allocating a second request
    // tree just to read the flag. Invalid JSON retains the HTTP validation path.
    #[derive(serde::Deserialize)]
    struct RequestMode {
        #[serde(default)]
        stream: bool,
    }
    serde_json::from_slice::<RequestMode>(body).is_ok_and(|request| request.stream)
}

pub(super) fn uses_json_context_error(
    status: StatusCode,
    code: &str,
    stage: &str,
    upstream_status: Option<u16>,
) -> bool {
    status == StatusCode::BAD_REQUEST
        && code == "context_length_exceeded"
        && stage == "context_validation"
        && upstream_status.is_none()
}

pub(super) async fn adapt_claude_nonstream_response(
    response: Response,
    request_id: &str,
) -> Response {
    let response = adapt_claude_response(response, request_id).await;
    if response.status() != StatusCode::BAD_REQUEST
        || response
            .headers()
            .contains_key("x-crosslane-upstream-status")
    {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    // Normalization already produced a bounded, sanitized error body. Inspect
    // its stable classification rather than matching user-supplied error prose.
    let bytes = to_bytes(body, MAX_ERROR_BODY_BYTES)
        .await
        .unwrap_or_default();
    let envelope = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
    let local_context_error = envelope["type"] == "error"
        && envelope["error"]["status"] == 400
        && envelope["error"].get("upstream_status").is_none()
        && uses_json_context_error(
            parts.status,
            envelope["error"]["code"].as_str().unwrap_or_default(),
            envelope["error"]["stage"].as_str().unwrap_or_default(),
            None,
        );
    if local_context_error {
        // CRS Console rewrites every non-2xx JSON body. Keep this as an error
        // object, never fabricate a successful assistant message or usage.
        parts.status = StatusCode::OK;
        parts.headers.remove(header::CONTENT_LENGTH);
        parts.headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        parts
            .headers
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        parts
            .headers
            .insert("x-crosslane-error-status", HeaderValue::from_static("400"));
    }
    Response::from_parts(parts, Body::from(bytes))
}

pub(super) async fn adapt_claude_stream_response(
    response: Response,
    request_id: &str,
    model: &str,
) -> Response {
    let response = adapt_claude_response(response, request_id).await;
    let upstream_status = response
        .headers()
        .get("x-crosslane-upstream-status")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok());
    if !uses_sse_error(response.status(), upstream_status) {
        return stream::protect_response(response, request_id, model);
    }
    let status = response.status();
    let (mut parts, body) = response.into_parts();
    // Normalized errors are bounded JSON; keep the semantic failure status in
    // the event and headers even though CRS requires a 200 streaming transport.
    let bytes = to_bytes(body, MAX_ERROR_BODY_BYTES)
        .await
        .unwrap_or_default();
    let mut envelope = serde_json::from_slice::<Value>(&bytes).unwrap_or_else(|_| {
        error_envelope(
            ErrorFormat::Claude,
            status.as_u16(),
            "Upstream service error",
            Some(request_id),
        )
    });
    envelope["error"]["status"] = Value::from(status.as_u16());
    parts.status = StatusCode::OK;
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    parts
        .headers
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    parts.headers.insert(
        "x-crosslane-error-status",
        HeaderValue::from(status.as_u16()),
    );
    let response = Response::from_parts(
        parts,
        Body::from(initial_error_stream(&envelope, request_id, model)),
    );
    stream::protect_response(response, request_id, model)
}

pub(super) async fn adapt_ccr_response(
    response: Response,
    request_id: &str,
    model: &str,
) -> Response {
    let response = adapt_claude_response(response, request_id).await;
    stream::protect_response(response, request_id, model)
}

/// Inspect an automatic candidate before any content reaches the caller.
pub(super) async fn inspect_provider_candidate(response: Response, request_id: &str) -> Response {
    if !response.status().is_success()
        || !response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"))
    {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let mut source = body.into_data_stream();
    let mut prefix = BytesMut::new();
    let mut examined = 0;
    let deadline = tokio::time::Instant::now() + PREFLIGHT_WAIT;
    parts
        .extensions
        .insert(CandidatePreflightDeadline(deadline));
    loop {
        while let Some((end, event)) = next_event(&prefix, examined) {
            let value = stream::event_value(&prefix[examined..end]);
            examined = end;
            if let PreludeEvent::Error {
                message,
                code,
                status,
                metadata,
            } = event
            {
                drop(source);
                let response = error_response(
                    status,
                    diagnostic_envelope(
                        status,
                        &message,
                        &code,
                        Some(status.as_u16()),
                        request_id,
                        Some(&metadata),
                    ),
                    request_id,
                );
                return rejected_candidate(response);
            }
            // A tool start already commits this candidate. Never wait for a
            // complete tool block and then transparently replay its action.
            let prelude = match value.as_ref() {
                Some(value) => matches!(value["type"].as_str(), Some("message_start" | "ping")),
                None => matches!(event, PreludeEvent::Prelude),
            };
            if !prelude {
                return restore_candidate(parts, prefix, source);
            }
        }
        if prefix.len() >= MAX_PREFLIGHT_BYTES {
            return restore_candidate(parts, prefix, source);
        }
        match tokio::time::timeout_at(deadline, source.next()).await {
            Ok(Some(Ok(bytes))) if bytes.len() <= MAX_PREFLIGHT_BYTES - prefix.len() => {
                prefix.extend_from_slice(&bytes)
            }
            Ok(Some(Err(_))) => {
                drop(source);
                return rejected_candidate(json_error(
                    StatusCode::BAD_GATEWAY,
                    "Upstream stream transport failed before assistant content",
                    "upstream_transport_interrupted",
                    None,
                    request_id,
                ));
            }
            Ok(Some(chunk)) => {
                return restore_candidate(
                    parts,
                    prefix,
                    futures::stream::iter([chunk]).chain(source),
                )
            }
            Ok(None) => {
                return rejected_candidate(json_error(
                    StatusCode::BAD_GATEWAY,
                    "Upstream event stream ended before assistant content",
                    "upstream_event_stream_empty",
                    None,
                    request_id,
                ));
            }
            Err(_) => return restore_candidate(parts, prefix, source),
        }
    }
}

fn rejected_candidate(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert("x-crosslane-candidate-error", HeaderValue::from_static("1"));
    response
}

fn restore_candidate(
    parts: axum::http::response::Parts,
    prefix: BytesMut,
    source: impl futures::Stream<Item = Result<Bytes, axum::Error>> + Send + 'static,
) -> Response {
    let body = futures::stream::once(async { Ok::<_, axum::Error>(prefix.freeze()) }).chain(source);
    Response::from_parts(parts, Body::from_stream(body))
}

fn initial_error_stream(envelope: &Value, request_id: &str, model: &str) -> String {
    // Claude Code retries an error-only stream without streaming. CRS then
    // sanitizes that HTTP error and erases the original diagnostics again.
    // A completed empty block makes the SSE error terminal for the client.
    // Initialization has zero usage and ends with error, never message_stop.
    let model = log_model(model);
    let start = serde_json::json!({
        "type":"message_start",
        "message":{
            "id":format!("msg_{request_id}"),
            "type":"message",
            "role":"assistant",
            "model":if model.is_empty() { "unknown" } else { &model },
            "content":[],
            "stop_reason":null,
            "stop_sequence":null,
            "usage":{"input_tokens":0,"output_tokens":0}
        }
    });
    format!(
        "event: message_start\ndata: {start}\n\n\
         event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
         event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
         event: error\ndata: {envelope}\n\n"
    )
}

pub(super) async fn adapt_claude_response(response: Response, request_id: &str) -> Response {
    if response.status() != StatusCode::OK {
        return normalize_http_error(response, request_id).await;
    }
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim();
    if content_type.eq_ignore_ascii_case("text/event-stream") {
        let started = Instant::now();
        let response = preflight_stream(response, request_id).await;
        tracing::info!(
            event = "proxy.stream.preflight",
            trace_id = request_id,
            duration_ms = started.elapsed().as_millis() as u64,
            http_status = response.status().as_u16(),
            "CRS stream preflight completed"
        );
        response
    } else {
        normalize_json_success(response, request_id).await
    }
}

async fn normalize_json_success(response: Response, request_id: &str) -> Response {
    let (parts, body) = response.into_parts();
    let mut stream = body.into_data_stream();
    let mut prefix = BytesMut::new();
    let deadline = tokio::time::Instant::now() + PREFLIGHT_WAIT;
    loop {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(chunk))) if chunk.len() > MAX_ERROR_BODY_BYTES - prefix.len() => {
                let tail = futures::stream::iter([Ok::<Bytes, axum::Error>(chunk)]).chain(stream);
                return restore_unmodified_json(parts, prefix, tail);
            }
            Ok(Some(Ok(chunk))) => prefix.extend_from_slice(&chunk),
            Ok(Some(Err(_))) => {
                finish_discarded_stream(stream);
                return json_error(
                    StatusCode::BAD_GATEWAY,
                    "Upstream JSON response transport failed",
                    "upstream_transport_interrupted",
                    None,
                    request_id,
                );
            }
            Ok(None) => break,
            Err(_) => return restore_unmodified_json(parts, prefix, stream),
        }
    }
    let parsed = serde_json::from_slice::<Value>(&prefix).ok();
    let Some(error) = parsed.as_ref().filter(|value| is_error_payload(value)) else {
        return Response::from_parts(parts, Body::from(prefix.freeze()));
    };
    let message = error_message(error).unwrap_or("Upstream service error");
    let code = error_code(error).unwrap_or("upstream_unavailable");
    let kind = error
        .pointer("/error/type")
        .or_else(|| error.pointer("/response/error/type"))
        .and_then(Value::as_str);
    let envelope = diagnostic_envelope(
        status_for_error(code, kind),
        message,
        code,
        Some(StatusCode::OK.as_u16()),
        request_id,
        Some(error),
    );
    let mut response = error_response(status_for_error(code, kind), envelope, request_id);
    if let Some(retry_after) = parts.headers.get(header::RETRY_AFTER) {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, retry_after.clone());
    }
    response
}

fn restore_unmodified_json(
    parts: axum::http::response::Parts,
    prefix: BytesMut,
    mut tail: impl futures::Stream<Item = Result<Bytes, axum::Error>> + Unpin + Send + 'static,
) -> Response {
    let body = async_stream::stream! {
        if !prefix.is_empty() {
            yield Ok::<Bytes, axum::Error>(prefix.freeze());
        }
        while let Some(chunk) = tail.next().await {
            yield chunk;
        }
    };
    Response::from_parts(parts, Body::from_stream(body))
}

fn is_error_payload(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(Value::as_str),
        Some("error" | "response.failed")
    ) || value.get("status").and_then(Value::as_str) == Some("failed")
        || value
            .get("error")
            .is_some_and(|error| error.is_object() || error.is_string())
        || value
            .pointer("/response/error")
            .is_some_and(|error| error.is_object() || error.is_string())
}

async fn normalize_http_error(response: Response, request_id: &str) -> Response {
    let original_status = response.status();
    let (parts, body) = response.into_parts();
    let bytes = tokio::time::timeout(PREFLIGHT_WAIT, to_bytes(body, MAX_ERROR_BODY_BYTES))
        .await
        .ok()
        .and_then(Result::ok);
    let parsed = bytes
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok());
    let sse_error = bytes.as_deref().and_then(error_from_sse);
    let text_message = bytes
        .as_deref()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .filter(|text| !text.trim().is_empty());
    let message = parsed
        .as_ref()
        .and_then(error_message)
        .or_else(|| sse_error.as_ref().and_then(error_message))
        .or_else(|| parsed.is_none().then_some(text_message).flatten())
        .unwrap_or("Upstream service error");
    let parsed_code = parsed
        .as_ref()
        .and_then(error_code)
        .or_else(|| sse_error.as_ref().and_then(error_code));
    let status = if original_status.is_client_error() || original_status.is_server_error() {
        original_status
    } else {
        // A malformed success/redirect is a local protocol failure, not a
        // supplier HTTP error whose status needs to be forwarded.
        StatusCode::BAD_GATEWAY
    };
    let code = parsed_code.unwrap_or_else(|| match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "upstream_authentication_failed",
        StatusCode::PAYMENT_REQUIRED => "upstream_quota_exhausted",
        _ => default_code(status),
    });
    let metadata = parsed.as_ref().or(sse_error.as_ref());
    let upstream_status = metadata
        .and_then(|value| value.pointer("/error/upstream_status"))
        .and_then(Value::as_u64)
        .and_then(|status| u16::try_from(status).ok())
        .filter(|status| (100..=599).contains(status))
        .or_else(|| {
            parts
                .headers
                .get("x-crosslane-upstream-status")?
                .to_str()
                .ok()?
                .parse::<u16>()
                .ok()
        })
        .or_else(|| {
            (!parts.headers.contains_key("x-crosslane-error-code"))
                .then_some(original_status.as_u16())
        });
    let envelope =
        diagnostic_envelope(status, message, code, upstream_status, request_id, metadata);
    let mut response = error_response(status, envelope, request_id);
    for name in [
        "x-crosslane-error-code",
        "x-crosslane-error-stage",
        "x-crosslane-account-error",
        "x-crosslane-upstream-status",
    ] {
        if let Some(value) = parts.headers.get(name) {
            response
                .headers_mut()
                .insert(header::HeaderName::from_static(name), value.clone());
        }
    }
    if let Some(retry_after) = parts.headers.get(header::RETRY_AFTER) {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, retry_after.clone());
    }
    response
}

async fn preflight_stream(response: Response, request_id: &str) -> Response {
    let (parts, body) = response.into_parts();
    let mut stream = body.into_data_stream();
    let mut prefix = BytesMut::new();
    let mut examined = 0;
    let deadline = parts
        .extensions
        .get::<CandidatePreflightDeadline>()
        .map_or_else(
            || tokio::time::Instant::now() + PREFLIGHT_WAIT,
            |value| value.0,
        );
    loop {
        while let Some((end, event)) = next_event(&prefix, examined) {
            examined = end;
            match event {
                PreludeEvent::Error {
                    message,
                    code,
                    status,
                    metadata,
                } => {
                    finish_discarded_stream(stream);
                    let upstream_status = metadata
                        .pointer("/error/upstream_status")
                        .and_then(Value::as_u64)
                        .and_then(|status| u16::try_from(status).ok());
                    let envelope = diagnostic_envelope(
                        status,
                        &message,
                        &code,
                        upstream_status,
                        request_id,
                        Some(&metadata),
                    );
                    return error_response(status, envelope, request_id);
                }
                PreludeEvent::Success => {
                    return restore_stream(parts, prefix, stream, request_id.to_owned());
                }
                PreludeEvent::Prelude => {}
            }
        }
        if prefix.len() >= MAX_PREFLIGHT_BYTES {
            return restore_stream(parts, prefix, stream, request_id.to_owned());
        }
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(chunk))) if chunk.len() > MAX_PREFLIGHT_BYTES - prefix.len() => {
                let tail = futures::stream::iter([Ok::<Bytes, axum::Error>(chunk)]).chain(stream);
                return restore_stream(parts, prefix, tail, request_id.to_owned());
            }
            Ok(Some(Ok(chunk))) => prefix.extend_from_slice(&chunk),
            Ok(Some(Err(_))) => {
                finish_discarded_stream(stream);
                return json_error(
                    StatusCode::BAD_GATEWAY,
                    "Upstream stream transport failed",
                    "upstream_transport_interrupted",
                    None,
                    request_id,
                );
            }
            Ok(None) => {
                return json_error(
                    StatusCode::BAD_GATEWAY,
                    "Upstream event stream ended before assistant content",
                    "upstream_event_stream_empty",
                    None,
                    request_id,
                );
            }
            Err(_) => return restore_stream(parts, prefix, stream, request_id.to_owned()),
        }
    }
}

fn restore_stream(
    parts: axum::http::response::Parts,
    prefix: BytesMut,
    mut tail: impl futures::Stream<Item = Result<Bytes, axum::Error>> + Unpin + Send + 'static,
    request_id: String,
) -> Response {
    let body = async_stream::stream! {
        let mut rewriter = SseErrorRewriter::new(request_id);
        for frame in rewriter.push(prefix.freeze()) {
            yield Ok::<Bytes, axum::Error>(frame);
        }
        while let Some(chunk) = tail.next().await {
            match chunk {
                Ok(chunk) => {
                    for frame in rewriter.push(chunk) {
                        yield Ok::<Bytes, axum::Error>(frame);
                    }
                }
                Err(_) => {
                    finish_discarded_stream(tail);
                    // An unfinished frame would concatenate with the error
                    // event and make the latter invisible to CRS's line parser.
                    let partial = rewriter.finish().is_some() || rewriter.passthrough;
                    if partial { yield Ok(Bytes::from_static(b"\n\n")); }
                    if !rewriter.saw_error {
                        yield Ok(sse_error_bytes(
                            "Upstream stream transport failed",
                            "upstream_transport_interrupted",
                            StatusCode::BAD_GATEWAY,
                            &rewriter.request_id,
                        ));
                    }
                    break;
                }
            }
        }
        if let Some(pending) = rewriter.finish() {
            yield Ok(pending);
        }
    };
    Response::from_parts(parts, Body::from_stream(body))
}

struct SseErrorRewriter {
    request_id: String,
    pending: BytesMut,
    passthrough: bool,
    saw_error: bool,
    boundary: stream::EventBoundary,
}

impl SseErrorRewriter {
    fn new(request_id: String) -> Self {
        Self {
            request_id,
            pending: BytesMut::new(),
            passthrough: false,
            saw_error: false,
            boundary: stream::EventBoundary::default(),
        }
    }

    fn push(&mut self, chunk: Bytes) -> Vec<Bytes> {
        if chunk.is_empty() {
            return Vec::new();
        }
        let mut output = Vec::new();
        let mut offset = 0;
        while offset < chunk.len() {
            if self.passthrough {
                let start = offset;
                while offset < chunk.len() {
                    let end = self.boundary.push(chunk[offset]);
                    offset += 1;
                    if end {
                        self.passthrough = false;
                        break;
                    }
                }
                output.push(chunk.slice(start..offset));
                continue;
            }
            let take = (MAX_PREFLIGHT_BYTES - self.pending.len()).min(chunk.len() - offset);
            self.pending
                .extend_from_slice(&chunk[offset..offset + take]);
            offset += take;
            while let Some((end, event)) = next_event(&self.pending, 0) {
                let frame = self.pending.split_to(end).freeze();
                match event {
                    PreludeEvent::Error {
                        message,
                        code,
                        status,
                        metadata,
                    } => {
                        self.saw_error = true;
                        let upstream_status = metadata
                            .pointer("/error/upstream_status")
                            .and_then(Value::as_u64)
                            .and_then(|status| u16::try_from(status).ok());
                        let mut envelope = diagnostic_envelope(
                            status,
                            &message,
                            &code,
                            upstream_status,
                            &self.request_id,
                            Some(&metadata),
                        );
                        envelope["error"]["status"] = Value::from(status.as_u16());
                        output.push(Bytes::from(format!("event: error\ndata: {envelope}\n\n")));
                    }
                    PreludeEvent::Prelude | PreludeEvent::Success => output.push(frame),
                }
            }
            if self.pending.len() == MAX_PREFLIGHT_BYTES {
                self.passthrough = true;
                self.boundary = stream::EventBoundary::default();
                for byte in &self.pending {
                    self.boundary.push(*byte);
                }
                output.push(self.pending.split().freeze());
            }
        }
        output
    }

    fn finish(&mut self) -> Option<Bytes> {
        (!self.pending.is_empty()).then(|| self.pending.split().freeze())
    }
}

fn sse_error_bytes(message: &str, code: &str, status: StatusCode, request_id: &str) -> Bytes {
    let mut envelope = error_envelope(
        ErrorFormat::Claude,
        status.as_u16(),
        message,
        Some(request_id),
    );
    enrich_error_envelope(&mut envelope, code, None, Some(request_id));
    envelope["error"]["status"] = Value::from(status.as_u16());
    Bytes::from(format!("event: error\ndata: {envelope}\n\n"))
}

pub(super) fn stream_error_envelope(
    message: &str,
    code: &str,
    stage: &str,
    upstream_status: Option<u16>,
    request_id: &str,
) -> Value {
    let metadata = serde_json::json!({"error":{"stage":stage}});
    let status = upstream_status
        .and_then(|status| StatusCode::from_u16(status).ok())
        .filter(|status| status.is_client_error() || status.is_server_error())
        .unwrap_or_else(|| status_for_error(code, None));
    let mut envelope = diagnostic_envelope(
        status,
        message,
        code,
        upstream_status,
        request_id,
        Some(&metadata),
    );
    envelope["error"]["status"] = Value::from(status.as_u16());
    envelope
}

fn finish_discarded_stream(
    mut stream: impl futures::Stream<Item = Result<Bytes, axum::Error>> + Unpin + Send + 'static,
) {
    tokio::spawn(async move {
        let _ = tokio::time::timeout(Duration::from_secs(30), async {
            while stream.next().await.is_some() {}
        })
        .await;
    });
}

enum PreludeEvent {
    Prelude,
    Success,
    Error {
        message: String,
        code: String,
        status: StatusCode,
        metadata: Value,
    },
}

fn next_event(buffer: &[u8], offset: usize) -> Option<(usize, PreludeEvent)> {
    let remaining = buffer.get(offset..)?;
    let lf = remaining
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|end| (offset + end, 2));
    let crlf = remaining
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|end| (offset + end, 4));
    let end = match (lf, crlf) {
        (Some(lf), Some(crlf)) => Some(if lf.0 < crlf.0 { lf } else { crlf }),
        (Some(end), None) | (None, Some(end)) => Some(end),
        (None, None) => None,
    }?;
    let frame = String::from_utf8_lossy(&buffer[offset..end.0]);
    let next = end.0 + end.1;
    let mut event_name = "";
    let mut data = String::new();
    for line in frame.lines().map(str::trim_end) {
        if let Some(name) = line.strip_prefix("event:") {
            event_name = name.trim();
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.trim_start());
        }
    }
    let parsed = serde_json::from_str::<Value>(&data).ok();
    let event_type = parsed
        .as_ref()
        .and_then(|value| value.get("type").and_then(Value::as_str));
    if event_name == "error" || event_type == Some("error") {
        let message = parsed
            .as_ref()
            .and_then(error_message)
            .unwrap_or("Upstream stream failed");
        let code = parsed
            .as_ref()
            .and_then(error_code)
            .unwrap_or("upstream_unavailable");
        let kind = parsed
            .as_ref()
            .and_then(|value| value.pointer("/error/type").and_then(Value::as_str));
        return Some((
            next,
            PreludeEvent::Error {
                message: message.to_owned(),
                code: code.to_owned(),
                status: parsed
                    .as_ref()
                    .and_then(|value| {
                        ["/error/upstream_status", "/error/status"]
                            .into_iter()
                            .find_map(|pointer| {
                                let status = value.pointer(pointer)?.as_u64()?;
                                let status = u16::try_from(status).ok()?;
                                let status = StatusCode::from_u16(status).ok()?;
                                (status.is_client_error() || status.is_server_error())
                                    .then_some(status)
                            })
                    })
                    .unwrap_or_else(|| status_for_error(code, kind)),
                metadata: parsed.unwrap_or(Value::Null),
            },
        ));
    }
    if frame.is_empty()
        || (data.is_empty() && frame.starts_with(':'))
        || matches!(event_name, "ping" | "message_start" | "content_block_start")
        || matches!(
            event_type,
            Some("ping" | "message_start" | "content_block_start")
        )
    {
        return Some((next, PreludeEvent::Prelude));
    }
    Some((next, PreludeEvent::Success))
}

fn error_message(value: &Value) -> Option<&str> {
    value
        .pointer("/error/detail")
        .or_else(|| value.pointer("/error/message"))
        .and_then(Value::as_str)
        .or_else(|| value.get("error").and_then(Value::as_str))
        .or_else(|| {
            value
                .pointer("/response/error/message")
                .and_then(Value::as_str)
        })
        .or_else(|| value.get("message").and_then(Value::as_str))
}

fn error_code(value: &Value) -> Option<&str> {
    value
        .pointer("/error/code")
        .or_else(|| value.pointer("/response/error/code"))
        .or_else(|| value.get("code"))
        .and_then(Value::as_str)
        .filter(|code| valid_code(code) && safe_code(code) == *code)
        .or_else(|| {
            crosslane_translate::error::is_context_length_error(error_message(value)?)
                .then_some("context_length_exceeded")
        })
        .or_else(|| {
            let kind = value
                .pointer("/error/type")
                .or_else(|| value.pointer("/response/error/type"))
                .and_then(Value::as_str)?;
            Some(match kind {
                "invalid_request_error" => "invalid_request",
                "rate_limit_error" => "upstream_rate_limited",
                "authentication_error" | "permission_error" => "upstream_authentication_failed",
                "overloaded_error" => "upstream_overloaded",
                "timeout_error" => "upstream_idle_timeout",
                _ => return None,
            })
        })
}

fn error_from_sse(body: &[u8]) -> Option<Value> {
    let mut offset = 0;
    while let Some((next, event)) = next_event(body, offset) {
        offset = next;
        if let PreludeEvent::Error {
            message,
            code,
            metadata,
            ..
        } = event
        {
            if metadata.is_object() {
                return Some(metadata);
            }
            return Some(serde_json::json!({"error":{"message":message,"code":code}}));
        }
    }
    None
}

fn status_for_error(code: &str, kind: Option<&str>) -> StatusCode {
    match code {
        "context_length_exceeded"
        | "request_payload_exceeded"
        | "tool_catalog_too_large"
        | "unsupported_tool_protocol"
        | "tool_budget_exceeded"
        | "invalid_tool_protocol"
        | "upstream_content_filtered"
        | "invalid_request" => StatusCode::BAD_REQUEST,
        "upstream_rate_limited" | "upstream_quota_exhausted" | "rate_limit_error" => {
            StatusCode::TOO_MANY_REQUESTS
        }
        "upstream_idle_timeout" => StatusCode::GATEWAY_TIMEOUT,
        "upstream_authentication_failed" | "upstream_overloaded" | "server_overloaded" => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        _ => match kind {
            Some("invalid_request_error") => StatusCode::BAD_REQUEST,
            Some("rate_limit_error") => StatusCode::TOO_MANY_REQUESTS,
            Some("authentication_error" | "permission_error" | "overloaded_error") => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            _ => StatusCode::BAD_GATEWAY,
        },
    }
}

fn default_code(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => "invalid_request",
        StatusCode::PAYLOAD_TOO_LARGE => "request_payload_exceeded",
        StatusCode::TOO_MANY_REQUESTS => "upstream_rate_limited",
        StatusCode::GATEWAY_TIMEOUT => "upstream_idle_timeout",
        _ => "upstream_unavailable",
    }
}

fn valid_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 64
        && code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn safe_code(code: &str) -> &str {
    if valid_code(code) && !code.to_ascii_lowercase().starts_with("ksk_") {
        code
    } else {
        "upstream_unavailable"
    }
}

fn is_opaque_message(message: &str) -> bool {
    matches!(
        message,
        "Upstream service error, please retry later"
            | "Service temporarily unavailable, please retry"
            | "service unavailable"
            | "Upstream service error"
    )
}

fn json_error(
    status: StatusCode,
    message: &str,
    code: &str,
    upstream_status: Option<u16>,
    request_id: &str,
) -> Response {
    let envelope = diagnostic_envelope(status, message, code, upstream_status, request_id, None);
    error_response(status, envelope, request_id)
}

fn diagnostic_envelope(
    status: StatusCode,
    message: &str,
    code: &str,
    upstream_status: Option<u16>,
    request_id: &str,
    metadata: Option<&Value>,
) -> Value {
    let mut envelope = error_envelope(
        ErrorFormat::Claude,
        status.as_u16(),
        message,
        Some(request_id),
    );
    if upstream_status.is_some() {
        envelope["error"]["stage"] = Value::String("provider_upstream".to_owned());
    }
    if let Some(metadata) = metadata {
        if let Some(stage) = metadata
            .pointer("/error/stage")
            .and_then(Value::as_str)
            .filter(|stage| valid_code(stage) && safe_code(stage) == *stage)
        {
            envelope["error"]["stage"] = Value::String(stage.to_owned());
        }
        if metadata.pointer("/error/recovery").and_then(Value::as_str)
            == Some("compaction_retry_failed")
        {
            envelope["error"]["recovery"] = Value::String("compaction_retry_failed".to_owned());
        }
        if metadata
            .pointer("/error/upstream_reason")
            .and_then(Value::as_str)
            == Some("CONTENT_LENGTH_EXCEEDS_THRESHOLD")
        {
            envelope["error"]["upstream_reason"] =
                Value::String("CONTENT_LENGTH_EXCEEDS_THRESHOLD".to_owned());
        }
        if let Some(context) = metadata.pointer("/error/context").and_then(|value| {
            serde_json::from_value::<crate::stats::ContextOverflowDiagnostics>(value.clone()).ok()
        }) {
            envelope["error"]["context"] =
                serde_json::to_value(context).expect("context diagnostics");
        }
    }
    if let Some(reason) = context_rejection_reason(message) {
        envelope["error"]["upstream_reason"] = Value::String(reason.to_owned());
    }
    envelope["error"]["status"] = Value::from(status.as_u16());
    enrich_error_envelope(&mut envelope, code, upstream_status, Some(request_id));
    envelope
}

fn error_response(status: StatusCode, envelope: Value, request_id: &str) -> Response {
    let upstream_status = envelope
        .pointer("/error/upstream_status")
        .and_then(Value::as_u64)
        .and_then(|status| u16::try_from(status).ok());
    let mut response = (status, axum::Json(envelope)).into_response();
    if let Some(upstream_status) = upstream_status {
        response.headers_mut().insert(
            "x-crosslane-upstream-status",
            HeaderValue::from(upstream_status),
        );
    }
    response.headers_mut().insert(
        "x-crosslane-error-status",
        HeaderValue::from(status.as_u16()),
    );
    if matches!(
        status,
        StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
            | StatusCode::TOO_MANY_REQUESTS
    ) {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    if let Ok(value) = HeaderValue::from_str(request_id) {
        response.headers_mut().insert("request-id", value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn automatic_and_response_preflight_share_one_wait_window() {
        let source = async_stream::stream! {
            yield Ok::<_, std::io::Error>(Bytes::from_static(b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n"));
            tokio::time::sleep(Duration::from_secs(12)).await;
            yield Ok(Bytes::from_static(b"event: ping\ndata: {\"type\":\"ping\"}\n\n"));
        };
        let response = Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(source))
            .unwrap();
        let started = Instant::now();
        let response = inspect_provider_candidate(response, "trace_wait").await;
        assert!(response
            .extensions()
            .get::<CandidatePreflightDeadline>()
            .is_some());
        let response = adapt_claude_stream_response(response, "trace_wait", "model").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            started.elapsed() < Duration::from_secs(7),
            "the same prelude must not wait for a second five-second window"
        );
    }

    #[tokio::test]
    async fn oversized_event_resynchronizes_before_later_error_for_lf_and_crlf() {
        for newline in ["\n", "\r\n"] {
            for chunk_size in [257, 4093, 131072] {
                let content = format!("event: content_block_delta{newline}data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{}\"}}}}{newline}{newline}", "x".repeat(70_000));
                let error = format!("event: error{newline}data: {{\"type\":\"error\",\"error\":{{\"type\":\"invalid_request_error\",\"message\":\"large event followed by rejected field\"}}}}{newline}{newline}");
                let input = content.clone() + &error;
                let chunks = input
                    .as_bytes()
                    .chunks(chunk_size)
                    .map(|chunk| Ok::<_, std::io::Error>(Bytes::copy_from_slice(chunk)))
                    .collect::<Vec<_>>();
                let response = Response::builder()
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .body(Body::from_stream(futures::stream::iter(chunks)))
                    .unwrap();
                let bytes = to_bytes(
                    adapt_claude_stream_response(response, "trace_big_event", "model")
                        .await
                        .into_body(),
                    1024 * 1024,
                )
                .await
                .unwrap();
                assert!(bytes.starts_with(content.as_bytes()));
                let error = error_from_sse(&bytes).expect("later error must remain visible");
                assert_eq!(error["error"]["code"], "invalid_request");
                assert_eq!(error["request_id"], "trace_big_event");
                assert!(error["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("rejected field"));
            }
        }
    }

    #[tokio::test]
    async fn ccr_preserves_http_400_and_console_preserves_sse_diagnostics() {
        for ccr in [false, true] {
            let response = Response::builder().status(400).body(Body::from(r#"{"type":"error","error":{"type":"invalid_request_error","message":"output_config.effort: Extra inputs are not permitted"}}"#)).unwrap();
            let response = if ccr {
                adapt_ccr_response(response, "trace_contract", "model").await
            } else {
                adapt_claude_stream_response(response, "trace_contract", "model").await
            };
            assert_eq!(
                response.status(),
                if ccr {
                    StatusCode::BAD_REQUEST
                } else {
                    StatusCode::OK
                }
            );
            let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
            assert!(std::str::from_utf8(&bytes)
                .unwrap()
                .contains("output_config.effort"));
        }
    }

    #[tokio::test]
    async fn nonstream_context_error_preserves_typed_failure_and_numeric_diagnostics() {
        let response = Response::builder()
            .status(400)
            .header("content-type", "application/json")
            .header("x-crosslane-error-code", "context_length_exceeded")
            .header("x-crosslane-error-stage", "context_validation")
            .header("x-crosslane-account-error", "false")
            .body(Body::from(
                serde_json::json!({
                    "type":"error", "request_id":"trace_old_999999",
                    "secret":"never_return_this",
                    "error":{
                        "type":"invalid_request_error", "status":400,
                        "code":"context_length_exceeded", "stage":"context_validation",
                        "message":"prompt is too long: 200301 tokens > 200000",
                        "context":{
                            "total_input_tokens":200301, "protected_prefix_tokens":100,
                            "tool_definition_tokens":200, "current_message_tokens":200001,
                            "history_tokens":0, "overhead_tokens":0,
                            "minimum_input_tokens":200301, "maximum_input_tokens":200000,
                            "overflow_scope":"current_message"
                        }
                    }
                })
                .to_string(),
            ))
            .unwrap();
        let response = adapt_claude_nonstream_response(response, "trace_new_123456").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(response.headers()["x-crosslane-error-status"], "400");
        assert_eq!(
            response.headers()["x-crosslane-error-code"],
            "context_length_exceeded"
        );
        assert_eq!(response.headers()["x-crosslane-account-error"], "false");
        assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
        let body = body_json(response).await;
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["status"], 400);
        assert_eq!(body["error"]["retryable"], false);
        assert_eq!(body["error"]["stage"], "context_validation");
        assert_eq!(body["error"]["context"]["current_message_tokens"], 200001);
        assert_eq!(body["error"]["context"]["maximum_input_tokens"], 200000);
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains("200001 tokens (limit 200000)"), "{body}");
        assert!(message.contains("/rewind"), "{body}");
        assert!(message.contains("trace_new_123456"), "{body}");
        assert!(!body.to_string().contains("trace_old_999999"));
        assert!(!body.to_string().contains("never_return_this"));
        assert!(body.get("content").is_none());
        assert!(body.get("usage").is_none());
        assert!(body.get("stop_reason").is_none());
    }

    #[tokio::test]
    async fn nonstream_adapter_keeps_non_context_and_supplier_http_statuses() {
        for (status, code, stage, upstream) in [
            (
                400,
                "context_length_exceeded",
                "provider_upstream",
                Some(400),
            ),
            (
                400,
                "context_length_exceeded",
                "context_validation",
                Some(400),
            ),
            (400, "invalid_request", "context_validation", None),
            (400, "context_length_exceeded", "request_validation", None),
            (
                401,
                "upstream_authentication_failed",
                "provider_upstream",
                Some(401),
            ),
            (403, "invalid_api_key", "authentication", None),
            (413, "request_body_too_large", "request_body", None),
            (429, "upstream_rate_limited", "provider_upstream", Some(429)),
            (503, "upstream_unavailable", "provider_upstream", Some(503)),
        ] {
            let mut envelope = serde_json::json!({
                "type":"error","error":{"type":"invalid_request_error","status":status,
                "code":code,"stage":stage,"message":"request rejected"}
            });
            if let Some(upstream) = upstream {
                envelope["error"]["upstream_status"] = Value::from(upstream);
            }
            let response = Response::builder()
                .status(status)
                .header("x-crosslane-error-code", code)
                .body(Body::from(envelope.to_string()))
                .unwrap();
            let response = adapt_claude_nonstream_response(response, "trace_no_remap").await;
            assert_eq!(response.status().as_u16(), status, "{code}/{stage}");
            assert_eq!(
                response.headers()["x-crosslane-error-status"]
                    .to_str()
                    .unwrap(),
                status.to_string()
            );
            let body = body_json(response).await;
            assert_eq!(body["error"]["status"], status);
            if let Some(upstream) = upstream {
                assert_eq!(body["error"]["upstream_status"], upstream);
            }
        }
    }

    #[tokio::test]
    async fn nonstream_adapter_preserves_successful_messages() {
        let message = serde_json::json!({
            "type":"message", "id":"msg_success", "role":"assistant",
            "content":[{"type":"text","text":"context_length_exceeded is an error code"}],
            "stop_reason":"end_turn", "usage":{"input_tokens":5,"output_tokens":7}
        });
        let response = Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(Body::from(message.to_string()))
            .unwrap();
        let response = adapt_claude_nonstream_response(response, "trace_success").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key("x-crosslane-error-status"));
        assert_eq!(body_json(response).await, message);
    }

    async fn body_json(response: Response) -> Value {
        let bytes = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    async fn body_sse_error(response: Response) -> Value {
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/event-stream"
        );
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(text.matches("event: error").count(), 1);
        assert!(text.ends_with("\n\n"));
        let events = text
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|data| serde_json::from_str::<Value>(data).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            events
                .iter()
                .map(|event| event["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "message_start",
                "content_block_start",
                "content_block_stop",
                "error"
            ]
        );
        assert_eq!(events[0]["message"]["content"], serde_json::json!([]));
        assert_eq!(
            events[0]["message"]["usage"],
            serde_json::json!({"input_tokens":0,"output_tokens":0})
        );
        assert_eq!(events[0]["message"]["stop_reason"], Value::Null);
        assert_eq!(
            events[1]["content_block"],
            serde_json::json!({"type":"text","text":""})
        );
        assert_eq!(events[1]["index"], events[2]["index"]);
        events.last().unwrap().clone()
    }

    #[tokio::test]
    async fn supplier_http_errors_keep_status_and_crs_diagnostics() {
        for status in [
            400, 401, 402, 403, 404, 408, 409, 413, 422, 429, 500, 502, 503, 504, 529, 599,
        ] {
            for message in [
                "",
                "Internal Server Error",
                "messages.1.output_config: Extra inputs are not permitted",
            ] {
                let response = Response::builder()
                    .status(status)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::RETRY_AFTER, "17")
                    .body(Body::from(
                        serde_json::json!({"error":{"message":message}}).to_string(),
                    ))
                    .unwrap();
                let adapted = adapt_claude_response(response, "trace_passthrough").await;
                assert_eq!(adapted.status().as_u16(), status);
                assert_eq!(
                    adapted.headers()["x-crosslane-error-status"],
                    status.to_string()
                );
                assert_eq!(adapted.headers()[header::RETRY_AFTER], "17");
                let body = body_json(adapted).await;
                assert_eq!(body["error"]["status"], status);
                assert_eq!(body["error"]["upstream_status"], status);
                assert_eq!(body["request_id"], "trace_passthrough");
                assert!(body["error"]["detail"].is_string());
                assert!(body["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("trace_passthrough"));
            }
        }
    }

    #[tokio::test]
    async fn supplier_plain_text_errors_keep_status_and_sanitized_cause() {
        let response = Response::builder()
            .status(422)
            .header(header::CONTENT_TYPE, "text/plain")
            .body(Body::from("tool schema invalid: ksk_private-credential"))
            .unwrap();
        let adapted = adapt_claude_response(response, "trace_plain_text").await;
        assert_eq!(adapted.status().as_u16(), 422);
        let body = body_json(adapted).await;
        assert_eq!(body["error"]["upstream_status"], 422);
        assert_eq!(body["error"]["detail"], "tool schema invalid: [REDACTED]");
    }

    #[tokio::test]
    async fn supplier_stream_errors_keep_raw_status_in_sse_diagnostics() {
        for status in [
            400, 401, 402, 403, 404, 408, 409, 413, 422, 429, 500, 502, 503, 504, 529, 599,
        ] {
            let response = Response::builder()
                .status(status)
                .body(Body::from(
                    serde_json::json!({"error":{"message":"invalid tool schema"}}).to_string(),
                ))
                .unwrap();
            let adapted =
                adapt_claude_stream_response(response, "trace_sse_status", "claude-sonnet-4.6")
                    .await;
            assert_eq!(
                adapted.headers()["x-crosslane-error-status"],
                status.to_string()
            );
            let body = body_sse_error(adapted).await;
            assert_eq!(body["error"]["status"], status);
            assert_eq!(body["error"]["upstream_status"], status);
            assert_eq!(body["error"]["detail"], "invalid tool schema");
            assert_eq!(body["request_id"], "trace_sse_status");
        }
    }

    #[test]
    fn stream_mode_requires_a_valid_request_and_covers_supplier_http_errors() {
        assert!(stream_requested(br#"{"stream":true,"messages":[]}"#));
        for body in [
            br#"{"stream":false}"#.as_slice(),
            br#"{"stream":"true"}"#,
            br#"{"stream":true"#,
            br#"{}"#,
        ] {
            assert!(!stream_requested(body));
        }
        for status in 400..=599 {
            assert!(uses_sse_error(
                StatusCode::from_u16(status).unwrap(),
                Some(status)
            ));
        }
        for status in [401, 403, 405, 413] {
            assert!(!uses_sse_error(StatusCode::from_u16(status).unwrap(), None));
        }
        assert!(!uses_sse_error(StatusCode::OK, None));
    }

    #[test]
    fn stream_diagnostics_preserve_explicit_supplier_errors() {
        for status in [401, 402, 403, 413, 422, 500, 503, 504, 529] {
            let envelope = stream_error_envelope(
                "supplier error",
                "upstream_unavailable",
                "upstream_dispatch",
                Some(status),
                "trace_stream_status",
            );
            assert_eq!(envelope["error"]["status"], status);
            assert_eq!(envelope["error"]["upstream_status"], status);
            let event = format!("event: error\ndata: {envelope}\n\n");
            let Some((
                _,
                PreludeEvent::Error {
                    status: parsed_status,
                    ..
                },
            )) = next_event(event.as_bytes(), 0)
            else {
                panic!("expected stream error");
            };
            assert_eq!(parsed_status.as_u16(), status);
        }
    }

    #[tokio::test]
    async fn context_threshold_stream_error_survives_crs_with_recovery_and_trace() {
        let mut envelope = error_envelope(
            ErrorFormat::Claude,
            400,
            "Input content length exceeds threshold.",
            Some("trace_old_999"),
        );
        envelope["error"]["stage"] = serde_json::json!("upstream_dispatch");
        envelope["error"]["recovery"] = serde_json::json!("compaction_retry_failed");
        envelope["error"]["upstream_reason"] =
            serde_json::json!("CONTENT_LENGTH_EXCEEDS_THRESHOLD");
        enrich_error_envelope(
            &mut envelope,
            "context_length_exceeded",
            Some(400),
            Some("trace_old_999"),
        );
        let response = Response::builder()
            .status(400)
            .header("x-crosslane-error-code", "context_length_exceeded")
            .header("x-crosslane-account-error", "false")
            .body(Body::from(envelope.to_string()))
            .unwrap();
        let adapted =
            adapt_claude_stream_response(response, "trace_current_123456", "claude-sonnet-4.6")
                .await;
        assert_eq!(adapted.headers()["x-crosslane-error-status"], "400");
        assert_eq!(adapted.headers()["x-crosslane-account-error"], "false");
        let body = body_sse_error(adapted).await;
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["status"], 400);
        assert_eq!(body["error"]["upstream_status"], 400);
        assert_eq!(body["request_id"], "trace_current_123456");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.starts_with("prompt is too long: upstream input limit exceeded"));
        assert!(message.contains("the exact limit was not reported"));
        assert!(message.contains("Compaction was attempted, but the retry still failed"));
        assert!(message.contains("Run /compact"));
        assert!(message.contains("reason: CONTENT_LENGTH_EXCEEDS_THRESHOLD"));
        assert!(message.contains("request ID: trace_current_123456"));
        assert!(!message.contains("trace_old"));
        assert_eq!(message.matches("[code:").count(), 1);
        assert_eq!(body["error"]["retryable"], false);
    }

    #[tokio::test]
    async fn stream_adapter_preserves_numeric_context_and_typed_diagnostics() {
        let mut envelope = error_envelope(
            ErrorFormat::Claude,
            400,
            "prompt is too long: 200001 tokens > 200000",
            Some("trace_old_500999"),
        );
        envelope["error"]["context"] = serde_json::json!({
            "total_input_tokens":200001, "protected_prefix_tokens":100, "tool_definition_tokens":200,
            "current_message_tokens":1000, "history_tokens":198701, "overhead_tokens":0,
            "minimum_input_tokens":1300, "maximum_input_tokens":200000
        });
        enrich_error_envelope(
            &mut envelope,
            "context_length_exceeded",
            None,
            Some("trace_old_500999"),
        );
        let response = Response::builder()
            .status(400)
            .header("x-crosslane-error-code", "context_length_exceeded")
            .body(Body::from(envelope.to_string()))
            .unwrap();
        let body = body_sse_error(
            adapt_claude_stream_response(response, "trace_new_123456", "claude-sonnet-4.6").await,
        )
        .await;
        assert_eq!(
            body["error"]["detail"],
            "prompt is too long: 200001 tokens > 200000"
        );
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("prompt is too long: 200001 tokens > 200000"));
        assert!(
            body["error"].get("upstream_status").is_none(),
            "local validation did not dispatch upstream"
        );
        assert_eq!(body["error"]["context"]["maximum_input_tokens"], 200000);
    }

    #[tokio::test]
    async fn upstream_payload_errors_preserve_http_status_and_diagnostics() {
        for (status, raw, code, client_status) in [
            (413, "payload too large", "request_payload_exceeded", 413),
            (502, "Internal Server Error", "upstream_unavailable", 502),
        ] {
            let response = Response::builder()
                .status(status)
                .body(Body::from(
                    serde_json::json!({"error":{"message":raw}}).to_string(),
                ))
                .unwrap();
            let body = body_json(adapt_claude_response(response, "trace_failure").await).await;
            assert_eq!(body["error"]["upstream_status"], status);
            assert_eq!(body["error"]["status"], client_status);
            assert_eq!(body["error"]["code"], code);
            assert!(body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("request ID: trace_failure"));
        }
    }

    #[tokio::test]
    async fn rich_stream_errors_still_redact_credentials_and_ban_triggers() {
        for raw in [
            "account is disabled; Input content length exceeds threshold; ksk_private-credential",
            "tool schema invalid: ksk_private-credential",
        ] {
            let response = Response::builder()
                .status(400)
                .body(Body::from(
                    serde_json::json!({"error":{"message":raw,"code":"ksk_private-credential"}})
                        .to_string(),
                ))
                .unwrap();
            let body = body_sse_error(
                adapt_claude_stream_response(response, "trace_redacted", "ksk_private-credential")
                    .await,
            )
            .await;
            let rendered = body.to_string();
            assert!(!rendered.contains("ksk_private"));
            assert!(!rendered.contains("account is disabled"));
            assert!(body["error"]["message"].as_str().unwrap().chars().count() <= 1024);
        }
    }

    #[tokio::test]
    async fn copilot_field_validation_error_keeps_cause_stage_and_status() {
        let response = Response::builder()
            .status(400)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.1.output_config: Extra inputs are not permitted"}}"#,
            ))
            .unwrap();
        let adapted =
            adapt_claude_stream_response(response, "trace_copilot", "claude-opus-5-5").await;
        assert_eq!(adapted.headers()["x-crosslane-error-status"], "400");
        let error = body_sse_error(adapted).await;
        assert_eq!(
            error["error"]["detail"],
            "messages.1.output_config: Extra inputs are not permitted"
        );
        assert_eq!(error["error"]["type"], "invalid_request_error");
        assert_eq!(error["error"]["status"], 400);
        assert_eq!(error["error"]["upstream_status"], 400);
        assert_eq!(error["error"]["stage"], "provider_upstream");
        assert_eq!(error["error"]["retryable"], false);
        assert!(error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("request ID: trace_copilot"));
    }

    #[tokio::test]
    async fn opaque_upstream_400_preserves_http_status_and_diagnostics() {
        let response = Response::builder()
            .status(400)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"error":{"message":"Internal Server Error"}}"#,
            ))
            .unwrap();
        let adapted = adapt_claude_response(response, "trace_test").await;
        assert_eq!(adapted.status(), StatusCode::BAD_REQUEST);
        let body = body_json(adapted).await;
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["code"], "invalid_request");
        assert_eq!(body["error"]["upstream_status"], 400);
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("request ID: trace_test"));
    }

    #[tokio::test]
    async fn unexpected_success_status_becomes_an_error_with_a_body() {
        let response = Response::builder().status(204).body(Body::empty()).unwrap();
        let adapted = adapt_claude_response(response, "trace_empty").await;
        assert_eq!(adapted.status(), StatusCode::BAD_GATEWAY);
        let body = body_json(adapted).await;
        assert_eq!(body["error"]["upstream_status"], 204);
        assert_eq!(body["error"]["code"], "upstream_unavailable");
        assert_eq!(body["request_id"], "trace_empty");
    }

    #[tokio::test]
    async fn upstream_200_json_error_becomes_http_error_for_crs() {
        let response = Response::builder()
            .status(200)
            .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
            .body(Body::from(
                r#"{"type":"error","error":{"message":"model is temporarily unavailable","code":"model_unavailable"}}"#,
            ))
            .unwrap();
        let adapted = adapt_claude_response(response, "trace_json_error").await;
        assert_eq!(adapted.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(adapted.headers()[header::CONTENT_TYPE], "application/json");
        let body = body_json(adapted).await;
        assert_eq!(body["error"]["detail"], "model is temporarily unavailable");
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("request ID: trace_json_error"));
        assert_eq!(body["error"]["code"], "model_unavailable");
        assert_eq!(body["error"]["upstream_status"], 200);
        assert_eq!(body["request_id"], "trace_json_error");
    }

    #[tokio::test]
    async fn mislabeled_200_json_error_is_still_detected() {
        for content_type in [None, Some("text/plain")] {
            let mut builder = Response::builder().status(200);
            if let Some(content_type) = content_type {
                builder = builder.header(header::CONTENT_TYPE, content_type);
            }
            let response = builder
                .body(Body::from(r#"{"error":"model is not supported"}"#))
                .unwrap();
            let adapted = adapt_claude_response(response, "trace_mislabeled").await;
            assert_eq!(adapted.status(), StatusCode::BAD_GATEWAY);
            let body = body_json(adapted).await;
            assert_eq!(body["error"]["detail"], "model is not supported");
            assert_eq!(body["error"]["upstream_status"], 200);
        }
    }

    #[tokio::test]
    async fn successful_200_json_body_is_preserved() {
        let input = r#"{"type":"message","content":[{"type":"text","text":"ok"}]}"#;
        let response = Response::builder()
            .status(200)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(input))
            .unwrap();
        let adapted = adapt_claude_response(response, "trace_success").await;
        assert_eq!(adapted.status(), StatusCode::OK);
        let bytes = to_bytes(adapted.into_body(), 1024).await.unwrap();
        assert_eq!(bytes, input);
    }

    #[tokio::test]
    async fn large_successful_json_body_is_not_truncated_by_error_probe() {
        let input = format!(
            r#"{{"type":"message","content":[{{"type":"text","text":"{}"}}]}}"#,
            "x".repeat(MAX_ERROR_BODY_BYTES)
        );
        let source = futures::stream::iter(vec![
            Ok::<Bytes, std::io::Error>(Bytes::copy_from_slice(&input.as_bytes()[..32])),
            Ok(Bytes::copy_from_slice(&input.as_bytes()[32..])),
        ]);
        let response = Response::builder()
            .status(200)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from_stream(source))
            .unwrap();
        let adapted = adapt_claude_response(response, "trace_large_json").await;
        assert_eq!(adapted.status(), StatusCode::OK);
        let bytes = to_bytes(adapted.into_body(), input.len() + 1)
            .await
            .unwrap();
        assert_eq!(bytes, input);
    }

    #[tokio::test]
    async fn string_error_field_is_kept_in_error_message() {
        let response = Response::builder()
            .status(400)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"error":"model is not supported"}"#))
            .unwrap();
        let adapted = adapt_claude_response(response, "trace_string_error").await;
        assert_eq!(adapted.status(), StatusCode::BAD_REQUEST);
        let body = body_json(adapted).await;
        assert_eq!(body["error"]["detail"], "model is not supported");
    }

    #[tokio::test]
    async fn early_sse_error_becomes_http_error_for_crs() {
        let response = Response::builder()
            .status(200)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(concat!(
                "event: ping\ndata: {\"type\":\"ping\"}\n\n",
                "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
                "event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"Internal Server Error\",\"code\":\"upstream_unavailable\"}}\n\n"
            )))
            .unwrap();
        let adapted = adapt_claude_response(response, "trace_stream").await;
        assert_eq!(adapted.status(), StatusCode::BAD_GATEWAY);
        let body = body_json(adapted).await;
        assert_eq!(body["error"]["code"], "upstream_unavailable");
        assert_eq!(body["request_id"], "trace_stream");
    }

    #[tokio::test]
    async fn early_sse_rate_limit_retains_rate_limit_status_and_type() {
        let response = Response::builder()
            .status(200)
            .header(header::CONTENT_TYPE, "Text/Event-Stream; charset=utf-8")
            .body(Body::from(
                "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"Rate limit exceeded\"}}\n\n",
            ))
            .unwrap();
        let adapted = adapt_claude_response(response, "trace_rate_limit").await;
        assert_eq!(adapted.status(), StatusCode::TOO_MANY_REQUESTS);
        let body = body_json(adapted).await;
        assert_eq!(body["error"]["type"], "rate_limit_error");
        assert_eq!(body["error"]["code"], "upstream_rate_limited");
    }

    #[tokio::test]
    async fn actionable_upstream_400_keeps_client_status() {
        let response = Response::builder()
            .status(400)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"type":"error","error":{"message":"prompt is too long: 200001 tokens > 200000"}}"#,
            ))
            .unwrap();
        let adapted = adapt_claude_response(response, "trace_request").await;
        assert_eq!(adapted.status(), StatusCode::BAD_REQUEST);
        let body = body_json(adapted).await;
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(
            body["error"]["detail"],
            "prompt is too long: 200001 tokens > 200000"
        );
        assert_eq!(body["error"]["code"], "context_length_exceeded");
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Run /compact"));
    }

    #[tokio::test]
    async fn upstream_413_preserves_payload_status() {
        let response = Response::builder()
            .status(413)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"error":{"message":"payload too large"}}"#))
            .unwrap();
        let adapted = adapt_claude_response(response, "trace_payload").await;
        assert_eq!(adapted.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = body_json(adapted).await;
        assert_eq!(body["error"]["code"], "request_payload_exceeded");
        assert_eq!(body["error"]["upstream_status"], 413);
    }

    #[tokio::test]
    async fn upstream_auth_status_is_preserved_with_upstream_diagnostics() {
        let response = Response::builder()
            .status(401)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"error":{"message":"Unauthorized"}}"#))
            .unwrap();
        let adapted = adapt_claude_response(response, "trace_auth").await;
        assert_eq!(adapted.status(), StatusCode::UNAUTHORIZED);
        let body = body_json(adapted).await;
        assert_eq!(body["error"]["upstream_status"], 401);
        assert_eq!(body["error"]["type"], "authentication_error");
        assert_eq!(body["error"]["code"], "upstream_authentication_failed");
    }

    #[tokio::test]
    async fn non_200_sse_error_is_normalized_to_json() {
        let response = Response::builder()
            .status(400)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(concat!(
                "event: ping\ndata: {\"type\":\"ping\"}\n\n",
                "event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"Internal Server Error\",\"code\":\"upstream_unavailable\"}}\n\n"
            )))
            .unwrap();
        let adapted = adapt_claude_response(response, "trace_non_200").await;
        assert_eq!(adapted.status(), StatusCode::BAD_REQUEST);
        assert_eq!(adapted.headers()[header::CONTENT_TYPE], "application/json");
        let body = body_json(adapted).await;
        assert_eq!(body["error"]["code"], "upstream_unavailable");
        assert_eq!(body["error"]["upstream_status"], 400);
    }

    #[tokio::test]
    async fn semantic_sse_content_is_forwarded_unchanged() {
        let input = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\"}\n\n"
        );
        let response = Response::builder()
            .status(200)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(input))
            .unwrap();
        let adapted = adapt_claude_response(response, "trace_stream").await;
        assert_eq!(adapted.status(), StatusCode::OK);
        let bytes = to_bytes(adapted.into_body(), 1024 * 1024).await.unwrap();
        assert_eq!(bytes, input);
    }

    #[tokio::test]
    async fn late_upstream_sse_error_keeps_content_and_adds_diagnostics() {
        let content = Bytes::from_static(
            b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\"}\n\n",
        );
        let error = b"event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"Internal Server Error\"}}\n\n";
        let source = futures::stream::iter(vec![
            Ok::<Bytes, std::io::Error>(content.clone()),
            Ok(Bytes::copy_from_slice(&error[..35])),
            Ok(Bytes::copy_from_slice(&error[35..])),
        ]);
        let response = Response::builder()
            .status(200)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(source))
            .unwrap();
        let adapted = adapt_claude_response(response, "trace_late_error").await;
        assert_eq!(adapted.status(), StatusCode::OK);
        let bytes = to_bytes(adapted.into_body(), 1024 * 1024).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.starts_with(std::str::from_utf8(&content).unwrap()));
        assert_eq!(text.matches("event: error").count(), 1);
        assert!(text.contains("upstream_unavailable"));
        assert!(text.contains("request ID: trace_late_error"));
    }

    #[tokio::test]
    async fn late_sse_rate_limit_preserves_error_type() {
        let response = Response::builder()
            .status(200)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(concat!(
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\"}\n\n",
                "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"Rate limit exceeded\"}}\n\n"
            )))
            .unwrap();
        let adapted = adapt_claude_response(response, "trace_late_rate").await;
        assert_eq!(adapted.status(), StatusCode::OK);
        let bytes = to_bytes(adapted.into_body(), 1024 * 1024).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("\"type\":\"rate_limit_error\""));
        assert!(text.contains("\"code\":\"upstream_rate_limited\""));
        assert!(text.contains("trace_late_rate"));
    }

    #[tokio::test]
    async fn large_chunk_of_small_sse_events_still_rewrites_error() {
        let input = format!(
            "{}{}{}",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\"}\n\n",
            "event: ping\ndata: {\"type\":\"ping\"}\n\n".repeat(2_000),
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"Internal Server Error\"}}\n\n"
        );
        assert!(input.len() > MAX_PREFLIGHT_BYTES);
        let response = Response::builder()
            .status(200)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(input))
            .unwrap();
        let adapted = adapt_claude_response(response, "trace_large_chunk").await;
        assert_eq!(adapted.status(), StatusCode::OK);
        let bytes = to_bytes(adapted.into_body(), 1024 * 1024).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert_eq!(text.matches("event: ping").count(), 2_000);
        assert_eq!(text.matches("event: error").count(), 1);
        assert!(text.contains("request ID: trace_large_chunk"));
    }

    #[tokio::test]
    async fn incomplete_sse_prelude_is_a_gateway_error() {
        let response = Response::builder()
            .status(200)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(
                "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
            ))
            .unwrap();
        let adapted = adapt_claude_response(response, "trace_incomplete").await;
        assert_eq!(adapted.status(), StatusCode::BAD_GATEWAY);
        let body = body_json(adapted).await;
        assert_eq!(body["error"]["code"], "upstream_event_stream_empty");
    }

    #[tokio::test]
    async fn late_transport_error_is_forwarded_as_sse_error() {
        let initial = Bytes::from_static(
            b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\"}\n\n",
        );
        let source = futures::stream::iter(vec![
            Ok::<Bytes, std::io::Error>(initial.clone()),
            Ok(Bytes::from_static(
                b"event: message_delta\ndata: {\"type\":",
            )),
            Err(std::io::Error::other("socket closed")),
        ]);
        let response = Response::builder()
            .status(200)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(source))
            .unwrap();
        let adapted = adapt_claude_response(response, "trace_late").await;
        assert_eq!(adapted.status(), StatusCode::OK);
        let bytes = to_bytes(adapted.into_body(), 1024 * 1024).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.starts_with(std::str::from_utf8(&initial).unwrap()));
        assert!(!text.contains("event: message_delta"));
        assert!(text.contains("event: error\ndata:"));
        assert!(text.contains("upstream_transport_interrupted"));
        assert!(text.contains("trace_late"));
    }

    #[tokio::test]
    async fn oversized_first_chunk_is_passed_through_without_loss() {
        let input = format!("data: {}\n\n", "a".repeat(MAX_PREFLIGHT_BYTES));
        let response = Response::builder()
            .status(200)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(input.clone()))
            .unwrap();
        let adapted = adapt_claude_response(response, "trace_large").await;
        assert_eq!(adapted.status(), StatusCode::OK);
        let bytes = to_bytes(adapted.into_body(), MAX_PREFLIGHT_BYTES + 1024)
            .await
            .unwrap();
        assert_eq!(bytes, input);
    }
}

#[cfg(test)]
mod overflow_hint_regressions {
    use super::*;

    fn context(current: u64, minimum: u64) -> Value {
        serde_json::json!({
            "total_input_tokens": minimum + 100,
            "protected_prefix_tokens": minimum - current - 10,
            "tool_definition_tokens": 0,
            "current_message_tokens": current,
            "history_tokens": 100,
            "overhead_tokens": 10,
            "minimum_input_tokens": minimum,
            "maximum_input_tokens": 990000
        })
    }

    #[test]
    fn indivisible_input_recovery_survives_repeated_sse_normalization() {
        let mut envelope = serde_json::json!({"error":{"type":"invalid_request_error","status":400,"message":"prompt is too long: 1200000 tokens > 990000",
            "context":context(1200000, 1200010)}});
        enrich_error_envelope(
            &mut envelope,
            "context_length_exceeded",
            None,
            Some("trace_hint"),
        );
        let hint = envelope["error"]["hint"].as_str().unwrap();
        assert!(hint.contains("Stop retrying the same oversized input"));
        assert!(hint.contains("send a short follow-up"));
        assert!(hint.contains("try /compact"));
        assert!(hint.contains("Esc Esc or /rewind"));
        assert!(hint.contains("cannot make the current input fit"));
        assert!(!envelope.to_string().contains("prompt is too long"));
        assert_eq!(envelope["error"]["retryable"], false);
        let normalized = diagnostic_envelope(
            StatusCode::BAD_REQUEST,
            error_message(&envelope).unwrap(),
            "context_length_exceeded",
            None,
            "trace_hint",
            Some(&envelope),
        );
        assert_eq!(normalized["error"], envelope["error"]);
    }

    #[test]
    fn required_input_recovery_explains_what_history_compaction_cannot_shrink() {
        let mut envelope = serde_json::json!({"error":{
            "message":"prompt is too long: 1200000 tokens > 990000",
            "context":context(100, 1200010)
        }});
        enrich_error_envelope(&mut envelope, "context_length_exceeded", None, None);
        assert!(envelope["error"]["detail"]
            .as_str()
            .unwrap()
            .starts_with("Required input"));
        let hint = envelope["error"]["hint"].as_str().unwrap();
        assert!(hint.contains("required system instructions, enabled tool definitions"));
        assert!(hint.contains("Run /compact only if older history"));
        assert!(!hint.contains("send a short follow-up"));
    }

    #[test]
    fn history_and_supplier_context_errors_keep_native_client_recovery() {
        for (current, minimum, upstream) in [(100, 110, None), (1200000, 1200010, Some(400))] {
            let mut envelope = serde_json::json!({"error":{
                "message":"prompt is too long: 1200000 tokens > 990000",
                "context":context(current, minimum)
            }});
            enrich_error_envelope(&mut envelope, "context_length_exceeded", upstream, None);
            assert!(envelope["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("prompt is too long"));
            let hint = envelope["error"]["hint"].as_str().unwrap();
            if minimum > 990000 {
                assert_eq!(
                    hint,
                    super::super::context_recovery::LEGACY_INPUT_OVERFLOW_HINT
                );
            } else {
                assert!(hint.starts_with("Run /compact"));
            }
        }
    }
}
