use super::model_policy::configure;
use super::*;
use serde_json::{json, Value};

const COMPACT_PROMPT: &str = "Your task is to create a detailed summary of the conversation so far, preserving requirements and exact identifiers.\nReturn the summary in <summary> tags.";
const GENERIC_ERROR: &str =
    "Encountered an unexpected error when processing the request, please try again.";
const CLIENT_SUMMARY: &str = "<summary>CLIENT_SUMMARY_RECOVERED: cobalt-731 雪✅.</summary>";
const CLAUDE_AGENT: &str = "claude-cli/2.1.296 (external, sdk-cli)";

fn summary_text(body: &str, stream: bool) -> String {
    if stream {
        body.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str::<Value>(data).ok())
            .filter_map(|event| event["delta"]["text"].as_str().map(str::to_owned))
            .collect()
    } else {
        serde_json::from_str::<Value>(body).unwrap()["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect()
    }
}

fn successful_summary() -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "application/vnd.amazon.eventstream")
        .set_body_bytes(generation_body(CLIENT_SUMMARY))
}

fn summary_models() -> Vec<Value> {
    [
        "source-large",
        "mapped-small",
        "resolved-small",
        "resolved-tiny",
        "summary-large",
    ]
    .map(|model| {
        json!({"modelId":model,
            "tokenLimits":{"maxInputTokens":1_000_000,"maxOutputTokens":128_000}})
    })
    .into()
}

async fn configure_summary_daemon(daemon: &Daemon, max_retries: u32) {
    configure(daemon, |config| {
        config.server.enforce_user_agent_check = false;
        config.upstream.preferred_endpoint = Some(crosslane_core::config::Endpoint::Amazonq);
        config.upstream.max_retries = max_retries;
        config.upstream.stream_read_timeout_ms = 1_000;
        config.upstream.pool.stream_max_connections = 1;
        config.features.default_model_id = "source-large".into();
        config.features.enable_model_fallback = false;
        config.features.enable_prompt_cache = false;
        config.features.auto_continue_rounds = 0;
        config.context.auto_compact_on_overflow = false;
        config.pool.max_concurrent_per_account = 1;
        config.pool.cooldown.error_cooldown_ms = 10;
        config.pool.cooldown.cooldown_ms = 10;
        config.pool.cooldown.max_error_count = 100;
    })
    .await;
    import_context_alignment_account(daemon, 0.0).await;
}

async fn start_summary_daemon(mock: &MockServer, max_retries: u32) -> (Daemon, u16) {
    Mock::given(method("GET"))
        .and(path("/ListAvailableModels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models":summary_models()})))
        .mount(mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    configure_summary_daemon(&daemon, max_retries).await;
    (daemon, port)
}

fn summary_request(stream: bool, tools_disabled: bool) -> Value {
    let mut request = json!({
        "model":"source-large", "max_tokens":128_000, "stream":stream,
        "conversation_id":"client-summary-stable-id",
        "system":"Preserve policy NO_UPLOAD, exact identifiers and Unicode 雪✅.",
        "tools":[{"name":"Read", "description":"schema-keep-731: Read approved records only.",
            "input_schema":{"type":"object","properties":{"path":{"type":"string"}},
                "required":["path"],"additionalProperties":false}}],
        "messages":[
            {"role":"user","content":"Read the approved records; preserve cobalt-731."},
            {"role":"assistant","content":[{"type":"tool_use","id":"read-history-731",
                "name":"Read","input":{"path":"owned/雪✅.txt"}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"read-history-731",
                "content":"EARLY=cobalt-731\nUnicode=雪✅\nLATE=spruce-956"},
                {"type":"text","text":COMPACT_PROMPT}]}
        ]
    });
    if tools_disabled {
        request["tool_choice"] = json!({"type":"none"});
    }
    request
}

fn minimal_summary(stream: bool) -> Value {
    json!({"model":"source-large","max_tokens":128_000,"stream":stream,
        "messages":[{"role":"user","content":COMPACT_PROMPT}]})
}

async fn send_request(
    daemon: &Daemon,
    port: u16,
    request: &Value,
    user_agent: &str,
) -> (reqwest::StatusCode, String) {
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .timeout(Duration::from_secs(8))
        .header("x-api-key", daemon.api_key.as_deref().unwrap())
        .header("user-agent", user_agent)
        .json(request)
        .send()
        .await
        .unwrap();
    let status = response.status();
    (status, response.text().await.unwrap())
}

async fn generation_requests(mock: &MockServer) -> Vec<wiremock::Request> {
    mock.received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| {
            request.method == "POST" && request.url.path() == "/generateAssistantResponse"
        })
        .collect()
}

