#![cfg(feature = "crs")]

use super::crs_isolated_runtime::{
    catalog, configure_daemon, import_profile_account, request, send, text,
};
use super::*;
use futures::StreamExt;
use serde_json::{json, Value};

const PROBE_TEXT: &str = "LATER_AMAZONQ_REQUEST";

#[derive(Clone, Copy)]
struct CancelScenario {
    first_output_tokens: u64,
    first_credits: f64,
    isolated_runtime: bool,
}

fn confirmed_usage(input_tokens: u64, output_tokens: u64, credits: f64) -> Vec<u8> {
    event_stream_frame(
        "messageMetadataEvent",
        json!({"messageMetadataEvent":{"usage":{
            "inputTokens":input_tokens,"outputTokens":output_tokens,"creditsConsumed":credits
        }}}),
    )
}

async fn settled_usage(daemon: &Daemon, expected: usize) -> (Value, Value) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let stats = expect_ok(
            daemon
                .call("stats", json!({"detail":true,"recent":20}))
                .await,
        );
        let keys = expect_ok(daemon.call("apikey.list", json!({"provider":"kiro"})).await);
        let keys = keys.as_array().unwrap();
        assert_eq!(keys.len(), 1, "one fixture API key");
        let usage = keys[0]["usage"].clone();
        if stats["stats"]["recent_requests"].as_array().unwrap().len() >= expected
            && usage["total_requests"].as_u64().unwrap() >= expected as u64
        {
            return (stats, usage);
        }
        assert!(
            Instant::now() < deadline,
            "missing background cancellation settlement: stats={stats}, usage={usage}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn assert_cancelled_once(stats: &Value, usage: &Value, scenario: CancelScenario) -> String {
    let output_tokens = scenario.first_output_tokens + 7;
    let credits = scenario.first_credits + 0.25;
    let rows = stats["stats"]["recent_requests"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "one logical cancellation record: {stats}");
    let row = &rows[0];
    assert_eq!(row["path"], "/v1/messages");
    assert_eq!(
        row["endpoint"],
        if scenario.isolated_runtime {
            "KiroRuntime"
        } else {
            "AmazonQ"
        }
    );
    assert_eq!(row["status"], 499, "client cancellation status: {row}");
    assert!(
        row["error"].is_string(),
        "cancellation must retain a terminal cause: {row}"
    );
    assert_eq!(
        row["diagnostics"]["account_error"], false,
        "client cancellation is not an account failure: {row}"
    );
    assert_eq!(row["diagnostics"]["failure_scope"], "client");
    assert_eq!(row["diagnostics"]["error_code"], "client_cancelled");
    assert_eq!(row["diagnostics"]["error_stage"], "client_disconnect");
    assert_eq!(row["diagnostics"]["credits_source"], "server");
    assert_eq!(
        row["input_tokens"], 111,
        "include confirmed usage of the empty first attempt"
    );
    assert_eq!(row["output_tokens"], output_tokens);
    assert_eq!(row["credits"], credits);
    let total = &stats["stats"]["total"];
    assert_eq!(total["requests"], 1, "{stats}");
    assert_eq!(total["successes"], 0, "{stats}");
    assert_eq!(total["failures"], 1, "{stats}");
    assert_eq!(total["input_tokens"], 111, "{stats}");
    assert_eq!(total["output_tokens"], output_tokens, "{stats}");
    assert_eq!(total["credits"], credits, "{stats}");
    let history = usage["history"].as_array().unwrap();
    assert_eq!(
        history.len(),
        1,
        "one API-key ledger settlement, not one per attempt: {usage}"
    );
    assert_eq!(history[0]["provider_id"], "kiro");
    assert_eq!(history[0]["path"], "/v1/messages");
    assert_eq!(history[0]["token_usage_source"], "server");
    assert_eq!(history[0]["input_tokens"], 111);
    assert_eq!(history[0]["output_tokens"], output_tokens);
    assert_eq!(history[0]["credits"], credits);
    assert_eq!(usage["total_requests"], 1);
    assert_eq!(usage["total_input_tokens"], 111);
    assert_eq!(usage["total_output_tokens"], output_tokens);
    assert_eq!(usage["total_credits"], credits);
    row["request_id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn cancelling_retry_streams_settles_confirmed_usage_once_and_releases_the_only_slot() {
    use axum::body::{Body, Bytes};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use std::convert::Infallible;
    use tokio::sync::Notify;

    struct RetryStreamDrop(Arc<AtomicUsize>);
    impl Drop for RetryStreamDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let _guard = HTTP_TEST_LOCK.lock().await;
    for scenario in [
        // No reported output or credits: the frozen payload may switch to
        // Runtime without replaying any known generated output.
        CancelScenario {
            first_output_tokens: 0,
            first_credits: 0.0,
            isolated_runtime: true,
        },
        // Known output makes Runtime ineligible. The pre-existing AmazonQ
        // retry must still settle both attempts when the client disconnects.
        CancelScenario {
            first_output_tokens: 2,
            first_credits: 0.125,
            isolated_runtime: false,
        },
    ] {
        let cancelled_output = scenario.first_output_tokens + 7;
        let cancelled_credits = scenario.first_credits + 0.25;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = format!("http://{}", listener.local_addr().unwrap());
        let release = Arc::new(Notify::new());
        let generation_calls = Arc::new(AtomicUsize::new(0));
        let retry_dropped = Arc::new(AtomicUsize::new(0));
        let (gate, calls, dropped) = (
            release.clone(),
            generation_calls.clone(),
            retry_dropped.clone(),
        );
        let app = axum::Router::new()
        .route("/ListAvailableModels", get(|| async {axum::Json(catalog())}))
        .route("/generateAssistantResponse", post(move |headers: axum::http::HeaderMap| {
            let (gate, calls, dropped) = (gate.clone(), calls.clone(), dropped.clone());
            async move {
                match calls.fetch_add(1, Ordering::SeqCst) {
                    0 => {
                        assert!(headers.get("x-amz-target").is_none(), "first request must use AmazonQ");
                        let mut body = event_stream_frame("contextUsageEvent", json!({"contextUsagePercentage":0.7}));
                        body.extend(confirmed_usage(11, scenario.first_output_tokens, scenario.first_credits));
                        ([("content-type", "application/vnd.amazon.eventstream")], Body::from(body)).into_response()
                    }
                    1 => {
                        if scenario.isolated_runtime {
                            assert_eq!(headers["x-amz-target"], "KiroRuntimeService.GenerateAssistantResponse");
                        } else {
                            assert!(headers.get("x-amz-target").is_none(), "known output must keep the existing AmazonQ retry");
                        }
                        let output = async_stream::stream! {
                            let _drop_observer = RetryStreamDrop(dropped);
                            // The client sees a following delta only after these
                            // already-confirmed tokens and credits were decoded.
                            yield Ok::<_, Infallible>(Bytes::from(confirmed_usage(100, 7, 0.25)));
                            yield Ok::<_, Infallible>(Bytes::from(event_stream_frame(
                                "assistantResponseEvent", json!({"content":"CANCEL_RETRY_AFTER_THIS_VISIBLE_DELTA "})
                            )));
                            gate.notified().await;
                            yield Ok::<_, Infallible>(Bytes::from(generation_body("RETRY_TAIL_RELEASED")));
                        };
                        ([("content-type", "application/vnd.amazon.eventstream")], Body::from_stream(output)).into_response()
                    }
                    2 => {
                        assert!(headers.get("x-amz-target").is_none(), "cancelled isolated retry polluted endpoint preference");
                        ([("content-type", "application/vnd.amazon.eventstream")], Body::from(generation_body(PROBE_TEXT))).into_response()
                    }
                    other => panic!("unexpected generation after cancellation: {other}"),
                }
            }
        }));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let port = unused_tcp_port();
        let daemon =
            Daemon::start_http(port, &format!("{upstream}/generateAssistantResponse")).await;
        configure_daemon(&daemon, None).await;
        import_profile_account(&daemon).await;
        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("x-api-key", daemon.api_key.as_deref().unwrap())
            .header("user-agent", "claude-cli/2.1.300 (external, e2e)")
            .timeout(Duration::from_secs(8))
            .json(&request(true))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let mut source = response.bytes_stream();
        let mut output = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !String::from_utf8_lossy(&output)
                .contains("CANCEL_RETRY_AFTER_THIS_VISIBLE_DELTA")
            {
                output.extend_from_slice(
                    &source
                        .next()
                        .await
                        .expect("retry delta before cancellation")
                        .unwrap(),
                );
            }
        })
        .await
        .expect("poll and receive actual retry semantic output before cancelling");
        assert_eq!(generation_calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            retry_dropped.load(Ordering::SeqCst),
            0,
            "upstream must still be active before cancellation"
        );
        drop(source);
        tokio::time::timeout(Duration::from_secs(2), async {
            while retry_dropped.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect(
            "client cancellation must drop the active upstream stream before releasing its tail",
        );

        let (cancelled_stats, cancelled_usage) = settled_usage(&daemon, 1).await;
        let cancelled_request_id =
            assert_cancelled_once(&cancelled_stats, &cancelled_usage, scenario);
        assert_eq!(
            generation_calls.load(Ordering::SeqCst),
            2,
            "no generation after cancellation"
        );
        let probe = json!({"model":"source-large","max_tokens":128,"stream":false,
        "messages":[{"role":"user","content":"Check the only stream slot after cancellation."}]});
        let (status, body) = send(&daemon, port, &probe).await;
        assert_eq!(
            status, 200,
            "the only stream slot leaked after cancellation: {body}"
        );
        assert_eq!(text(&body, false), PROBE_TEXT);
        assert_eq!(generation_calls.load(Ordering::SeqCst), 3);
        let (stats, usage) = settled_usage(&daemon, 2).await;
        let rows = stats["stats"]["recent_requests"].as_array().unwrap();
        assert_eq!(
            rows.len(),
            2,
            "no duplicate cancellation finalizer: {stats}"
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row["request_id"] == cancelled_request_id)
                .count(),
            1
        );
        let total = &stats["stats"]["total"];
        assert_eq!(total["requests"], 2);
        assert_eq!(total["successes"], 1);
        assert_eq!(total["failures"], 1);
        assert_eq!(total["input_tokens"], 211);
        assert_eq!(total["output_tokens"], cancelled_output + 20);
        assert_eq!(total["credits"], cancelled_credits + 0.25);
        let history = usage["history"].as_array().unwrap();
        assert_eq!(history.len(), 2, "{usage}");
        assert_eq!(
            history
                .iter()
                .filter(|row| row["input_tokens"] == 111
                    && row["output_tokens"] == cancelled_output
                    && row["credits"] == cancelled_credits)
                .count(),
            1
        );
        assert_eq!(usage["total_requests"], 2);
        assert_eq!(usage["total_input_tokens"], 211);
        assert_eq!(usage["total_output_tokens"], cancelled_output + 20);
        assert_eq!(usage["total_credits"], cancelled_credits + 0.25);
        release.notify_waiters();
        daemon.stop().await;
        task.abort();
    }
}
