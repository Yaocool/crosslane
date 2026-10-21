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