async fn assert_single_success_accounted(daemon: &Daemon) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let stats = expect_ok(
            daemon
                .call("stats", json!({"detail":true,"recent":20}))
                .await,
        );
        let logs = stats["stats"]["recent_requests"].as_array().unwrap();
        if !logs.is_empty() {
            assert_eq!(logs.len(), 1, "one logical client request: {stats}");
            assert_eq!(logs[0]["path"], "/v1/messages");
            assert_eq!(logs[0]["status"], 200);
            assert_eq!(logs[0]["input_tokens"], 100);
            assert_eq!(logs[0]["output_tokens"], 20);
            assert_eq!(logs[0]["credits"], 0.25);
            assert_eq!(stats["stats"]["total"]["requests"], 1);
            assert_eq!(stats["stats"]["total"]["successes"], 1);
            assert_eq!(stats["stats"]["total"]["failures"], 0);
            assert_eq!(stats["stats"]["total"]["credits"], 0.25);
            return;
        }
        assert!(
            Instant::now() < deadline,
            "missing final accounting: {stats}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn client_summary_generic_500_recovery_preserves_exact_payload_and_single_accounting() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    for stream in [false, true] {
        for tools_disabled in [false, true] {
            let mock = MockServer::start().await;
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&calls);
            Mock::given(method("POST"))
                .and(path("/generateAssistantResponse"))
                .respond_with(move |_: &wiremock::Request| {
                    if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                        if tools_disabled {
                            ResponseTemplate::new(500).set_body_string(GENERIC_ERROR)
                        } else {
                            ResponseTemplate::new(500).set_body_json(json!({
                                "type":"InternalServerException", "message":GENERIC_ERROR
                            }))
                        }
                    } else {
                        successful_summary()
                    }
                })
                .mount(&mock)
                .await;
            let (daemon, port) = start_summary_daemon(&mock, 1).await;
            let original = summary_request(stream, tools_disabled);
            let (status, body) = send_request(&daemon, port, &original, CLAUDE_AGENT).await;
            assert_eq!(
                status, 200,
                "stream={stream}, none={tools_disabled}: {body}"
            );
            assert_eq!(summary_text(&body, stream), CLIENT_SUMMARY, "{body}");
            assert!(!body.contains("\"type\":\"error\""), "{body}");
            let requests = generation_requests(&mock).await;
            assert_eq!(requests.len(), 2, "{body}");
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            assert_eq!(
                requests[0].body, requests[1].body,
                "retry changed wire input"
            );
            let payload: Value = serde_json::from_slice(&requests[0].body).unwrap();
            assert_eq!(payload["inferenceConfig"]["maxTokens"], 128_000);
            let state = &payload["conversationState"];
            assert!(state["conversationId"]
                .as_str()
                .is_some_and(|id| !id.is_empty()));
            assert!(state["agentContinuationId"]
                .as_str()
                .is_some_and(|id| !id.is_empty()));
            let current = &state["currentMessage"]["userInputMessage"];
            assert_eq!(current["modelId"], "source-large");
            assert_eq!(current["content"], COMPACT_PROMPT);
            assert!(current["userInputMessageContext"]["tools"]
                .as_array()
                .is_none_or(Vec::is_empty));
            assert!(current["userInputMessageContext"]["toolResults"]
                .as_array()
                .is_none_or(Vec::is_empty));
            let history = state["history"].to_string();
            for expected in [
                "NO_UPLOAD",
                "read-history-731",
                "owned/雪✅.txt",
                "EARLY=cobalt-731",
                "LATE=spruce-956",
            ] {
                assert!(
                    history.contains(expected),
                    "source lost {expected}: {history}"
                );
            }
            // Existing summary projection retains loaded definitions as source;
            // `none` keeps historical records without restoring callable tools.
            {
                let definition = state["history"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|message| message["userInputMessage"]["content"].as_str())
                    .find_map(|text| {
                        text.split_once("Original tool definition (complete JSON source):\n")
                            .and_then(|(_, source)| source.lines().next())
                    })
                    .map(|source| serde_json::from_str::<Value>(source).unwrap())
                    .expect("complete source tool definition");
                assert_eq!(
                    definition["input_schema"],
                    original["tools"][0]["input_schema"]
                );
                assert_eq!(
                    definition["description"],
                    original["tools"][0]["description"]
                );
            }
            let translated = serde_json::from_value(payload).unwrap();
            crosslane_translate::validate_kiro_tool_history(&translated).unwrap();
            assert_single_success_accounted(&daemon).await;
            daemon.stop().await;
        }
    }
}

