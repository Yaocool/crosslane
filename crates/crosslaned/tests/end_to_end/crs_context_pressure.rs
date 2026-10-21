use super::*;
use serde_json::{json, Value};

fn catalog(window: u64) -> Value {
    let mut catalog = crs_isolated_runtime::catalog();
    catalog["models"][0]["modelId"] = json!("claude-opus-5.5");
    catalog["models"][0]["tokenLimits"]["maxInputTokens"] = json!(window);
    catalog
}

fn reply(text: &str, percentage: Option<f64>) -> ResponseTemplate {
    let mut body = event_stream_frame("assistantResponseEvent", json!({"content":text}));
    if let Some(percentage) = percentage {
        body.extend(event_stream_frame(
            "contextUsageEvent",
            json!({"contextUsagePercentage":percentage}),
        ));
    }
    body.extend(event_stream_frame(
        "metadataEvent",
        json!({"stopReason":"END_TURN", "inputTokens":17,"outputTokens":2,"creditsConsumed":0.25}),
    ));
    ResponseTemplate::new(200)
        .insert_header("content-type", "application/vnd.amazon.eventstream")
        .set_body_bytes(body)
}

fn body(messages: Value, stream: bool) -> Value {
    json!({"model":"claude-opus-5-5", "max_tokens":128000, "stream":stream,
        "conversation_id":"pressure-owned-session", "system":"Preserve every source fact and tool contract.",
        "tools":[{"name":"checkpoint_reader","description":"Owned reader 中文😀", "input_schema":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}},
            {"name":"deferred_reader","defer_loading":true,"input_schema":{"type":"object","properties":{"key":{"type":"string"}}}}],
        "messages":messages})
}

#[tokio::test]
async fn observed_window_pressure_compacts_before_local_overflow_without_changing_billing_or_caps()
{
    let _guard = HTTP_TEST_LOCK.lock().await;
    for stream in [false, true] {
        let mock = MockServer::start().await;
        let summaries = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&summaries);
        Mock::given(method("POST"))
            .and(path("/generateAssistantResponse"))
            .respond_with(move |request: &wiremock::Request| {
                let wire: Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(
                    request.headers["x-amz-target"],
                    "KiroRuntimeService.GenerateAssistantResponse"
                );
                let current = wire["conversationState"]["currentMessage"]["userInputMessage"]
                    ["content"]
                    .as_str()
                    .unwrap();
                if current.contains("durable conversation checkpoint")
                    || current.contains("consolidating independently produced summaries")
                {
                    seen.fetch_add(1, Ordering::SeqCst);
                    let history = format!("{} {current}", wire["conversationState"]["history"]);
                    let facts = ["EARLY=731", "MIDDLE=482", "LATE=956"]
                        .into_iter()
                        .filter(|fact| history.contains(fact))
                        .collect::<Vec<_>>()
                        .join("; ");
                    reply(
                        &format!(
                            "<summary>Source facts: {facts}; keep the reader contract.</summary>"
                        ),
                        None,
                    )
                } else {
                    assert_eq!(wire["inferenceConfig"]["maxTokens"], 128000);
                    if current.contains("NEXT") {
                        assert!(
                            seen.load(Ordering::SeqCst) > 0,
                            "pressure was ignored until backend overflow"
                        );
                        assert!(wire["conversationState"]["history"]
                            .to_string()
                            .contains("MIDDLE=482"));
                    }
                    reply("ACK", Some(80.0))
                }
            })
            .mount(&mock)
            .await;
        let (daemon, port) =
            crs_isolated_runtime::start_daemon_with_catalog(&mock, None, catalog(20_000)).await;
        model_policy::configure(&daemon, |config| {
            config.context.auto_compact_on_overflow = true;
            config.context.compaction_preserve_recent_turns = 0;
        })
        .await;
        let first = format!(
            "EARLY=731\n{}\nMIDDLE=482\n{}\nLATE=956",
            " x".repeat(3000),
            " x".repeat(3000)
        );
        let (status, response) = crs_isolated_runtime::send(
            &daemon,
            port,
            &body(json!([{"role":"user","content":first}]), stream),
        )
        .await;
        assert_eq!(status, 200, "{response}");
        assert!(!response.contains("\"type\":\"error\""), "{response}");
        let next = format!("NEXT {}", " y".repeat(1500));
        let (status, response) = crs_isolated_runtime::send(
            &daemon,
            port,
            &body(
                json!([
            {"role":"user","content":first},{"role":"assistant","content":"ACK"},
            {"role":"user","content":next}]),
                stream,
            ),
        )
        .await;
        assert_eq!(status, 200, "{response}");
        assert!(response.contains("\"type\":\"compaction\""), "{response}");
        assert!(!response.contains("\"type\":\"error\""), "{response}");
        let stats = expect_ok(
            daemon
                .call("stats", json!({"detail":true,"recent":30}))
                .await,
        );
        let rows = stats["stats"]["recent_requests"].as_array().unwrap();
        let primary: Vec<_> = rows
            .iter()
            .filter(|row| row["path"] == "/v1/messages")
            .collect();
        assert_eq!(primary.len(), 2, "{stats}");
        assert!(
            primary
                .iter()
                .all(|row| row["input_tokens"] == 17 && row["credits"] == 0.25),
            "calibration altered billing: {stats}"
        );
        assert!(
            rows.iter().any(|row| row["path"] == "/internal/compact"),
            "{stats}"
        );
        daemon.stop().await;
    }
}

