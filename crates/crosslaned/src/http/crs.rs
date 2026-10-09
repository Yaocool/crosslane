//! Response compatibility for Claude Relay Service's Console/CCR relay paths.
//!
//! CRS forwards non-200 bodies directly, but it commits HTTP 200 as soon as
//! Crosslane sends streaming headers. Probe only the bounded SSE prelude so a
//! failure before assistant content can still be returned as a JSON error.

use std::time::Duration;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::BytesMut;
use crosslane_translate::{error_envelope, sanitize_error_message, ErrorFormat};
use futures::StreamExt;
use serde_json::Value;

const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_PREFLIGHT_BYTES: usize = 64 * 1024;
const PREFLIGHT_WAIT: Duration = Duration::from_secs(5);

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
    let message = envelope["error"]["message"].as_str().unwrap_or_default();
    if is_opaque_message(message) {
        envelope["error"]["message"] =
            Value::String(diagnostic_message(code, upstream_status, request_id));
    }
}

pub(super) fn enrich_stream_message(message: String, code: &str, request_id: &str) -> String {
    if is_opaque_message(&message) {
        diagnostic_message(safe_code(code), None, Some(request_id))
    } else {
        message
    }
}

fn diagnostic_message(
    code: &str,
    upstream_status: Option<u16>,
    request_id: Option<&str>,
) -> String {
    let mut detail = format!("Upstream request failed (code: {code}");
    if let Some(status) = upstream_status {
        detail.push_str(&format!(", upstream HTTP {status}"));
    }
    if let Some(request_id) = request_id {
        detail.push_str(&format!(", request ID: {request_id}"));
    }
    detail.push(')');
    detail
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
        preflight_stream(response, request_id).await
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
    let mut response = json_error(
        status_for_error(code, kind),
        message,
        code,
        Some(StatusCode::OK.as_u16()),
        request_id,
    );
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
    let message = parsed
        .as_ref()
        .and_then(error_message)
        .or_else(|| sse_error.as_ref().map(|(message, _)| message.as_str()))
        .unwrap_or("Upstream service error");
    let parsed_code = parsed
        .as_ref()
        .and_then(error_code)
        .or_else(|| sse_error.as_ref().map(|(_, code)| code.as_str()));
    let safe = sanitize_error_message(message);
    let status = if original_status.is_client_error() || original_status.is_server_error() {
        normalized_upstream_status(original_status, &safe)
    } else {
        StatusCode::BAD_GATEWAY
    };
    let code = if status != original_status {
        match original_status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "upstream_authentication_failed",
            StatusCode::PAYLOAD_TOO_LARGE if status == StatusCode::BAD_REQUEST => {
                "request_payload_exceeded"
            }
            _ => "upstream_unavailable",
        }
    } else {
        parsed_code.unwrap_or_else(|| default_code(status))
    };
    let mut response = json_error(
        status,
        &safe,
        code,
        Some(original_status.as_u16()),
        request_id,
    );
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
    let deadline = tokio::time::Instant::now() + PREFLIGHT_WAIT;
    loop {
        while let Some((end, event)) = next_event(&prefix, examined) {
            examined = end;
            match event {
                PreludeEvent::Error {
                    message,
                    code,
                    status,
                } => {
                    finish_discarded_stream(stream);
                    return json_error(status, &message, &code, None, request_id);
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
                    let _ = rewriter.finish();
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
}

impl SseErrorRewriter {
    fn new(request_id: String) -> Self {
        Self {
            request_id,
            pending: BytesMut::new(),
            passthrough: false,
            saw_error: false,
        }
    }

    fn push(&mut self, chunk: Bytes) -> Vec<Bytes> {
        if chunk.is_empty() {
            return Vec::new();
        }
        if self.passthrough {
            return vec![chunk];
        }
        let mut output = Vec::new();
        let mut offset = 0;
        while offset < chunk.len() {
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
                    } => {
                        self.saw_error = true;
                        output.push(sse_error_bytes(&message, &code, status, &self.request_id));
                    }
                    PreludeEvent::Prelude | PreludeEvent::Success => output.push(frame),
                }
            }
            if self.pending.len() == MAX_PREFLIGHT_BYTES {
                self.passthrough = true;
                output.push(self.pending.split().freeze());
                if offset < chunk.len() {
                    output.push(chunk.slice(offset..));
                }
                break;
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
    Bytes::from(format!("event: error\ndata: {envelope}\n\n"))
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
                message: sanitize_error_message(message),
                code: code.to_owned(),
                status: status_for_error(code, kind),
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
        .pointer("/error/message")
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
        .filter(|code| valid_code(code))
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

fn error_from_sse(body: &[u8]) -> Option<(String, String)> {
    let mut offset = 0;
    while let Some((next, event)) = next_event(body, offset) {
        offset = next;
        if let PreludeEvent::Error { message, code, .. } = event {
            return Some((message, code));
        }
    }
    None
}

fn status_for_error(code: &str, kind: Option<&str>) -> StatusCode {
    match code {
        "context_length_exceeded"
        | "tool_budget_exceeded"
        | "invalid_tool_protocol"
        | "invalid_request" => StatusCode::BAD_REQUEST,
        "upstream_rate_limited" | "upstream_quota_exhausted" | "rate_limit_error" => {
            StatusCode::TOO_MANY_REQUESTS
        }
        "upstream_idle_timeout" => StatusCode::GATEWAY_TIMEOUT,
        "upstream_authentication_failed" | "upstream_overloaded" => StatusCode::SERVICE_UNAVAILABLE,
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
    if valid_code(code) {
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

pub(super) fn normalized_upstream_status(status: StatusCode, safe_message: &str) -> StatusCode {
    match status {
        StatusCode::BAD_REQUEST if is_opaque_message(safe_message) => StatusCode::BAD_GATEWAY,
        StatusCode::PAYLOAD_TOO_LARGE if is_opaque_message(safe_message) => StatusCode::BAD_GATEWAY,
        StatusCode::PAYLOAD_TOO_LARGE => StatusCode::BAD_REQUEST,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => StatusCode::SERVICE_UNAVAILABLE,
        _ => status,
    }
}

fn json_error(
    status: StatusCode,
    message: &str,
    code: &str,
    upstream_status: Option<u16>,
    request_id: &str,
) -> Response {
    let mut envelope = error_envelope(
        ErrorFormat::Claude,
        status.as_u16(),
        message,
        Some(request_id),
    );
    enrich_error_envelope(&mut envelope, code, upstream_status, Some(request_id));
    let mut response = (status, axum::Json(envelope)).into_response();
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

    async fn body_json(response: Response) -> Value {
        let bytes = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    #[tokio::test]
    async fn opaque_upstream_400_becomes_actionable_gateway_envelope() {
        let response = Response::builder()
            .status(400)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"error":{"message":"Internal Server Error"}}"#,
            ))
            .unwrap();
        let adapted = adapt_claude_response(response, "trace_test").await;
        assert_eq!(adapted.status(), StatusCode::BAD_GATEWAY);
        let body = body_json(adapted).await;
        assert_eq!(body["error"]["type"], "api_error");
        assert_eq!(body["error"]["code"], "upstream_unavailable");
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
        assert_eq!(body["error"]["message"], "model is temporarily unavailable");
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
            assert_eq!(body["error"]["message"], "model is not supported");
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
        assert_eq!(body["error"]["message"], "model is not supported");
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
            body["error"]["message"],
            "prompt is too long: 200001 tokens > 200000"
        );
    }

    #[tokio::test]
    async fn upstream_413_avoids_claude_codes_inbound_body_limit_message() {
        let response = Response::builder()
            .status(413)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"error":{"message":"payload too large"}}"#))
            .unwrap();
        let adapted = adapt_claude_response(response, "trace_payload").await;
        assert_eq!(adapted.status(), StatusCode::BAD_REQUEST);
        let body = body_json(adapted).await;
        assert_eq!(body["error"]["code"], "request_payload_exceeded");
        assert_eq!(body["error"]["upstream_status"], 413);
    }

    #[tokio::test]
    async fn upstream_auth_status_is_not_presented_as_client_auth_failure() {
        let response = Response::builder()
            .status(401)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"error":{"message":"Unauthorized"}}"#))
            .unwrap();
        let adapted = adapt_claude_response(response, "trace_auth").await;
        assert_eq!(adapted.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(adapted).await;
        assert_eq!(body["error"]["upstream_status"], 401);
        assert_eq!(body["error"]["type"], "overloaded_error");
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
        assert_eq!(adapted.status(), StatusCode::BAD_GATEWAY);
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
