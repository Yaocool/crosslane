#![cfg(feature = "crs")]

use super::*;
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::Mutex;

const PROFILE: &str = "arn:aws:codewhisperer:us-east-1:123456789012:profile/isolated-runtime";
const READER: &str = "checkpoint_reader";
const HISTORY_NONCE: &str = "owned-history-nonce-731 中文😀";
const RUNTIME_TEXT: &str = "RUNTIME_RECOVERED 中文😀";
const PROBE_TEXT: &str = "LATER_AMAZONQ_REQUEST";
const DESCRIPTION: &str = "Read the exact owned checkpoint path";
const EXECUTION_HINT: &str = " This is a client-executed custom function. Return a structured tool call with the requested arguments; the client will perform the operation.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ObservedEndpoint {
    Amazonq,
    Codewhisperer,
    Runtime,
}

fn endpoint(request: &wiremock::Request) -> ObservedEndpoint {
    match request
        .headers
        .get("x-amz-target")
        .map(|value| value.to_str().unwrap())
    {
        None => ObservedEndpoint::Amazonq,
        Some("KiroRuntimeService.GenerateAssistantResponse") => ObservedEndpoint::Runtime,
        Some("AmazonCodeWhispererStreamingService.GenerateAssistantResponse") => {
            ObservedEndpoint::Codewhisperer
        }
        other => panic!("unexpected generation RPC target: {other:?}"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RuntimeReply {
    Valid,
    Refusal,
    Empty,
    Http500,
    Http401,
    EventException,
    CorruptCrc,
}

fn context_only() -> Vec<u8> {
    event_stream_frame("contextUsageEvent", json!({"contextUsagePercentage":0.7}))
}

fn refusal() -> Vec<u8> {
    event_stream_frame(
        "metadataEvent",
        json!({"stopReason":"CONTENT_FILTERED", "stopDetails":{"refusal":{
            "category":"REASONING_EXTRACTION", "explanation":"Owned synthetic refusal."
        }}}),
    )
}

pub(super) fn assert_native_refusal(status: u16, body: &str, stream: bool, category: &str) {
    assert_eq!(status, 200, "{body}");
    let terminal = if stream {
        assert!(body.contains("message_stop"), "{body}");
        body.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|value| value["type"] == "message_delta")
            .unwrap()["delta"]
            .clone()
    } else {
        serde_json::from_str::<Value>(body).unwrap()
    };
    assert_eq!(terminal["stop_reason"], "refusal", "{body}");
    assert_eq!(terminal["stop_details"]["type"], "refusal", "{body}");
    assert_eq!(terminal["stop_details"]["category"], category, "{body}");
    assert!(!body.contains("\"type\":\"error\""), "{body}");
}

fn event_response(body: Vec<u8>) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "application/vnd.amazon.eventstream")
        .set_body_bytes(body)
}

fn runtime_response(reply: RuntimeReply) -> ResponseTemplate {
    match reply {
        RuntimeReply::Valid => event_response(generation_body(RUNTIME_TEXT)),
        RuntimeReply::Refusal => event_response(refusal()),
        RuntimeReply::Empty => event_response(context_only()),
        RuntimeReply::Http500 => ResponseTemplate::new(500)
            .set_body_json(json!({"message":"Owned isolated Runtime failure."})),
        RuntimeReply::Http401 => ResponseTemplate::new(401)
            .set_body_json(json!({"message":"Owned isolated Runtime unauthorized."})),
        RuntimeReply::EventException => event_response(event_stream_frame(
            "serviceUnavailableError",
            json!({"message":"Owned Runtime event failure.","reason":"INSUFFICIENT_MODEL_CAPACITY"}),
        )),
        RuntimeReply::CorruptCrc => {
            let mut body = generation_body("UNVERIFIED_BAD_CRC_TEXT");
            // A zero CRC explicitly disables validation in the fixture codec;
            // make the first frame's CRC nonzero and incorrect instead.
            let first_frame_length = u32::from_be_bytes(body[..4].try_into().unwrap()) as usize;
            body[first_frame_length - 4..first_frame_length].copy_from_slice(&1u32.to_be_bytes());
            event_response(body)
        }
    }
}