#[tokio::test]
async fn internal_summary_recovers_transient_stream_errors_but_never_native_refusals() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    for failure in ["empty", "generic", "capacity", "http500", "refusal"] {
        let mock = MockServer::start().await;
        let attempts = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&attempts);
        Mock::given(method("POST")).and(path("/generateAssistantResponse"))
            .respond_with(move |request:&wiremock::Request| {
                let wire:Value=serde_json::from_slice(&request.body).unwrap();
                assert_eq!(request.headers["x-amz-target"],"KiroRuntimeService.GenerateAssistantResponse");
                let current=wire["conversationState"]["currentMessage"]["userInputMessage"]["content"].as_str().unwrap();
                if (current.contains("durable conversation checkpoint") || current.contains("consolidating independently produced summaries")) && seen.fetch_add(1,Ordering::SeqCst)==0 {
                    if failure=="http500" { return ResponseTemplate::new(500).set_body_json(json!({"message":"Encountered an unexpected error when processing the request, please try again.","reason":null})); }
                    let mut bytes=event_stream_frame("metadataEvent",json!({"inputTokens":17,"outputTokens":0,"creditsConsumed":0.125}));
                    if failure=="generic" {bytes.extend(event_stream_frame("error",json!({"message":"Encountered an unexpected error when processing the request, please try again.","reason":null})));}
                    if failure=="capacity" {bytes.extend(event_stream_frame("serviceUnavailableError",json!({"message":"The service is temporarily unavailable. Please try again later.","reason":"MODEL_TEMPORARILY_UNAVAILABLE"})));}
                    if failure=="refusal" {bytes.extend(event_stream_frame("metadataEvent",json!({"stopReason":"CONTENT_FILTERED","stopDetails":{"refusal":{"category":"REASONING_EXTRACTION"}}})));}
                    return ResponseTemplate::new(200).insert_header("content-type","application/vnd.amazon.eventstream").set_body_bytes(bytes);
                }
                if current.contains("durable conversation checkpoint") || current.contains("consolidating independently produced summaries") {
                    let source=format!("{} {current}",wire["conversationState"]["history"]);
                    reply(if source.contains("PRESERVE_MIDDLE=482") {
                        "<summary>PRESERVE_MIDDLE=482; original source read completely.</summary>"
                    } else { "<summary>Other complete source records; no changed decision.</summary>" },None)
                } else {
                    assert_eq!(wire["inferenceConfig"]["maxTokens"],128000);
                    assert!(wire["conversationState"]["history"].to_string().contains("PRESERVE_MIDDLE=482"));
                    reply("DONE",None)
                }
            }).mount(&mock).await;
        let (daemon, port) =
            crs_isolated_runtime::start_daemon_with_catalog(&mock, None, catalog(20_000)).await;
        model_policy::configure(&daemon, |config| {
            config.context.auto_compact_on_overflow = true;
            config.context.compaction_preserve_recent_turns = 0;
        })
        .await;
        let source = format!(
            "{} PRESERVE_MIDDLE=482 {}",
            " x".repeat(11_000),
            " y".repeat(11_000)
        );
        let (status, response) = crs_isolated_runtime::send(
            &daemon,
            port,
            &body(
                json!([
            {"role":"user","content":source},{"role":"assistant","content":"ACK"},
            {"role":"user","content":"Continue; do not lose source facts."}]),
                true,
            ),
        )
        .await;
        assert_eq!(status, 200, "{response}");
        if failure == "refusal" {
            assert!(response.contains("compaction_failed"), "{response}");
            assert!(response.contains("reasoning_extraction"), "{response}");
            assert_eq!(attempts.load(Ordering::SeqCst), 1, "refusal was retried");
        } else {
            assert!(response.contains("DONE"), "{failure}: {response}");
            assert!(
                !response.contains("compaction_failed"),
                "{failure}: {response}"
            );
            assert!(attempts.load(Ordering::SeqCst) >= 2);
        }
        let stats = expect_ok(
            daemon
                .call("stats", json!({"detail":true,"recent":30}))
                .await,
        );
        let rows = stats["stats"]["recent_requests"].as_array().unwrap();
        if failure == "refusal" {
            assert!(
                rows.iter().any(|row| row["path"] == "/internal/compact"
                    && row["diagnostics"]["error_code"] == "upstream_content_filtered"
                    && row["diagnostics"]["error_stage"] == "upstream_generation"),
                "{stats}"
            );
        }
        if failure != "http500" {
            assert!(
                rows.iter()
                    .any(|row| row["path"] == "/internal/compact" && row["credits"] == 0.125),
                "failed summary charge lost: {stats}"
            );
        }
        daemon.stop().await;
    }
}