#[tokio::test]
async fn client_summary_generic_recovery_is_bounded_and_respects_disabled_retries() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    for stream in [false, true] {
        for (max_retries, expected_calls) in [(1, 2), (0, 1)] {
            let mock = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/generateAssistantResponse"))
                .respond_with(
                    ResponseTemplate::new(500).set_body_json(json!({"message":GENERIC_ERROR})),
                )
                .mount(&mock)
                .await;
            let (daemon, port) = start_summary_daemon(&mock, max_retries).await;
            let (status, body) =
                send_request(&daemon, port, &minimal_summary(stream), CLAUDE_AGENT).await;
            assert!(
                if stream {
                    status == 200
                } else {
                    status.is_server_error()
                },
                "{body}"
            );
            assert!(body.contains("error"), "{body}");
            assert!(!body.contains(CLIENT_SUMMARY), "{body}");
            assert_eq!(
                generation_requests(&mock).await.len(),
                expected_calls,
                "max_retries={max_retries}, stream={stream}: {body}"
            );
            daemon.stop().await;
        }
    }
}

#[tokio::test]
async fn client_summary_recovery_does_not_apply_to_ordinary_or_unrecognized_clients() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    for ordinary in [false, true] {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/generateAssistantResponse"))
            .respond_with(ResponseTemplate::new(500).set_body_string(GENERIC_ERROR))
            .mount(&mock)
            .await;
        let (daemon, port) = start_summary_daemon(&mock, 1).await;
        let mut request = minimal_summary(false);
        if ordinary {
            request["messages"][0]["content"] = json!("Continue the ordinary conversation.");
        }
        let (status, body) = send_request(
            &daemon,
            port,
            &request,
            if ordinary {
                CLAUDE_AGENT
            } else {
                "integration-client/1.0"
            },
        )
        .await;
        assert!(status.is_server_error(), "{body}");
        assert_eq!(
            generation_requests(&mock).await.len(),
            1,
            "ordinary={ordinary}: {body}"
        );
        daemon.stop().await;
    }
}

#[tokio::test]
async fn client_summary_specialized_500_rejections_do_not_gain_generic_recovery() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    for (kind, rejection) in [
        (
            "authentication",
            json!({"type":"UnauthorizedException","message":GENERIC_ERROR}),
        ),
        (
            "request",
            json!({"type":"ValidationException","message":GENERIC_ERROR}),
        ),
        (
            "context",
            json!({"message":GENERIC_ERROR,"reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD"}),
        ),
        (
            "capacity",
            json!({"message":GENERIC_ERROR,"reason":"MODEL_TEMPORARILY_UNAVAILABLE"}),
        ),
        (
            "content_filter",
            json!({"message":GENERIC_ERROR,"reason":"CONTENT_FILTERED"}),
        ),
        (
            "unknown_type",
            json!({"type":"UnknownProviderFailure","message":GENERIC_ERROR}),
        ),
    ] {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/generateAssistantResponse"))
            .respond_with(ResponseTemplate::new(500).set_body_json(rejection))
            .mount(&mock)
            .await;
        let (daemon, port) = start_summary_daemon(&mock, 1).await;
        // No compactable history: existing upstream-context handling fails its
        // minimum-context guard without launching an internal summary request.
        let (_, body) = send_request(&daemon, port, &minimal_summary(false), CLAUDE_AGENT).await;
        assert!(body.contains("error"), "{kind}: {body}");
        assert_eq!(generation_requests(&mock).await.len(), 1, "{kind}: {body}");
        daemon.stop().await;
    }
}

#[tokio::test]
async fn client_summary_first_attempt_success_does_not_retry() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    for stream in [false, true] {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/generateAssistantResponse"))
            .respond_with(|_: &wiremock::Request| successful_summary())
            .mount(&mock)
            .await;
        let (daemon, port) = start_summary_daemon(&mock, 1).await;
        let (status, body) =
            send_request(&daemon, port, &minimal_summary(stream), CLAUDE_AGENT).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(summary_text(&body, stream), CLIENT_SUMMARY, "{body}");
        assert_eq!(generation_requests(&mock).await.len(), 1);
        assert_single_success_accounted(&daemon).await;
        daemon.stop().await;
    }
}

