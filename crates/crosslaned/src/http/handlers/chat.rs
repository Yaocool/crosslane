//! Client-side Chat Completions semantics over Kiro's single-response protocol.

use std::convert::Infallible;
use std::pin::Pin;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::BytesMut;
use futures::{Stream, StreamExt};
use serde_json::{json, Value};
use uuid::Uuid;

use super::{ApiError, ErrorFormat, ServiceHttpState};

const MAX_CHAT_BYTES: usize = 50 * 1024 * 1024;

pub(super) fn legacy_chunk(value: &mut Value) -> Result<(), &'static str> {
    for choice in value
        .get_mut("choices")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        for field in ["message", "delta"] {
            if let Some(message) = choice.get_mut(field).and_then(Value::as_object_mut) {
                if let Some(calls) = message.remove("tool_calls") {
                    let calls = calls.as_array().ok_or("invalid internal tool calls")?;
                    if calls.len() > 1
                        || calls.first().is_some_and(|call| {
                            call["index"].as_u64().is_some_and(|index| index > 0)
                        })
                    {
                        return Err(
                            "Kiro returned parallel calls for a legacy single-function request",
                        );
                    }
                    if let Some(call) = calls.first() {
                        if let Some(function) = call.get("function") {
                            message.insert("function_call".into(), function.clone());
                        }
                    }
                }
            }
        }
        if choice["finish_reason"] == "tool_calls" {
            choice["finish_reason"] = json!("function_call");
        }
    }
    Ok(())
}

enum ChatEvent {
    Data(Value),
    Keepalive,
    Done,
}

fn events(
    response: Response,
) -> Pin<Box<dyn Stream<Item = Result<ChatEvent, &'static str>> + Send>> {
    Box::pin(async_stream::stream! {
        let mut source = response.into_body().into_data_stream();
        let mut buffer = BytesMut::new();
        let mut done = false;
        while let Some(chunk) = source.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(_) => {
                    yield Err("chat stream was interrupted");
                    return;
                }
            };
            if done {
                continue;
            }
            buffer.extend_from_slice(&chunk);
            while let Some((end, delimiter)) = super::super::responses::frame_end(&buffer) {
                if end > MAX_CHAT_BYTES {
                    yield Err("chat stream exceeded the frame limit");
                    return;
                }
                let frame = buffer.split_to(end + delimiter);
                let frame = match std::str::from_utf8(&frame[..end]) {
                    Ok(frame) => frame,
                    Err(_) => {
                        yield Err("invalid chat stream encoding");
                        return;
                    }
                };
                let data = frame
                    .lines()
                    .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
                    .collect::<Vec<_>>()
                    .join("\n");
                if data == "[DONE]" {
                    // The native bridge settles usage and releases admission
                    // after sending DONE. Drain it to EOF before allowing the
                    // next candidate to acquire the same account/slot.
                    done = true;
                    buffer.clear();
                    break;
                }
                if data.is_empty() {
                    yield Ok(ChatEvent::Keepalive);
                } else {
                    match serde_json::from_str(&data) {
                        Ok(value) => yield Ok(ChatEvent::Data(value)),
                        Err(_) => {
                            yield Err("invalid chat stream JSON");
                            return;
                        }
                    }
                }
            }
            if buffer.len() > MAX_CHAT_BYTES {
                yield Err("chat stream exceeded the frame limit");
                return;
            }
        }
        yield if done { Ok(ChatEvent::Done) } else { Err("chat stream ended before completion") };
    })
}

fn data(value: &Value) -> Bytes {
    Bytes::from(format!("data: {value}\n\n"))
}
fn stream_error(message: &str) -> Bytes {
    data(
        &json!({"error":{"type":"api_error","message":crosslane_translate::sanitize_error_message(message)}}),
    )
}