pub(super) fn catalog() -> Value {
    let models = [
        "source-large",
        "mapped-small",
        "resolved-small",
        "resolved-tiny",
        "summary-large",
    ]
    .map(|id| {
        json!({"modelId":id,
        "tokenLimits":{"maxInputTokens":1_000_000,"maxOutputTokens":128_000},
        "additionalModelRequestFieldsSchema":{"properties":{"output_config":{"properties":{
            "effort":{"enum":["low","medium","high"]}
        }}}}})
    });
    json!({"models":models})
}

async fn start_daemon(
    mock: &MockServer,
    preferred: Option<crosslane_core::config::Endpoint>,
) -> (Daemon, u16) {
    start_daemon_with_catalog(mock, preferred, catalog()).await
}

async fn start_daemon_with_catalog(
    mock: &MockServer,
    preferred: Option<crosslane_core::config::Endpoint>,
    models: Value,
) -> (Daemon, u16) {
    Mock::given(method("GET"))
        .and(path("/ListAvailableModels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(models))
        .mount(mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/ListAvailableProfiles"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"profiles":[{"arn":PROFILE}]})),
        )
        .expect(0)
        .mount(mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    configure_daemon(&daemon, preferred).await;
    import_profile_account(&daemon).await;
    (daemon, port)
}

pub(super) async fn import_profile_account(daemon: &Daemon) {
    let imported = expect_ok(daemon.call("account.import", json!({"accounts":[{
        "id":"acc_99999999", "email":"isolated-runtime@example.com",
        "machine_id":"9".repeat(64), "profile_arn":PROFILE,
        "credentials":{"access_token":"owned-isolated-runtime-token","region":"us-east-1",
            "expires_at":4_000_000_000i64,"auth_method":"idc"},
        "usage":{"current":0.0,"limit":100.0,"percent_used":0.0,"updated_at":1},
        "subscription":{"kind":"pro"},"created_at":1
    }]})).await);
    assert_eq!(imported["imported"], 1, "{imported}");
    let models = expect_ok(daemon.call("models", json!({})).await);
    assert_eq!(models.as_array().map(Vec::len), Some(5), "{models}");
}

pub(super) async fn configure_daemon(
    daemon: &Daemon,
    preferred: Option<crosslane_core::config::Endpoint>,
) {
    model_policy::configure(daemon, |config| {
        config.upstream.preferred_endpoint = preferred;
        // The request-local Runtime budget must remain bounded even when the
        // outer dispatcher would normally permit another supplier attempt.
        config.upstream.max_retries = 1;
        config.upstream.pool.stream_max_connections = 1;
        config.upstream.stream_slot_wait_timeout_ms = 100;
        config.features.enable_model_fallback = false;
        config.features.enable_prompt_cache = false;
        config.features.auto_continue_rounds = 0;
        config.context.auto_compact_on_overflow = false;
        config.pool.max_concurrent_per_account = 1;
    })
    .await;
}

pub(super) fn request(stream: bool) -> Value {
    json!({"model":"source-large", "max_tokens":128_000, "stream":stream,
    "conversation_id":"d4c46f31-fb4e-4f81-9cec-07ac1bcd0731",
    "system":"Preserve the owned history and tool contract 中文😀.",
    "tools":[{"name":READER,"description":DESCRIPTION,
        "input_schema":{"type":"object","properties":{"path":{"type":"string"}},
            "required":["path"],"additionalProperties":false}}],
    "messages":[
        {"role":"user","content":"Read the owned checkpoint."},
        {"role":"assistant","content":[{"type":"tool_use","id":"history-reader-731",
            "name":READER,"input":{"path":"/owned/历史😀.json"}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"history-reader-731",
            "content":HISTORY_NONCE},{"type":"text","text":"Continue from the owned result; answer in text."}]}
    ]})
}

pub(super) async fn send(daemon: &Daemon, port: u16, body: &Value) -> (u16, String) {
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("x-api-key", daemon.api_key.as_deref().unwrap())
        .header("user-agent", "claude-cli/2.1.300 (external, e2e)")
        .timeout(Duration::from_secs(8))
        .json(body)
        .send()
        .await
        .unwrap();
    (response.status().as_u16(), response.text().await.unwrap())
}

pub(super) fn text(body: &str, stream: bool) -> String {
    if stream {
        body.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
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

fn normalized_transport(mut body: Value) -> Value {
    let object = body.as_object_mut().unwrap();
    object.remove("agentMode");
    object.remove("profileArn");
    let state = body["conversationState"].as_object_mut().unwrap();
    state.remove("agentTaskType");
    state.remove("rootConversationId");
    if let Some(history) = state.get_mut("history").and_then(Value::as_array_mut) {
        for message in history {
            if let Some(user) = message
                .get_mut("userInputMessage")
                .and_then(Value::as_object_mut)
            {
                user.remove("modelId");
            }
        }
    }
    body
}

fn assert_preserved(first: &Value, runtime: &Value) {
    assert_preserved_for_model(first, runtime, "source-large");
}

fn assert_preserved_for_model(first: &Value, runtime: &Value, model: &str) {
    assert_eq!(first["inferenceConfig"]["maxTokens"], 128_000);
    assert_eq!(runtime["inferenceConfig"]["maxTokens"], 128_000);
    assert_eq!(first["profileArn"], PROFILE);
    assert_eq!(runtime["profileArn"], PROFILE);
    assert_eq!(runtime["agentMode"], "vibe");
    assert_eq!(runtime["conversationState"]["agentTaskType"], "vibe");
    assert_eq!(
        runtime["conversationState"]["rootConversationId"],
        first["conversationState"]["conversationId"]
    );
    let id = first["conversationState"]["conversationId"]
        .as_str()
        .unwrap();
    assert!(
        uuid::Uuid::parse_str(id).is_ok(),
        "invalid conversation UUID: {id}"
    );
    assert_eq!(
        runtime["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
        model
    );
    let first_history = first["conversationState"]["history"].as_array().unwrap();
    let runtime_history = runtime["conversationState"]["history"].as_array().unwrap();
    assert_eq!(first_history.len(), runtime_history.len());
    for (original, retried) in first_history.iter().zip(runtime_history) {
        if let Some(user) = original.get("userInputMessage") {
            assert_eq!(user["modelId"], model);
            assert!(
                retried["userInputMessage"].get("modelId").is_none(),
                "Runtime must omit historical modelId, not replace it"
            );
        }
    }
    let first_tools = first
        .pointer("/conversationState/currentMessage/userInputMessage/userInputMessageContext/tools")
        .and_then(Value::as_array)
        .unwrap();
    let specification = first_tools
        .iter()
        .find_map(|tool| tool.get("toolSpecification"))
        .unwrap();
    assert_eq!(specification["description"], DESCRIPTION);
    let serialized = first.to_string();
    for retained in [
        "history-reader-731",
        "/owned/历史😀.json",
        HISTORY_NONCE,
        READER,
    ] {
        assert!(
            serialized.contains(retained),
            "missing source contract: {first}"
        );
    }
    assert_eq!(
        normalized_transport(first.clone()),
        normalized_transport(runtime.clone()),
        "isolated Runtime changed logical input rather than only its transport contract"
    );
}

#[tokio::test]
async fn auto_empty_uses_one_isolated_runtime_attempt_in_json_and_sse() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    for stream in [false, true] {
        for reply in [
            RuntimeReply::Valid,
            RuntimeReply::Refusal,
            RuntimeReply::Empty,
            RuntimeReply::Http500,
            RuntimeReply::Http401,
            RuntimeReply::EventException,
            RuntimeReply::CorruptCrc,
        ] {
            let mock = MockServer::start().await;
            let observed = Arc::new(Mutex::new(Vec::<(ObservedEndpoint, Value)>::new()));
            let recorded = Arc::clone(&observed);
            let probe_cache = matches!(reply, RuntimeReply::Valid | RuntimeReply::Refusal);
            Mock::given(method("POST"))
                .and(path("/generateAssistantResponse"))
                .respond_with(move |request: &wiremock::Request| {
                    let mut requests = recorded.lock().unwrap();
                    let attempt = requests.len();
                    let actual = endpoint(request);
                    requests.push((actual, serde_json::from_slice(&request.body).unwrap()));
                    match attempt {
                        0 => {
                            assert_eq!(actual, ObservedEndpoint::Amazonq);
                            event_response(context_only())
                        }
                        1 => {
                            assert_eq!(actual, ObservedEndpoint::Runtime);
                            runtime_response(reply)
                        }
                        2 if probe_cache => {
                            assert_eq!(
                                actual,
                                ObservedEndpoint::Amazonq,
                                "isolated retry polluted endpoint preference"
                            );
                            event_response(generation_body(PROBE_TEXT))
                        }
                        _ => panic!(
                            "unexpected third generation for stream={stream}, reply={reply:?}"
                        ),
                    }
                })
                .mount(&mock)
                .await;
            let (daemon, port) = start_daemon(&mock, None).await;
            let (status, body) = send(&daemon, port, &request(stream)).await;
            {
                let requests = observed.lock().unwrap();
                assert_eq!(
                    requests.len(),
                    2,
                    "stream={stream}, reply={reply:?}: {body}"
                );
                assert_preserved(&requests[0].1, &requests[1].1);
            }
            match reply {
                RuntimeReply::Valid => {
                    assert_eq!(status, 200, "{body}");
                    assert_eq!(text(&body, stream), RUNTIME_TEXT);
                    assert!(!body.contains("\"type\":\"error\""), "{body}");
                    if stream {
                        assert!(body.contains("message_stop"), "{body}");
                    }
                }
                RuntimeReply::Refusal => {
                    assert_native_refusal(status, &body, stream, "reasoning_extraction");
                }
                RuntimeReply::Empty => {
                    assert_eq!(status, if stream { 200 } else { 502 }, "{body}");
                    assert!(
                        body.contains("no assistant output")
                            || body.contains("upstream_unavailable"),
                        "{body}"
                    );
                    assert!(!body.contains("message_stop"), "{body}");
                }
                RuntimeReply::Http500 | RuntimeReply::Http401 => {
                    let upstream_status = if reply == RuntimeReply::Http500 {
                        500
                    } else {
                        401
                    };
                    assert_eq!(status, if stream { 200 } else { upstream_status }, "{body}");
                    let error = if stream {
                        sse_error_payload(&body)
                    } else {
                        serde_json::from_str::<Value>(&body).unwrap()
                    };
                    assert_eq!(error["error"]["upstream_status"], upstream_status, "{body}");
                    assert_eq!(
                        error["error"]["code"],
                        if reply == RuntimeReply::Http500 {
                            "upstream_unavailable"
                        } else {
                            "upstream_authentication_failed"
                        },
                        "{body}"
                    );
                    if stream {
                        assert!(!body.contains("message_stop"), "{body}");
                    }
                }
                RuntimeReply::EventException | RuntimeReply::CorruptCrc => {
                    assert_eq!(status, if stream { 200 } else { 502 }, "{body}");
                    let error = if stream {
                        sse_error_payload(&body)
                    } else {
                        serde_json::from_str::<Value>(&body).unwrap()
                    };
                    let code = error["error"]["code"].as_str().unwrap();
                    if stream && reply == RuntimeReply::CorruptCrc {
                        assert_eq!(code, "upstream_event_stream_corrupt", "{body}");
                    } else {
                        assert!(
                            matches!(code, "upstream_unavailable" | "upstream_model_unavailable"),
                            "{body}"
                        );
                    }
                    assert!(!body.contains("UNVERIFIED_BAD_CRC_TEXT"), "{body}");
                    assert!(!body.contains("message_stop"), "{body}");
                }
            }
            if probe_cache {
                let probe = json!({"model":"source-large","max_tokens":128,"stream":stream,
                    "messages":[{"role":"user","content":"Check the normal endpoint after an isolated attempt."}]});
                let (status, body) = send(&daemon, port, &probe).await;
                assert_eq!(status, 200, "{body}");
                assert_eq!(text(&body, stream), PROBE_TEXT);
                assert_eq!(observed.lock().unwrap().len(), 3);
            }
            daemon.stop().await;
        }
    }
}

#[tokio::test]
async fn model_capacity_fallback_preserves_native_tool_termination() {
    const SOURCE: &str = "claude-opus-5.5";
    const TARGET: &str = "claude-opus-5";

    let _guard = HTTP_TEST_LOCK.lock().await;
    for stream in [false, true] {
        for reply in [RuntimeReply::Refusal, RuntimeReply::Valid] {
            let mock = MockServer::start().await;
            let observed = Arc::new(Mutex::new(Vec::<(ObservedEndpoint, Value)>::new()));
            let recorded = Arc::clone(&observed);
            Mock::given(method("POST"))
                .and(path("/generateAssistantResponse"))
                .respond_with(move |request: &wiremock::Request| {
                    let mut requests = recorded.lock().unwrap();
                    let attempt = requests.len();
                    let actual = endpoint(request);
                    let payload: Value = serde_json::from_slice(&request.body).unwrap();
                    let expected_model = if attempt == 0 { SOURCE } else { TARGET };
                    assert_eq!(
                        payload["conversationState"]["currentMessage"]["userInputMessage"]
                            ["modelId"],
                        expected_model,
                        "fallback or empty recovery changed the selected model"
                    );
                    assert_eq!(payload["inferenceConfig"]["maxTokens"], 128_000);
                    requests.push((actual, payload));
                    match attempt {
                        0 => {
                            assert_eq!(actual, ObservedEndpoint::Runtime);
                            ResponseTemplate::new(500).set_body_json(json!({
                                "message":"Encountered unexpectedly high load when processing the request, please try again.",
                                "reason":"MODEL_TEMPORARILY_UNAVAILABLE"
                            }))
                        }
                        1 => {
                            assert_eq!(actual, ObservedEndpoint::Runtime);
                            runtime_response(reply)
                        }
                        _ => panic!(
                            "native tool fallback made an extra generation: stream={stream}, reply={reply:?}"
                        ),
                    }
                })
                .expect(2)
                .mount(&mock)
                .await;
            // Exactly one lower version in this family makes the configured
            // dynamic catalog select Opus 5 after Opus 5.5's capacity error.
            let mut models = catalog();
            models["models"][0]["modelId"] = json!(SOURCE);
            models["models"][1]["modelId"] = json!(TARGET);
            let (daemon, port) = start_daemon_with_catalog(&mock, None, models).await;
            model_policy::configure(&daemon, |config| {
                config.features.enable_model_fallback = true;
            })
            .await;
            let mut body = request(stream);
            body["model"] = json!(SOURCE);
            let (status, body) = send(&daemon, port, &body).await;
            {
                let requests = observed.lock().unwrap();
                assert_eq!(
                    requests.len(),
                    2,
                    "stream={stream}, reply={reply:?}: {body}"
                );
                assert_eq!(
                    requests
                        .iter()
                        .map(|(endpoint, _)| *endpoint)
                        .collect::<Vec<_>>(),
                    [ObservedEndpoint::Runtime, ObservedEndpoint::Runtime]
                );
                let (source, fallback) = (&requests[0].1, &requests[1].1);
                assert_eq!(source["profileArn"], PROFILE);
                for message in source["conversationState"]["history"].as_array().unwrap() {
                    if let Some(user) = message.get("userInputMessage") {
                        assert!(user.get("modelId").is_none());
                    }
                }
                let mut expected_fallback = normalized_transport(source.clone());
                expected_fallback["conversationState"]["currentMessage"]["userInputMessage"]
                    ["modelId"] = json!(TARGET);
                assert_eq!(
                    expected_fallback,
                    normalized_transport(fallback.clone()),
                    "capacity fallback changed the source contract beyond its selected model"
                );
                assert_eq!(fallback["inferenceConfig"]["maxTokens"], 128_000);
                assert_eq!(fallback["profileArn"], PROFILE);
            }
            if reply == RuntimeReply::Refusal {
                assert_native_refusal(status, &body, stream, "reasoning_extraction");
            } else {
                assert_eq!(status, 200, "{body}");
                assert_eq!(text(&body, stream), RUNTIME_TEXT);
                assert!(!body.contains("\"type\":\"error\""), "{body}");
                if stream {
                    assert!(body.contains("message_stop"), "{body}");
                }
            }
            assert_eq!(
                observed.lock().unwrap().len(),
                2,
                "no replay after Runtime result"
            );
            daemon.stop().await;
        }
    }
}

#[tokio::test]
async fn ordinary_text_empty_replies_also_use_one_runtime_attempt_in_json_and_sse() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    for stream in [false, true] {
        let mock = MockServer::start().await;
        let observed = Arc::new(Mutex::new(Vec::<(ObservedEndpoint, Value)>::new()));
        let recorded = Arc::clone(&observed);
        Mock::given(method("POST"))
            .and(path("/generateAssistantResponse"))
            .respond_with(move |request: &wiremock::Request| {
                let mut requests = recorded.lock().unwrap();
                let attempt = requests.len();
                let actual = endpoint(request);
                requests.push((actual, serde_json::from_slice(&request.body).unwrap()));
                match attempt {
                    0 => {
                        assert_eq!(actual, ObservedEndpoint::Amazonq);
                        event_response(context_only())
                    }
                    1 => {
                        assert_eq!(actual, ObservedEndpoint::Runtime);
                        event_response(generation_body(RUNTIME_TEXT))
                    }
                    _ => panic!("ordinary text recovery made a third generation"),
                }
            })
            .mount(&mock)
            .await;
        let (daemon, port) = start_daemon(&mock, None).await;
        let mut ordinary = request(stream);
        ordinary.as_object_mut().unwrap().remove("tools");
        ordinary["messages"] =
            json!([{"role":"user","content":"Answer this ordinary text request 中文😀."}]);
        let (status, body) = send(&daemon, port, &ordinary).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(text(&body, stream), RUNTIME_TEXT);
        {
            let requests = observed.lock().unwrap();
            assert_eq!(requests.len(), 2, "{body}");
            let (first, runtime) = (&requests[0].1, &requests[1].1);
            assert_eq!(first["inferenceConfig"]["maxTokens"], 128_000);
            assert_eq!(runtime["inferenceConfig"]["maxTokens"], 128_000);
            assert_eq!(
                runtime["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
                "source-large"
            );
            assert_eq!(runtime["profileArn"], PROFILE);
            assert_eq!(
                runtime["conversationState"]["rootConversationId"],
                first["conversationState"]["conversationId"]
            );
            assert_eq!(
                normalized_transport(first.clone()),
                normalized_transport(runtime.clone()),
                "ordinary retry changed the frozen logical input"
            );
        }
        daemon.stop().await;
    }
}

#[tokio::test]
async fn explicit_preferred_endpoint_keeps_the_existing_same_policy_retry() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    for stream in [false, true] {
        for (preferred, expected) in [
            (
                crosslane_core::config::Endpoint::Amazonq,
                ObservedEndpoint::Amazonq,
            ),
            (
                crosslane_core::config::Endpoint::Codewhisperer,
                ObservedEndpoint::Codewhisperer,
            ),
        ] {
            let mock = MockServer::start().await;
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&calls);
            Mock::given(method("POST"))
                .and(path("/generateAssistantResponse"))
                .respond_with(move |request: &wiremock::Request| {
                    assert_eq!(
                        endpoint(request),
                        expected,
                        "explicit preferred endpoint was overridden"
                    );
                    let attempt = observed.fetch_add(1, Ordering::SeqCst);
                    assert!(attempt < 2, "explicit retry made a third generation");
                    event_response(if attempt == 0 {
                        context_only()
                    } else {
                        generation_body(RUNTIME_TEXT)
                    })
                })
                .mount(&mock)
                .await;
            let (daemon, port) = start_daemon(&mock, Some(preferred)).await;
            // This path intentionally retains the pre-existing same-endpoint
            // clarification. The auto Runtime cases above use the original
            // unmodified description and must not pass through that rewrite.
            let mut explicit_request = request(stream);
            explicit_request["tools"][0]["description"] =
                json!(format!("{DESCRIPTION}{EXECUTION_HINT}"));
            let (status, body) = send(&daemon, port, &explicit_request).await;
            assert_eq!(status, 200, "{body}");
            assert_eq!(text(&body, stream), RUNTIME_TEXT);
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            daemon.stop().await;
        }
    }
}

#[tokio::test]
async fn visible_sse_output_cannot_be_replayed_to_runtime_after_a_terminal_refusal() {
    use axum::body::{Body, Bytes};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use std::convert::Infallible;
    use tokio::sync::Notify;

    let _guard = HTTP_TEST_LOCK.lock().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    let release = Arc::new(Notify::new());
    let generation_calls = Arc::new(AtomicUsize::new(0));
    let profile_calls = Arc::new(AtomicUsize::new(0));
    let (gate, generations, profiles) = (
        release.clone(),
        generation_calls.clone(),
        profile_calls.clone(),
    );
    let app = axum::Router::new()
        .route("/ListAvailableModels", get(|| async {axum::Json(catalog())}))
        .route("/ListAvailableProfiles", post(move || {
            let profiles = profiles.clone();
            async move {profiles.fetch_add(1, Ordering::SeqCst); axum::Json(json!({"profiles":[{"arn":PROFILE}]}))}
        }))
        .route("/generateAssistantResponse", post(move || {
            let (gate, generations) = (gate.clone(), generations.clone());
            async move {
                generations.fetch_add(1, Ordering::SeqCst);
                let output = async_stream::stream! {
                    yield Ok::<_, Infallible>(Bytes::from(event_stream_frame(
                        "assistantResponseEvent", json!({"content":"VISIBLE_BEFORE_TERMINAL_REFUSAL "})
                    )));
                    gate.notified().await;
                    yield Ok::<_, Infallible>(Bytes::from(refusal()));
                };
                ([("content-type", "application/vnd.amazon.eventstream")], Body::from_stream(output)).into_response()
            }
        }));
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let port = unused_tcp_port();
    let daemon = Daemon::start_http(port, &format!("{upstream}/generateAssistantResponse")).await;
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
        while !String::from_utf8_lossy(&output).contains("VISIBLE_BEFORE_TERMINAL_REFUSAL") {
            output.extend_from_slice(
                &source
                    .next()
                    .await
                    .expect("visible output before EOF")
                    .unwrap(),
            );
        }
    })
    .await
    .expect("semantic output must arrive before the upstream terminal event");
    release.notify_one();
    while let Some(chunk) = source.next().await {
        output.extend_from_slice(&chunk.unwrap());
    }
    let output = String::from_utf8(output).unwrap();
    assert_eq!(text(&output, true), "VISIBLE_BEFORE_TERMINAL_REFUSAL ");
    assert_native_refusal(200, &output, true, "reasoning_extraction");
    assert_eq!(generation_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        profile_calls.load(Ordering::SeqCst),
        0,
        "no Runtime preflight after semantic output"
    );
    daemon.stop().await;
    task.abort();
}

#[tokio::test]
async fn runtime_refusal_discards_partial_tool_calls_and_settles_reported_usage_once() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    for (stream, buffered) in [(false, false), (true, true), (true, false)] {
        let mock = MockServer::start().await;
        let mut body = event_stream_frame(
            "toolUseEvent",
            json!({
                "name":READER,"toolUseId":"never-execute","input":"{\"path\":\"/partial"
            }),
        );
        body.extend(event_stream_frame("metadataEvent", json!({
            "stopReason":"CONTENT_FILTERED",
            "stopDetails":{"refusal":{"category":"REASONING_EXTRACTION","explanation":"Owned refusal."}},
            "usage":{"inputTokens":31,"outputTokens":7,"creditsConsumed":0.125}
        })));
        Mock::given(method("POST"))
            .respond_with(event_response(body))
            .expect(1)
            .mount(&mock)
            .await;
        let mut models = catalog();
        models["models"][0]["modelId"] = json!("claude-opus-5");
        let (daemon, port) = start_daemon_with_catalog(&mock, None, models).await;
        model_policy::configure(&daemon, |config| {
            config.features.buffer_tool_calls = buffered
        })
        .await;
        let mut input = request(stream);
        input["model"] = json!("claude-opus-5");
        let (status, response) = send(&daemon, port, &input).await;
        assert_native_refusal(status, &response, stream, "reasoning_extraction");
        assert!(!response.contains("never-execute"), "{response}");
        assert!(!response.contains("/partial"), "{response}");
        let stats = expect_ok(
            daemon
                .call("stats", json!({"detail":true,"recent":10}))
                .await,
        );
        assert_eq!(stats["stats"]["total"]["requests"], 1, "{stats}");
        assert_eq!(stats["stats"]["total"]["credits"], 0.125, "{stats}");
        let row = &stats["stats"]["recent_requests"][0];
        assert_eq!(row["input_tokens"], 31, "{row}");
        assert_eq!(row["output_tokens"], 7, "{row}");
        daemon.stop().await;
    }
}

