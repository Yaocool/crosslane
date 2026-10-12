//! HTTP timings measured without buffering or changing response frames.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::response::Response;
use hyper::body::{Body as HttpBody, Frame, SizeHint};

pub(super) struct RequestCompletion {
    pub started: Instant,
    pub response_ready: Instant,
    pub trace_id: String,
    pub service_id: String,
    pub service_name: String,
    pub request_id: String,
    pub http_method: String,
    pub http_path: String,
    pub user_agent: String,
    pub http_status: u16,
    pub error_code: String,
    pub error_stage: String,
    pub upstream_status: String,
}

pub(super) fn trace_response(response: Response, completion: RequestCompletion) -> Response {
    let (parts, body) = response.into_parts();
    let mut body = TimedBody {
        body,
        completion: Some(completion),
        first_byte_ms: None,
        response_bytes: 0,
    };
    // Hyper can skip polling an empty body, or stop after the last frame when
    // is_end_stream is true. Neither case should be reported as cancellation.
    if body.is_end_stream() {
        body.finish("completed");
    }
    Response::from_parts(parts, Body::new(body))
}

struct TimedBody {
    body: Body,
    completion: Option<RequestCompletion>,
    first_byte_ms: Option<u64>,
    response_bytes: u64,
}

impl TimedBody {
    fn finish(&mut self, outcome: &'static str) {
        let Some(completion) = self.completion.take() else {
            return;
        };
        let finished = Instant::now();
        let duration_ms = elapsed_ms(completion.started, finished);
        let response_ready_ms = elapsed_ms(completion.started, completion.response_ready);
        let response_body_ms = elapsed_ms(completion.response_ready, finished);
        macro_rules! log_completion {
            ($level:ident) => {
                tracing::$level!(
                    event = "http.response.completed",
                    trace_id = %completion.trace_id,
                    service_id = %completion.service_id,
                    service_name = %completion.service_name,
                    request_id = %completion.request_id,
                    http_method = %completion.http_method,
                    http_path = %completion.http_path,
                    user_agent = %completion.user_agent,
                    http_status = completion.http_status,
                    error_code = %completion.error_code,
                    error_stage = %completion.error_stage,
                    upstream_status = %completion.upstream_status,
                    duration_ms,
                    response_ready_ms,
                    first_byte_ms = self.first_byte_ms,
                    response_body_ms,
                    response_bytes = self.response_bytes,
                    outcome,
                    "client response finished"
                )
            };
        }
        match outcome {
            "body_error" => log_completion!(error),
            "cancelled" => log_completion!(warn),
            _ => log_completion!(info),
        }
    }
}

impl HttpBody for TimedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let frame = Pin::new(&mut self.body).poll_frame(cx);
        match &frame {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref().filter(|data| !data.is_empty()) {
                    if self.first_byte_ms.is_none() {
                        self.first_byte_ms = self
                            .completion
                            .as_ref()
                            .map(|completion| elapsed_ms(completion.started, Instant::now()));
                    }
                    self.response_bytes = self.response_bytes.saturating_add(data.len() as u64);
                }
                if self.body.is_end_stream() {
                    self.finish("completed");
                }
            }
            Poll::Ready(Some(Err(_))) => self.finish("body_error"),
            Poll::Ready(None) => self.finish("completed"),
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

impl Drop for TimedBody {
    fn drop(&mut self) {
        self.finish(if self.body.is_end_stream() {
            "completed"
        } else {
            "cancelled"
        });
    }
}

fn elapsed_ms(started: Instant, finished: Instant) -> u64 {
    finished.duration_since(started).as_millis() as u64
}

#[cfg(test)]
mod tests;
