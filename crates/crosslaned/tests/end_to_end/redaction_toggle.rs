//! Synthetic credentials and a local upstream; no real account or model call.

use std::sync::Mutex;

use serde_json::{json, Value};
use tokio::sync::Notify;

use super::*;

const SECRET: &str = "opaque-toggle-fixture-token";
const MARKER_PREFIX: &str = "__CROSSLANE_SECRET_";
const HINT: &str = "[Gateway credential references]";

#[derive(Clone)]
struct CredentialEcho {
    requests: Arc<Mutex<Vec<Value>>>,
    received: Option<Arc<Notify>>,
    delay: Duration,
}

impl wiremock::Respond for CredentialEcho {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let payload: Value = serde_json::from_slice(&request.body).unwrap();
        let content = payload
            .pointer("/conversationState/currentMessage/userInputMessage/content")
            .and_then(Value::as_str)
            .unwrap();
        // Echo the model-visible value, including references split between
        // native tool frames, rather than guessing the local credential.
        let token = if let Some(marker) = reference_in(content) {
            marker.to_owned()
        } else {
            assert!(content.contains(SECRET));
            SECRET.to_owned()
        };
        self.requests.lock().unwrap().push(payload);
        if let Some(received) = &self.received {
            received.notify_one();
        }
        let input = json!({"token":token,"count":3,"literal":"$CLOUD_TOKEN"}).to_string();
        let middle = input.len() / 2;
        let mut body = event_stream_frame(
            "toolUseEvent",
            json!({"toolUseId":"toggle-tool","name":"request","input":&input[..middle],"stop":false}),
        );
        body.extend(event_stream_frame(
            "toolUseEvent",
            json!({"toolUseId":"toggle-tool","input":&input[middle..],"stop":true}),
        ));
        body.extend(event_stream_frame(
            "messageMetadataEvent",
            json!({"usage":{"inputTokens":100,"outputTokens":40,"creditsConsumed":0.1}}),
        ));
        ResponseTemplate::new(200)
            .insert_header("content-type", "application/vnd.amazon.eventstream")
            .set_delay(self.delay)
            .set_body_bytes(body)
    }
}

fn reference_in(content: &str) -> Option<&str> {
    content.match_indices(MARKER_PREFIX).find_map(|(start, _)| {
        let suffix = &content[start + MARKER_PREFIX.len()..];
        let nonce = suffix.get(..32)?;
        if !nonce.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let index = suffix[32..].strip_prefix('_')?;
        let digits = index.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || !index[digits..].starts_with("__") {
            return None;
        }
        let end = start + MARKER_PREFIX.len() + 32 + 1 + digits + 2;
        content.get(start..end)
    })
}

fn client_request(claude: bool, stream: bool) -> Value {
    let schema = json!({
        "type":"object",
        "properties":{
            "token":{"type":"string","pattern":"^opaque-"},
            "count":{"type":"integer"},
            "literal":{"type":"string"}
        },
        "required":["token","count","literal"],
        "additionalProperties":false
    });
    json!({
        "model":"source-large","max_tokens":256,"stream":stream,
        "messages":[{"role":"user","content":format!("token={SECRET}\nUse the request tool.")}],
        "tools":if claude {
            json!([{"name":"request","description":"Use the provided token","input_schema":schema}])
        } else {
            json!([{"type":"function","function":{"name":"request","description":"Use the provided token","parameters":schema}}])
        }
    })
}

async fn call_tool(daemon: &Daemon, port: u16, claude: bool, stream: bool) -> reqwest::Response {
    let route = if claude {
        "messages"
    } else {
        "chat/completions"
    };
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap()
        .post(format!("http://127.0.0.1:{port}/v1/{route}"))
        .bearer_auth(daemon.api_key.as_deref().unwrap())
        .header(
            "user-agent",
            if claude {
                "claude-cli/2.1.296 (external, sdk-cli)"
            } else {
                "codex_cli_rs/0.147.0 (e2e)"
            },
        )
        .json(&client_request(claude, stream))
        .send()
        .await
        .unwrap()
}