pub(super) fn legacy_stream(response: Response) -> Response {
    let (parts, body) = response.into_parts();
    let mut output = Response::from_parts(
        parts,
        Body::from_stream(async_stream::stream! {
            let mut source = events(Response::new(body));
            while let Some(event) = source.next().await {
                let event = match event {
                    Ok(event) => event,
                    Err(message) => {
                        yield Ok::<Bytes, Infallible>(stream_error(message));
                        break;
                    }
                };
                match event {
                    ChatEvent::Data(mut value) => {
                        if let Err(message) = legacy_chunk(&mut value) {
                            yield Ok(stream_error(message));
                            break;
                        }
                        yield Ok(data(&value));
                    }
                    ChatEvent::Keepalive => yield Ok(Bytes::from_static(b": keepalive\n\n")),
                    ChatEvent::Done => break,
                }
            }
            yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
        }),
    );
    output
        .headers_mut()
        .insert(header::CONTENT_TYPE, "text/event-stream".parse().unwrap());
    output
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
    output
}

fn add_usage(total: &mut Value, value: &Value) {
    if let Some(object) = value.as_object() {
        if !total.is_object() {
            *total = json!({});
        }
        for (key, value) in object {
            if let Some(number) = value.as_u64() {
                total[key] = json!(total[key].as_u64().unwrap_or(0).saturating_add(number));
            } else if value.is_object() {
                add_usage(&mut total[key], value);
            }
        }
    }
}

struct CandidateRequest {
    service: ServiceHttpState,
    path: String,
    headers: HeaderMap,
    body: Bytes,
    model: String,
    // The entrypoint's reservation ends when SSE headers are returned, while
    // subsequent candidates still need the serialized request body.
    _body_guard: crate::state::BodyGuard,
}

impl CandidateRequest {
    async fn run(&self, stream_started: bool) -> Result<Response, ApiError> {
        let state = &self.service.app;
        let trace = format!("trace_{}", Uuid::new_v4().simple());
        let started = std::time::Instant::now();
        let mut result = async {
            let connection = state
                .connections
                .try_acquire()
                .ok_or_else(|| ApiError::overloaded(ErrorFormat::OpenAi))?;
            let admission = state
                .admission
                .try_acquire()
                .ok_or_else(|| ApiError::overloaded(ErrorFormat::OpenAi))?;
            Box::pin(super::request::handle_openai(
                self.service.clone(),
                trace.clone(),
                self.path.clone(),
                self.headers.clone(),
                self.body.clone(),
                connection,
                admission,
            ))
            .await
        }
        .await;
        // Once the outer SSE response starts, errors no longer pass through
        // the HTTP entrypoint. Record them here with the committed HTTP status.
        if stream_started {
            if let Err(error) = &mut result {
                error.client_status = Some(200);
                super::record_failed_request(
                    state,
                    &trace,
                    &self.path,
                    &self.model,
                    started,
                    error,
                );
            }
        }
        result
    }
}

