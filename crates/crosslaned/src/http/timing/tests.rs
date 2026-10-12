use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::to_bytes;
use axum::http::HeaderMap;
use futures::future::poll_fn;
use serde_json::Value;

use super::*;

#[derive(Clone)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl Write for LogCapture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn capture() -> (tracing::subscriber::DefaultGuard, LogCapture) {
    let log = LogCapture(Arc::new(Mutex::new(Vec::new())));
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.clone())
        .finish();
    (tracing::subscriber::set_default(subscriber), log)
}

impl LogCapture {
    fn entries(&self) -> Vec<Value> {
        let output = self.0.lock().unwrap();
        std::str::from_utf8(&output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|entry| entry["fields"]["event"] == "http.response.completed")
            .collect()
    }

    fn only(&self, outcome: &str) -> Value {
        let entries = self.entries();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0]["fields"]["outcome"], outcome);
        entries[0].clone()
    }
}

fn context() -> RequestCompletion {
    let response_ready = Instant::now();
    RequestCompletion {
        started: response_ready - Duration::from_millis(10),
        response_ready,
        trace_id: "trace_timing".into(),
        service_id: "svc_timing".into(),
        service_name: "timing".into(),
        request_id: "msg_timing".into(),
        http_method: "GET".into(),
        http_path: "/stream".into(),
        user_agent: "test/1.0".into(),
        http_status: 200,
        error_code: String::new(),
        error_stage: String::new(),
        upstream_status: String::new(),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn streamed_duration_includes_first_byte_wait_and_entire_body() {
    let (_guard, log) = capture();
    let source = async_stream::stream! {
        yield Ok::<_, std::io::Error>(Bytes::new());
        tokio::time::sleep(Duration::from_millis(20)).await;
        yield Ok(Bytes::from_static(b"a"));
        tokio::time::sleep(Duration::from_millis(30)).await;
        yield Ok(Bytes::from_static(b"bc"));
    };
    let response = trace_response(Response::new(Body::from_stream(source)), context());
    assert!(
        log.entries().is_empty(),
        "response headers are not completion"
    );
    let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
    assert_eq!(bytes, "abc");
    let entry = log.only("completed");
    let fields = &entry["fields"];
    assert_eq!(fields["trace_id"], "trace_timing");
    assert_eq!(fields["response_ready_ms"], 10);
    assert!(fields["first_byte_ms"].as_u64().unwrap() >= 30);
    assert!(fields["response_body_ms"].as_u64().unwrap() >= 50);
    assert!(fields["duration_ms"].as_u64().unwrap() >= 60);
    assert_eq!(fields["response_bytes"], 3);
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_is_logged_once_with_partial_bytes_and_duration() {
    let (_guard, log) = capture();
    let source = async_stream::stream! {
        yield Ok::<_, std::io::Error>(Bytes::from_static(b"partial"));
        futures::future::pending::<()>().await;
    };
    let mut body = trace_response(Response::new(Body::from_stream(source)), context()).into_body();
    let frame = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.into_data().unwrap(), "partial");
    assert!(log.entries().is_empty());
    drop(body);
    let entry = log.only("cancelled");
    assert_eq!(entry["level"], "WARN");
    assert_eq!(entry["fields"]["response_bytes"], 7);
    assert!(entry["fields"]["duration_ms"].as_u64().unwrap() >= 10);
}

#[tokio::test(flavor = "current_thread")]
async fn body_errors_are_returned_and_logged_once() {
    let (_guard, log) = capture();
    let source = futures::stream::iter([
        Ok(Bytes::from_static(b"partial")),
        Err(std::io::Error::other("fixture body failure")),
    ]);
    let body = trace_response(Response::new(Body::from_stream(source)), context()).into_body();
    assert!(to_bytes(body, 1024).await.is_err());
    let entry = log.only("body_error");
    assert_eq!(entry["level"], "ERROR");
    assert_eq!(entry["fields"]["response_bytes"], 7);
}

#[test]
fn empty_and_unpolled_bodies_have_no_first_byte() {
    let (_guard, log) = capture();
    drop(trace_response(Response::new(Body::empty()), context()));
    let entry = log.only("completed");
    assert_eq!(entry["fields"]["response_bytes"], 0);
    assert!(entry["fields"].get("first_byte_ms").is_none());
    log.0.lock().unwrap().clear();
    drop(trace_response(
        Response::new(Body::from("unread")),
        context(),
    ));
    let entry = log.only("cancelled");
    assert!(entry["fields"].get("first_byte_ms").is_none());
}

struct Frames(VecDeque<Frame<Bytes>>);

impl HttpBody for Frames {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        Poll::Ready(self.0.pop_front().map(Ok))
    }

    fn is_end_stream(&self) -> bool {
        self.0.is_empty()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(
            self.0
                .iter()
                .filter_map(Frame::data_ref)
                .map(|data| data.len() as u64)
                .sum(),
        )
    }
}

#[tokio::test(flavor = "current_thread")]
async fn frames_trailers_and_size_hint_are_preserved_without_waiting_for_eof_poll() {
    let (_guard, log) = capture();
    let mut trailers = HeaderMap::new();
    trailers.insert("x-trailer", "kept".parse().unwrap());
    let response = Response::builder()
        .header("x-header", "kept")
        .body(Body::new(Frames(VecDeque::from([
            Frame::data(Bytes::from_static(b"ok")),
            Frame::trailers(trailers.clone()),
        ]))))
        .unwrap();
    let response = trace_response(response, context());
    assert_eq!(response.headers()["x-header"], "kept");
    let mut body = response.into_body();
    assert_eq!(body.size_hint().exact(), Some(2));
    assert_eq!(
        poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
            .await
            .unwrap()
            .unwrap()
            .into_data()
            .unwrap(),
        "ok"
    );
    assert!(log.entries().is_empty());
    assert_eq!(
        poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
            .await
            .unwrap()
            .unwrap()
            .into_trailers()
            .unwrap(),
        trailers
    );
    assert!(body.is_end_stream());
    assert_eq!(log.only("completed")["fields"]["response_bytes"], 2);
    drop(body);
    log.only("completed");
}