#[tokio::test]
async fn concurrent_summary_refusal_is_not_hidden_by_an_earlier_stream_failure() {
    concurrent_summary_refusal_case(false).await;
}

#[tokio::test]
async fn observed_refusal_survives_timeout_while_accepted_sibling_settles_usage() {
    concurrent_summary_refusal_case(true).await;
}

async fn concurrent_summary_refusal_case(late_sibling: bool) {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&attempts);
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(move |_: &wiremock::Request| {
            let index = seen.fetch_add(1, Ordering::SeqCst);
            if late_sibling && index == 1 {
                return reply("<summary>Complete owned source checkpoint.</summary>", None)
                    .set_delay(Duration::from_millis(11_000));
            }
            let bytes = if index == 0 && !late_sibling {
                event_stream_frame(
                    "validationError",
                    json!({"message":"First source part failed validation."}),
                )
            } else {
                event_stream_frame(
                    "metadataEvent",
                    json!({"stopReason":"CONTENT_FILTERED", "stopDetails":{"refusal":{"category":"CYBER"}}}),
                )
            };
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(if index == 0 { 100 } else { 200 }))
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(bytes)
        })
        .mount(&mock)
        .await;
    let (daemon, port) =
        crs_isolated_runtime::start_daemon_with_catalog(&mock, None, catalog(20_000)).await;
    model_policy::configure(&daemon, |config| {
        config.pool.max_concurrent_per_account = 2;
        config.upstream.pool.stream_max_connections = 2;
        config.context.auto_compact_on_overflow = true;
        config.context.compaction_preserve_recent_turns = 0;
        if late_sibling {
            // Leave planning headroom on loaded test hosts, while ensuring the
            // accepted response arrives after the foreground operation budget.
            config.context.compaction_summary_timeout_ms = 10_000;
        }
    })
    .await;
    let source = " x".repeat(22_000);
    let request = body(
        json!([
            {"role":"user", "content":source},
            {"role":"assistant", "content":"ACK"},
            {"role":"user", "content":"Continue from the complete source."}
        ]),
        true,
    );
    let response = if late_sibling {
        reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("x-api-key", daemon.api_key.as_deref().unwrap())
            .header("user-agent", "claude-cli/2.1.300 (external, e2e)")
            .timeout(Duration::from_secs(20))
            .json(&request)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    } else {
        crs_isolated_runtime::send(&daemon, port, &request).await.1
    };
    assert!(response.contains("compaction_failed"), "{response}");
    assert!(response.contains("cyber"), "{response}");
    assert!(!response.contains("timed out"), "{response}");
    assert!(
        !response.contains("First source part failed validation"),
        "{response}"
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 2, "refusal was retried");
    if late_sibling {
        tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                let stats = expect_ok(
                    daemon
                        .call("stats", json!({"detail":true,"recent":30}))
                        .await,
                );
                if stats["stats"]["recent_requests"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|row| {
                        row["path"] == "/internal/compact"
                            && row["status"] == 200
                            && row["credits"] == 0.25
                    })
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("accepted sibling was canceled or its reported charge was lost");
    }
    daemon.stop().await;
}