pub(super) fn multiple(
    service: ServiceHttpState,
    path: String,
    headers: HeaderMap,
    body: Bytes,
    count: u32,
    streaming: bool,
    include_usage: bool,
) -> futures::future::BoxFuture<'static, Result<Response, ApiError>> {
    Box::pin(async move {
        let mut value: Value = serde_json::from_slice(&body).map_err(|_| {
            ApiError::response_assembly("invalid internal chat request", ErrorFormat::OpenAi)
        })?;
        drop(body);
        value["n"] = json!(1);
        if streaming {
            value["stream_options"]["include_usage"] = json!(true);
        }
        let model = value["model"].as_str().unwrap_or_default().to_owned();
        let body = Bytes::from(value.to_string());
        drop(value);
        let body_guard = service
            .app
            .body_budget
            .reserve(body.len())
            .ok_or_else(|| ApiError::overloaded(ErrorFormat::OpenAi))?;
        let request = CandidateRequest {
            service,
            path,
            headers,
            body,
            model,
            _body_guard: body_guard,
        };
        let first = request.run(false).await?;
        if !streaming {
            let mut candidate = Some(first);
            let mut result = json!({});
            let mut usage = json!({});
            let mut choices = Vec::new();
            let mut total_bytes = 0usize;
            for index in 0..count {
                let response = match candidate.take() {
                    Some(response) => response,
                    None => request.run(false).await?,
                };
                let bytes = to_bytes(
                    response.into_body(),
                    MAX_CHAT_BYTES.saturating_sub(total_bytes),
                )
                .await
                .map_err(|_| {
                    ApiError::response_assembly(
                        "combined chat response exceeded the output limit",
                        ErrorFormat::OpenAi,
                    )
                })?;
                total_bytes = total_bytes.saturating_add(bytes.len());
                let mut value: Value = serde_json::from_slice(&bytes).map_err(|_| {
                    ApiError::response_assembly(
                        "invalid internal chat response",
                        ErrorFormat::OpenAi,
                    )
                })?;
                let mut choice = value
                    .get_mut("choices")
                    .and_then(Value::as_array_mut)
                    .and_then(|choices| (!choices.is_empty()).then(|| choices.remove(0)))
                    .ok_or_else(|| {
                        ApiError::response_assembly(
                            "chat response is missing a choice",
                            ErrorFormat::OpenAi,
                        )
                    })?;
                choice["index"] = json!(index);
                choices.push(choice);
                add_usage(&mut usage, &value["usage"]);
                if index == 0 {
                    result = value;
                }
            }
            result["choices"] = json!(choices);
            result["usage"] = usage;
            return Ok(Json(result).into_response());
        }
        let mut response = Response::new(Body::from_stream(async_stream::stream! {
            let mut next = Some(first);
            let mut usage = json!({});
            let mut envelope = json!({});
            for index in 0..count {
                let response = match next.take() {
                    Some(response) => response,
                    None => match request.run(true).await {
                        Ok(response) => response,
                        Err(error) => {
                            yield Ok::<Bytes, Infallible>(stream_error(&error.message));
                            yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
                            return;
                        }
                    },
                };
                let mut source = events(response);
                while let Some(event) = source.next().await {
                    match event {
                        Ok(ChatEvent::Data(mut value)) => {
                            if !value["error"].is_null() {
                                yield Ok(data(&value));
                                yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
                                return;
                            }
                            if envelope["id"].is_null() {
                                envelope = value.clone();
                            }
                            if !value["usage"].is_null() {
                                add_usage(&mut usage, &value["usage"]);
                            }
                            let choices = value.get_mut("choices").and_then(Value::as_array_mut);
                            if let Some(choices) = choices {
                                if choices.is_empty() {
                                    continue;
                                }
                                for choice in choices {
                                    choice["index"] = json!(index);
                                }
                            }
                            value["id"] = envelope["id"].clone();
                            value["created"] = envelope["created"].clone();
                            if include_usage {
                                value["usage"] = Value::Null;
                            } else {
                                value.as_object_mut().unwrap().remove("usage");
                            }
                            yield Ok(data(&value));
                        }
                        Ok(ChatEvent::Keepalive) => {
                            yield Ok(Bytes::from_static(b": keepalive\n\n"))
                        }
                        Ok(ChatEvent::Done) => break,
                        Err(message) => {
                            yield Ok(stream_error(message));
                            yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
                            return;
                        }
                    }
                }
            }
            if include_usage {
                envelope["choices"] = json!([]);
                envelope["usage"] = usage;
                yield Ok(data(&envelope));
            }
            yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
        }));
        *response.status_mut() = StatusCode::OK;
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, "text/event-stream".parse().unwrap());
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
        Ok(response)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    #[tokio::test]
    async fn terminal_event_waits_for_the_upstream_bridge_to_finish_accounting() {
        let settled = Arc::new(AtomicBool::new(false));
        let completed = settled.clone();
        let body = Body::from_stream(async_stream::stream! {
            yield Ok::<_, Infallible>(Bytes::from_static(b"data: [DONE]\n\n"));
            tokio::task::yield_now().await;
            completed.store(true, Ordering::SeqCst);
        });
        let mut stream = events(Response::new(body));
        assert!(matches!(stream.next().await, Some(Ok(ChatEvent::Done))));
        assert!(
            settled.load(Ordering::SeqCst),
            "starting another candidate now would race with settlement and admission release"
        );
    }
}
