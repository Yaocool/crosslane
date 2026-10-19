#![cfg(feature = "crs")]

use super::crs_isolated_runtime::{catalog, configure_daemon, request};
use super::*;
use serde_json::json;
use std::sync::Mutex;

#[tokio::test]
async fn cancelling_before_candidate_post_keeps_confirmed_usage_with_original_account() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    use axum::body::{Body, Bytes};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use std::convert::Infallible;
    use tokio::sync::Notify;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let release_error = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let first_account = Arc::new(Mutex::new(String::new()));
    let (observed_calls, observed_account, gate) =
        (calls.clone(), first_account.clone(), release_error.clone());
    let app = axum::Router::new()
        .route("/ListAvailableModels", get(|| async { axum::Json(catalog()) }))
        .route("/generateAssistantResponse", post(move |headers: axum::http::HeaderMap| {
            let (calls, first_account, gate) =
                (observed_calls.clone(), observed_account.clone(), gate.clone());
            async move {
                let attempt = calls.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    let auth = headers["authorization"].to_str().unwrap();
                    let id = if auth.ends_with("owned-switch-account-a") {
                        "acc_11111111"
                    } else {
                        assert!(auth.ends_with("owned-switch-account-b"));
                        "acc_22222222"
                    };
                    *first_account.lock().unwrap() = id.into();
                    let output = async_stream::stream! {
                        yield Ok::<_, Infallible>(Bytes::from(event_stream_frame(
                            "messageMetadataEvent", json!({"messageMetadataEvent":{"usage":{
                                "inputTokens":41,"outputTokens":3,"creditsConsumed":0.125
                            }}}),
                        )));
                        gate.notified().await;
                        yield Ok::<_, Infallible>(Bytes::from(event_stream_frame(
                            "serviceUnavailableError", json!({"message":"Owned first account event failed."}),
                        )));
                    };
                    ([("content-type", "application/vnd.amazon.eventstream")], Body::from_stream(output)).into_response()
                } else {
                    ([("content-type", "application/vnd.amazon.eventstream")],
                        Body::from(generation_body("UNEXPECTED_CANDIDATE_POST"))).into_response()
                }
            }
        }));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let port = unused_tcp_port();
    let daemon = Daemon::start_http(port, &format!("{upstream}/generateAssistantResponse")).await;
    configure_daemon(&daemon, Some(crosslane_core::config::Endpoint::Amazonq)).await;
    let accounts = [
        ("acc_11111111", "owned-switch-account-a"),
        ("acc_22222222", "owned-switch-account-b"),
    ]
    .map(|(id, token)| {
        json!({
            "id":id,"email":format!("{id}@example.com"),"machine_id":"1".repeat(64),
            "profile_arn":"arn:aws:codewhisperer:us-east-1:123456789012:profile/switch-cancel",
            "credentials":{"access_token":token,"region":"us-east-1",
                "expires_at":4_000_000_000i64,"auth_method":"idc"},
            "usage":{"current":0,"limit":100,"percent_used":0,"updated_at":1},
            "subscription":{"kind":"pro"},"created_at":1
        })
    });
    expect_ok(
        daemon
            .call("account.import", json!({"accounts":accounts}))
            .await,
    );
    let models = expect_ok(daemon.call("models", json!({})).await);
    assert_eq!(models.as_array().map(Vec::len), Some(5), "{models}");
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
    let source = response.bytes_stream();
    // The established client response precedes the upstream exception. Its
    // gated release makes the candidate's 200ms backoff observable via IPC.
    release_error.notify_one();
    let deadline = Instant::now() + Duration::from_secs(2);
    let (original, candidate) = loop {
        let original = first_account.lock().unwrap().clone();
        if !original.is_empty() {
            let candidate = if original == "acc_11111111" {
                "acc_22222222"
            } else {
                "acc_11111111"
            };
            let detail = expect_ok(daemon.call("account.show", json!({"id":candidate})).await);
            if detail["active_requests"] == 1 {
                // The candidate lease is acquired, but the existing backoff
                // has not sent its POST. Cancel exactly in this window.
                assert_eq!(calls.load(Ordering::SeqCst), 1);
                break (original, candidate);
            }
        }
        if Instant::now() >= deadline {
            let stats = expect_ok(
                daemon
                    .call("stats", json!({"detail":true,"recent":20}))
                    .await,
            );
            let accounts = expect_ok(daemon.call("account.list", json!({})).await);
            panic!("candidate lease was never acquired; calls={}, original={original}, stats={stats}, accounts={accounts}", calls.load(Ordering::SeqCst));
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    };
    drop(source);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let stats = expect_ok(
            daemon
                .call("stats", json!({"detail":true,"recent":20}))
                .await,
        );
        let rows = stats["stats"]["recent_requests"].as_array().unwrap();
        if let Some(row) = rows.first() {
            assert_eq!(rows.len(), 1, "{stats}");
            assert_eq!(row["status"], 499, "{row}");
            assert_eq!(
                row["account_id"], original,
                "old usage changed account: {row}"
            );
            assert_eq!(row["kiro_model"], "source-large");
            assert_eq!(row["input_tokens"], 41);
            assert_eq!(row["output_tokens"], 3);
            assert_eq!(row["credits"], 0.125);
            assert_eq!(stats["stats"]["total"]["credits"], 0.125);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cancellation did not settle: {stats}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "candidate must not post after cancellation"
    );
    let keys = expect_ok(daemon.call("apikey.list", json!({"provider":"kiro"})).await);
    assert_eq!(keys[0]["usage"]["total_requests"], 1);
    assert_eq!(keys[0]["usage"]["total_credits"], 0.125);
    assert_eq!(keys[0]["usage"]["history"].as_array().unwrap().len(), 1);
    for id in [original.as_str(), candidate] {
        let detail = expect_ok(daemon.call("account.show", json!({"id":id})).await);
        assert_eq!(detail["active_requests"], 0, "lease leaked: {detail}");
    }
    daemon.stop().await;
    server.abort();
}