#[tokio::test]
async fn terminal_peer_stops_new_recovery_children_but_keeps_accepted_attempt_charges() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&attempts);
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(move |_: &wiremock::Request| {
            let index = seen.fetch_add(1, Ordering::SeqCst);
            if index == 0 {
                let mut bytes = event_stream_frame("metadataEvent", json!({"inputTokens":17,"creditsConsumed":0.125}));
                bytes.extend(event_stream_frame("serviceUnavailableError", json!({"message":"The service is temporarily unavailable. Please try again later.","reason":"MODEL_TEMPORARILY_UNAVAILABLE"})));
                return ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(100))
                    .insert_header("content-type", "application/vnd.amazon.eventstream")
                    .set_body_bytes(bytes);
            }
            if index == 1 {
                return ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(300))
                    .insert_header("content-type", "application/vnd.amazon.eventstream")
                    .set_body_bytes(event_stream_frame("metadataEvent", json!({"stopReason":"CONTENT_FILTERED","stopDetails":{"refusal":{"category":"CYBER"}}})));
            }
            reply("<summary>Complete recovery source half.</summary>", None)
                .set_delay(Duration::from_millis(2000))
        })
        .mount(&mock)
        .await;
    let (daemon, port) =
        crs_isolated_runtime::start_daemon_with_catalog(&mock, None, catalog(20_000)).await;
    model_policy::configure(&daemon, |config| {
        config.pool.max_concurrent_per_account = 2;
        config.upstream.pool.stream_max_connections = 2;
        config.context.auto_compact_on_overflow = true;
        config.context.compaction_preserve_recent_turns = 0;
    })
    .await;
    let (_, response) = crs_isolated_runtime::send(
        &daemon,
        port,
        &body(
            json!([
                {"role":"user","content":" x".repeat(28_000)},
                {"role":"assistant","content":"ACK"},
                {"role":"user","content":"Continue from all original records."}
            ]),
            true,
        ),
    )
    .await;
    assert!(response.contains("cyber"), "{response}");
    let calls = attempts.load(Ordering::SeqCst);
    assert!(
        (2..=3).contains(&calls),
        "new recovery child dispatched after terminal peer failure: {calls}"
    );
    let stats = expect_ok(
        daemon
            .call("stats", json!({"detail":true,"recent":30}))
            .await,
    );
    let rows = stats["stats"]["recent_requests"].as_array().unwrap();
    let accepted_charges = if calls == 3 {
        vec![0.125, 0.25]
    } else {
        vec![0.125]
    };
    for charge in &accepted_charges {
        assert!(
            rows.iter()
                .any(|row| row["path"] == "/internal/compact" && row["credits"] == *charge),
            "accepted charge lost: {stats}"
        );
    }
    let total: f64 = rows
        .iter()
        .filter(|row| row["path"] == "/internal/compact")
        .map(|row| row["credits"].as_f64().unwrap())
        .sum();
    assert_eq!(
        total,
        accepted_charges.iter().sum::<f64>(),
        "accepted charges duplicated or lost: {stats}"
    );
    daemon.stop().await;
}

#[tokio::test]
async fn complete_summary_can_use_safe_window_without_clipping_required_current_context() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    let summaries = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&summaries);
    let checkpoint = format!(
        "PRESERVE_MIDDLE=482 {} END_CHECKPOINT",
        " fact".repeat(1100)
    );
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(move |request: &wiremock::Request| {
            let wire: Value = serde_json::from_slice(&request.body).unwrap();
            let current = wire["conversationState"]["currentMessage"]["userInputMessage"]
                ["content"]
                .as_str()
                .unwrap();
            if current.contains("durable conversation checkpoint")
                || current.contains("consolidating independently produced summaries")
            {
                seen.fetch_add(1, Ordering::SeqCst);
                reply(&format!("<summary>{checkpoint}</summary>"), None)
            } else {
                assert_eq!(
                    current,
                    format!("CURRENT_START {} CURRENT_END", " z".repeat(13_500))
                );
                assert_eq!(wire["inferenceConfig"]["maxTokens"], 128000);
                let history = wire["conversationState"]["history"].to_string();
                assert!(
                    history.contains("PRESERVE_MIDDLE=482") && history.contains("END_CHECKPOINT")
                );
                reply("COMPLETE", None)
            }
        })
        .mount(&mock)
        .await;
    let (daemon, port) =
        crs_isolated_runtime::start_daemon_with_catalog(&mock, None, catalog(20_000)).await;
    model_policy::configure(&daemon, |config| {
        config.context.auto_compact_on_overflow = true;
    })
    .await;
    let (status, response) = crs_isolated_runtime::send(&daemon, port, &body(json!([
        {"role":"user","content":format!("PRESERVE_MIDDLE=482 {}", " x".repeat(9000))},
        {"role":"assistant","content":"ACK"},
        {"role":"user","content":format!("CURRENT_START {} CURRENT_END", " z".repeat(13_500))}
    ]), true)).await;
    assert_eq!(status, 200, "{response}");
    assert!(
        response.contains("COMPLETE") && !response.contains("compaction_failed"),
        "{response}"
    );
    assert_eq!(
        summaries.load(Ordering::SeqCst),
        3,
        "expected two source parts and one merge; valid checkpoint was regenerated"
    );
    daemon.stop().await;
}