async fn recent_client_logs(daemon: &Daemon, expected: usize) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let stats = expect_ok(
            daemon
                .call("stats", json!({"detail":true,"recent":20}))
                .await,
        );
        let logs = stats["stats"]["recent_requests"].as_array().unwrap();
        if logs.len() >= expected {
            return logs.clone();
        }
        assert!(
            Instant::now() < deadline,
            "missing final accounting: {stats}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn alignment_account(accounts: &Value) -> &Value {
    accounts["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|account| account["id"] == "acc_99999999")
        .expect("fixture account")
}

#[tokio::test]
async fn client_summary_recovery_auth_failure_bans_unrefreshable_account_without_third_generation()
{
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(move |_: &wiremock::Request| {
            if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(500).set_body_json(json!({"message":GENERIC_ERROR}))
            } else {
                // HTTP 500 avoids the transport's existing 401/403 endpoint
                // cycling while still exercising the authentication classifier.
                ResponseTemplate::new(500).set_body_json(json!({
                    "type":"UnauthorizedException","message":"invalid token"
                }))
            }
        })
        .mount(&mock)
        .await;
    let (daemon, port) = start_summary_daemon(&mock, 1).await;
    let (_, body) = send_request(&daemon, port, &minimal_summary(false), CLAUDE_AGENT).await;
    assert!(body.contains("error"), "{body}");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "{body}");
    assert_eq!(generation_requests(&mock).await.len(), 2);
    let accounts = expect_ok(daemon.call("account.list", json!({})).await);
    assert_eq!(
        alignment_account(&accounts)["health"],
        "banned",
        "{accounts}"
    );
    let logs = recent_client_logs(&daemon, 1).await;
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0]["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(logs[0]["credits"], 0.0);
    daemon.stop().await;
}

/// A silent header delay would hit the existing per-read timeout. This server
/// instead trickles one HTTP 500 body, keeping each read below that timeout
/// while the original generation lasts longer than the recovery window.
struct TrickleSummaryUpstream {
    url: String,
    calls: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl TrickleSummaryUpstream {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let app = axum::Router::new()
            .route("/ListAvailableModels", axum::routing::get(|| async {
                axum::Json(json!({"models":summary_models()}))
            }))
            .route("/generateAssistantResponse", axum::routing::post(move || {
                let observed = Arc::clone(&observed);
                async move {
                    observed.fetch_add(1, Ordering::SeqCst);
                    let chunks = GENERIC_ERROR.as_bytes().chunks(16).map(Vec::from).collect::<Vec<_>>();
                    let body = async_stream::stream! {
                        for chunk in chunks {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            yield Ok::<bytes::Bytes, std::convert::Infallible>(bytes::Bytes::from(chunk));
                        }
                    };
                    axum::response::Response::builder()
                        .status(500)
                        .header("content-type", "text/plain")
                        .body(axum::body::Body::from_stream(body))
                        .unwrap()
                }
            }));
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            url: format!("http://{address}/generateAssistantResponse"),
            calls,
            task,
        }
    }
}

impl Drop for TrickleSummaryUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn client_summary_recovery_deadline_releases_permits_and_does_not_bound_original_attempt() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    {
        let mock = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        Mock::given(method("POST"))
            .and(path("/generateAssistantResponse"))
            .respond_with(move |_: &wiremock::Request| {
                match observed.fetch_add(1, Ordering::SeqCst) {
                    0 => ResponseTemplate::new(500).set_body_json(json!({"message":GENERIC_ERROR})),
                    1 => successful_summary().set_delay(Duration::from_secs(1)),
                    _ => successful_summary(),
                }
            })
            .mount(&mock)
            .await;
        let (daemon, port) = start_summary_daemon(&mock, 1).await;
        configure(&daemon, |config| {
            config.upstream.stream_read_timeout_ms = 400
        })
        .await;
        let (status, body) =
            send_request(&daemon, port, &minimal_summary(false), CLAUDE_AGENT).await;
        assert!(status.is_server_error(), "{body}");
        assert!(body.contains(GENERIC_ERROR), "{body}");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "{body}");
        let requests = generation_requests(&mock).await;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].body, requests[1].body);
        let logs = recent_client_logs(&daemon, 1).await;
        assert_eq!(logs.len(), 1);
        let attempts = logs[0]["attempts"].as_array().unwrap();
        assert_eq!(attempts.len(), 2, "{logs:?}");
        assert_eq!(attempts[0]["status"], 500);
        assert_eq!(attempts[1]["endpoint"], "client-summary-recovery");
        assert!(attempts[1]["status"].is_null());
        assert!(attempts[1]["error"]
            .as_str()
            .unwrap()
            .contains("deadline elapsed"));
        assert_eq!(logs[0]["credits"], 0.0);

        // Both the account and transport have one slot. Success here proves
        // cancellation released the timed-out recovery's two permits.
        let ordinary = json!({"model":"source-large","max_tokens":64,
            "messages":[{"role":"user","content":"Verify permits were released."}]});
        let (status, body) = send_request(&daemon, port, &ordinary, CLAUDE_AGENT).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(summary_text(&body, false), CLIENT_SUMMARY);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        let logs = recent_client_logs(&daemon, 2).await;
        assert_eq!(logs.len(), 2);
        assert_eq!(logs.iter().filter(|log| log["status"] == 200).count(), 1);
        daemon.stop().await;
    }
    {
        let upstream = TrickleSummaryUpstream::start().await;
        let port = unused_tcp_port();
        let daemon = Daemon::start_http(port, &upstream.url).await;
        configure_summary_daemon(&daemon, 1).await;
        configure(&daemon, |config| {
            config.upstream.stream_read_timeout_ms = 400
        })
        .await;
        let started = Instant::now();
        let (status, body) =
            send_request(&daemon, port, &minimal_summary(false), CLAUDE_AGENT).await;
        assert!(status.is_server_error(), "{body}");
        assert!(
            body.contains(GENERIC_ERROR),
            "the original response was interrupted: {body}"
        );
        assert!(started.elapsed() >= Duration::from_millis(500));
        assert_eq!(
            upstream.calls.load(Ordering::SeqCst),
            1,
            "expired window must not start recovery"
        );
        let logs = recent_client_logs(&daemon, 1).await;
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0]["attempts"].as_array().unwrap().len(), 1);
        daemon.stop().await;
    }
}