#[tokio::test]
async fn native_refusal_preserves_prior_round_charges_without_estimated_refusal_usage() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    for stream in [false, true] {
        let mock = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        Mock::given(method("POST"))
            .respond_with(move |_: &wiremock::Request| {
                match observed.fetch_add(1, Ordering::SeqCst) {
                    0 => event_response(event_stream_frame(
                        "messageMetadataEvent",
                        json!({"usage":{
                            "inputTokens":41,"outputTokens":0,"creditsConsumed":0.25
                        }}),
                    )),
                    1 => event_response(refusal()),
                    _ => panic!("no proxy retry after native refusal"),
                }
            })
            .expect(2)
            .mount(&mock)
            .await;
        let (daemon, port) = start_daemon(&mock, None).await;
        let (status, body) = send(&daemon, port, &request(stream)).await;
        assert_native_refusal(status, &body, stream, "reasoning_extraction");
        let stats = expect_ok(
            daemon
                .call("stats", json!({"detail":true,"recent":10}))
                .await,
        );
        let row = &stats["stats"]["recent_requests"][0];
        assert_eq!(row["input_tokens"], 41, "{row}");
        assert_eq!(row["output_tokens"], 0, "{row}");
        assert_eq!(row["credits"], 0.25, "{row}");
        assert_eq!(stats["stats"]["total"]["requests"], 1, "{stats}");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        daemon.stop().await;
    }
}

