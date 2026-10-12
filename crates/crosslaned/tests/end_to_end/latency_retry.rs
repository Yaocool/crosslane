use super::*;
use serde_json::json;

#[tokio::test]
async fn latency_failed_model_fallback_consumes_the_stream_retry_budget() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    let models = ["claude-sonnet-4.8", "claude-sonnet-4.5", "resolved-small", "resolved-tiny", "summary-large"]
        .map(|id| json!({"modelId":id,"tokenLimits":{"maxInputTokens":1_000_000,"maxOutputTokens":16384}}));
    Mock::given(method("GET"))
        .and(path("/ListAvailableModels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":models})))
        .mount(&mock)
        .await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(move |_: &wiremock::Request| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/vnd.amazon.eventstream")
                    .set_body_bytes(event_stream_frame(
                        "ThrottlingException",
                        json!({"__type":"ThrottlingException","message":"too many requests"}),
                    ))
            } else {
                ResponseTemplate::new(500).set_body_string("temporary server failure")
            }
        })
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    let accounts = ["acc_11111111", "acc_22222222"].map(|id| json!({
        "id":id,"email":format!("{id}@example.com"),"machine_id":"1".repeat(64),
        "credentials":{"access_token":"fake-token","region":"us-east-1","expires_at":4_000_000_000i64,"auth_method":"idc"},
        "usage":{"current":0,"limit":100,"percent_used":0,"updated_at":1},"subscription":{"kind":"pro"},"created_at":1
    }));
    expect_ok(
        daemon
            .call("account.import", json!({"accounts":accounts}))
            .await,
    );
    expect_ok(daemon.call("models", json!({})).await);
    model_policy::configure(&daemon, |config| {
        config.upstream.max_retries = 1;
        config.upstream.pool.stream_max_connections = 1;
        config.features.enable_model_fallback = true;
    })
    .await;
    let response = reqwest::Client::new().post(format!("http://127.0.0.1:{port}/v1/messages"))
        .bearer_auth(daemon.api_key.as_deref().unwrap())
        .header("user-agent", "claude-cli/2.1.260 (external, e2e)")
        .json(&json!({"model":"claude-sonnet-4.8","messages":[{"role":"user","content":"Hello"}],"max_tokens":32,"stream":true}))
        .send().await.unwrap();
    let body = response.text().await.unwrap();
    assert!(body.contains("error"), "{body}");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "a failed fallback must consume the only retry: {body}"
    );
    let requests = mock.received_requests().await.unwrap();
    let attempted_models = requests
        .iter()
        .filter(|request| request.method == "POST")
        .map(|request| {
            serde_json::from_slice::<serde_json::Value>(&request.body).unwrap()["conversationState"]
                ["currentMessage"]["userInputMessage"]["modelId"]
                .clone()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        attempted_models,
        [json!("claude-sonnet-4.8"), json!("claude-sonnet-4.5")]
    );
    daemon.stop().await;
}