fn returned_input(body: &str, claude: bool, stream: bool) -> Value {
    if !stream {
        let response: Value = serde_json::from_str(body).unwrap();
        return if claude {
            response["content"]
                .as_array()
                .unwrap()
                .iter()
                .find(|block| block["type"] == "tool_use")
                .unwrap()["input"]
                .clone()
        } else {
            serde_json::from_str(
                response["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap()
        };
    }
    let mut arguments = String::new();
    let mut initial = None;
    for event in body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str::<Value>(data).unwrap())
    {
        if claude {
            if event["type"] == "content_block_start"
                && event["content_block"]["type"] == "tool_use"
            {
                initial = Some(event["content_block"]["input"].clone());
            }
            if let Some(part) = event["delta"]["partial_json"].as_str() {
                arguments.push_str(part);
            }
        } else if let Some(calls) = event["choices"][0]["delta"]["tool_calls"].as_array() {
            for call in calls {
                if let Some(part) = call["function"]["arguments"].as_str() {
                    arguments.push_str(part);
                }
            }
        }
    }
    if arguments.is_empty() {
        initial.expect("stream contains the declared tool")
    } else {
        serde_json::from_str(&arguments).unwrap()
    }
}

async fn configure_redaction(daemon: &Daemon, enabled: bool) {
    super::model_policy::configure(daemon, |config| {
        config.features.enable_request_redaction = enabled;
        config.features.enhance_system_prompt = false;
        config.features.enable_model_fallback = false;
        config.features.tool_call_buffer_delay_ms = 0;
        config.upstream.max_retries = 0;
    })
    .await;
}

fn check_wire(payload: &Value, enabled: bool) {
    let wire = payload.to_string();
    let schema = payload
        .pointer("/conversationState/currentMessage/userInputMessage/userInputMessageContext/tools/0/toolSpecification/inputSchema/json")
        .unwrap();
    if enabled {
        assert!(
            !wire.contains(SECRET),
            "original secret reached the mock Kiro wire"
        );
        assert!(wire.contains(MARKER_PREFIX));
        assert!(wire.contains(HINT));
        assert!(schema["properties"]["token"]["anyOf"].is_array());
    } else {
        assert!(wire.contains(SECRET));
        assert!(!wire.contains(MARKER_PREFIX));
        assert!(!wire.contains(HINT));
        assert_eq!(
            schema["properties"]["token"],
            json!({"type":"string","pattern":"^opaque-"})
        );
    }
}

#[tokio::test]
async fn config_reload_toggles_request_redaction_without_changing_tool_arguments() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(CredentialEcho {
            requests: Arc::clone(&requests),
            received: None,
            delay: Duration::ZERO,
        })
        .expect(12)
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    for enabled in [true, false, true] {
        configure_redaction(&daemon, enabled).await;
        for claude in [true, false] {
            for stream in [false, true] {
                let response = call_tool(&daemon, port, claude, stream).await;
                assert_eq!(response.status(), 200);
                let body = response.text().await.unwrap();
                assert_eq!(
                    returned_input(&body, claude, stream),
                    json!({"token":SECRET,"count":3,"literal":"$CLOUD_TOKEN"})
                );
                check_wire(requests.lock().unwrap().last().unwrap(), enabled);
            }
        }
    }
    daemon.stop().await;
}

#[tokio::test]
async fn config_reload_keeps_an_inflight_redaction_mapping_until_tool_return() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let received = Arc::new(Notify::new());
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(CredentialEcho {
            requests: Arc::clone(&requests),
            received: Some(Arc::clone(&received)),
            delay: Duration::from_secs(2),
        })
        .expect(1)
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon = Arc::new(
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await,
    );
    import_context_alignment_account(&daemon, 0.0).await;
    configure_redaction(&daemon, true).await;
    let ongoing_daemon = Arc::clone(&daemon);
    let ongoing = tokio::spawn(async move { call_tool(&ongoing_daemon, port, true, false).await });
    tokio::time::timeout(Duration::from_secs(10), received.notified())
        .await
        .unwrap();
    check_wire(requests.lock().unwrap().last().unwrap(), true);
    configure_redaction(&daemon, false).await;
    assert!(
        !ongoing.is_finished(),
        "upstream delay must cover the config reload"
    );
    let response = ongoing.await.unwrap();
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    assert_eq!(
        returned_input(&body, true, false),
        json!({"token":SECRET,"count":3,"literal":"$CLOUD_TOKEN"})
    );
    Arc::try_unwrap(daemon).ok().unwrap().stop().await;
}

#[tokio::test]
async fn disabled_redaction_keeps_hosted_mcp_queries_and_continuations_unmasked() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    const SEARCH_SECRET: &str = "ksk_toggle_search_fixture";
    const QUERY: &str = "lookup ksk_toggle_search_fixture";
    let mock = MockServer::start().await;
    let rounds = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&rounds);
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(move |request: &wiremock::Request| {
            let payload: Value = serde_json::from_slice(&request.body).unwrap();
            let wire = payload.to_string();
            assert!(wire.contains(SEARCH_SECRET));
            assert!(!wire.contains(MARKER_PREFIX));
            assert!(!wire.contains(HINT));
            let body = if count.fetch_add(1, Ordering::SeqCst) == 0 {
                event_stream_frame(
                    "toolUseEvent",
                    json!({"toolUseId":"toggle-search","name":"web_search","input":json!({"query":QUERY}).to_string(),"stop":true}),
                )
            } else {
                let results = payload
                    .pointer("/conversationState/currentMessage/userInputMessage/userInputMessageContext/toolResults")
                    .unwrap()
                    .to_string();
                assert!(results.contains(QUERY));
                assert!(results.contains("verified toggle search snippet"));
                generation_body("Search completed.")
            };
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(body)
        })
        .expect(2)
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(|request: &wiremock::Request| {
            let payload: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(payload["params"]["arguments"]["query"], QUERY);
            assert!(!payload.to_string().contains(MARKER_PREFIX));
            ResponseTemplate::new(200).set_body_json(json!({"result":{"content":[{
                "type":"text","text":json!({"query":QUERY,"totalResults":1,"results":[{
                    "title":"Toggle fixture","url":"https://example.test/toggle","snippet":"verified toggle search snippet"
                }]}).to_string()
            }]}}))
        })
        .expect(1)
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    expect_ok(daemon.call("account.import",json!({"accounts":[{
        "id":"acc_77777777","email":"search@example.test","machine_id":"7".repeat(64),
        "credentials":{"access_token":"ksk_synthetic-search","region":"us-east-1","expires_at":0,"auth_method":"api_key"},"created_at":1
    }]})).await);
    configure_redaction(&daemon, false).await;
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .bearer_auth(daemon.api_key.as_deref().unwrap())
        .header("user-agent", "claude-cli/2.1.300 (external, e2e)")
        .json(&json!({
            "model":"claude-sonnet-4","max_tokens":1024,
            "messages":[{"role":"user","content":format!("Find sources for {SEARCH_SECRET}")}],
            "tools":[{"type":"web_search_20250305","name":"web_search"}]
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("Search completed."), "{body}");
    assert!(body.contains("web_search_tool_result"), "{body}");
    assert_eq!(rounds.load(Ordering::SeqCst), 2);
    daemon.stop().await;
}