#[tokio::test]
async fn native_refusal_retains_unmetered_partial_usage_without_publishing_calls() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    for (stream, reasoning_only) in [(false, false), (true, false), (false, true), (true, true)] {
        let mock = MockServer::start().await;
        let mut body = if reasoning_only {
            event_stream_frame(
                "reasoningContentEvent",
                json!({
                    "reasoningContentEvent":{"text":"Observed generation ".repeat(500)}
                }),
            )
        } else {
            event_stream_frame(
                "toolUseEvent",
                json!({
                    "name":READER,"toolUseId":"cancelled-unmetered",
                    "input":format!("{{\"path\":\"/partial{}", " long argument".repeat(500))
                }),
            )
        };
        body.extend(refusal());
        Mock::given(method("POST"))
            .respond_with(event_response(body))
            .expect(1)
            .mount(&mock)
            .await;
        let mut models = catalog();
        models["models"][0]["modelId"] = json!("claude-opus-5");
        let (daemon, port) = start_daemon_with_catalog(&mock, None, models).await;
        let mut input = request(stream);
        input["model"] = json!("claude-opus-5");
        input["thinking"] = json!({"type":"adaptive","display":"summarized"});
        let (status, response) = send(&daemon, port, &input).await;
        assert_native_refusal(status, &response, stream, "reasoning_extraction");
        assert!(!response.contains("cancelled-unmetered"), "{response}");
        assert!(!response.contains("/partial"), "{response}");
        let stats = expect_ok(
            daemon
                .call("stats", json!({"detail":true,"recent":10}))
                .await,
        );
        let row = &stats["stats"]["recent_requests"][0];
        assert!(row["input_tokens"].as_u64().unwrap() > 0, "{row}");
        assert!(row["output_tokens"].as_u64().unwrap() > 100, "{row}");
        assert!(row["credits"].as_f64().unwrap() > 0.0, "{row}");
        assert_eq!(row["diagnostics"]["credits_source"], "estimated", "{row}");
        assert_eq!(stats["stats"]["total"]["requests"], 1, "{stats}");
        daemon.stop().await;
    }
}