#[tokio::test]
async fn client_summary_policy_and_endpoint_hot_reload_during_cooldown_block_recovery() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    for change in [
        "provider_disabled",
        "key_disabled",
        "key_model_removed",
        "service_account_excluded",
        "endpoint_changed",
    ] {
        let mock = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        Mock::given(method("POST"))
            .and(path("/generateAssistantResponse"))
            .respond_with(move |_: &wiremock::Request| {
                if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(500).set_body_json(json!({"message":GENERIC_ERROR}))
                } else {
                    successful_summary()
                }
            })
            .mount(&mock)
            .await;
        let (daemon, port) = start_summary_daemon(&mock, 1).await;
        configure(&daemon, |config| {
            config.upstream.stream_read_timeout_ms = 3_000;
            config.pool.cooldown.error_cooldown_ms = 1_000;
        })
        .await;
        let request = minimal_summary(false);
        let request_future = send_request(&daemon, port, &request, CLAUDE_AGENT);
        let reload_policy = async {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let accounts = expect_ok(daemon.call("account.list", json!({})).await);
                if calls.load(Ordering::SeqCst) == 1
                    && alignment_account(&accounts)["health"] == "cooling"
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "first failure never entered cooldown: {accounts}"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            // Each case changes one gate. Only endpoint_changed alters the
            // upstream snapshot; it cannot mask the live key/service policy gates.
            configure(&daemon, |config| match change {
                "provider_disabled" => {
                    config.provider = config.effective_providers();
                    config
                        .provider
                        .iter_mut()
                        .find(|provider| provider.id == "kiro")
                        .unwrap()
                        .enabled = false;
                }
                "key_disabled" => {
                    config
                        .api_key
                        .iter_mut()
                        .find(|key| Some(&key.key) == daemon.api_key.as_ref())
                        .unwrap()
                        .enabled = false;
                }
                "key_model_removed" => {
                    config
                        .api_key
                        .iter_mut()
                        .find(|key| Some(&key.key) == daemon.api_key.as_ref())
                        .unwrap()
                        .allowed_models = vec!["resolved-tiny".into()];
                }
                "service_account_excluded" => {
                    let service = config
                        .proxy_service
                        .iter_mut()
                        .find(|service| service.port == port)
                        .unwrap();
                    service.account_ids.retain(|id| id != "acc_99999999");
                    service.excluded_account_ids.push("acc_99999999".into());
                }
                "endpoint_changed" => {
                    config.upstream.preferred_endpoint =
                        Some(crosslane_core::config::Endpoint::Runtime);
                }
                _ => unreachable!(),
            })
            .await;
        };
        let ((status, body), ()) = tokio::join!(request_future, reload_policy);
        assert!(status.is_server_error(), "{change}: {body}");
        assert!(body.contains(GENERIC_ERROR), "{change}: {body}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "recovery bypassed {change}: {body}"
        );
        assert_eq!(generation_requests(&mock).await.len(), 1);
        let accounts = expect_ok(daemon.call("account.list", json!({})).await);
        assert_eq!(alignment_account(&accounts)["enabled"], true);
        daemon.stop().await;
    }
}
