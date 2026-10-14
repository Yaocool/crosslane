use super::*;
use serde_json::{json, Value};

const ORIGINAL: &str = "mcp__mcp-gateway__orch_mcp_server__orch_dingtalk_notification_send";
const UNKNOWN_ALIAS: &str = "mcp_mcp-gateway__orch_mcp_server__orch_dingtalk_no_447129d6e2a1";

#[derive(Clone)]
struct ToolReply {
    expected_upstream: Option<String>,
    emitted: String,
    visible_text: bool,
    malformed_input: bool,
}

impl wiremock::Respond for ToolReply {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let value: Value = serde_json::from_slice(&request.body).unwrap();
        let tool = value.pointer(
            "/conversationState/currentMessage/userInputMessage/userInputMessageContext/tools/0/toolSpecification/name",
        );
        assert_eq!(
            tool.and_then(Value::as_str),
            self.expected_upstream.as_deref()
        );
        let mut body = if self.visible_text {
            event_stream_frame(
                "assistantResponseEvent",
                json!({"content":"Visible before the tool."}),
            )
        } else {
            Vec::new()
        };
        body.extend(event_stream_frame(
            "toolUseEvent",
            json!({
                "toolUseId":"call_tool_name", "name":self.emitted,
                "input":if self.malformed_input { "{invalid" } else { "{" }, "stop":false
            }),
        ));
        // Later fragments have only the call ID, so both response modes must
        // retain the validated identity from the first fragment.
        body.extend(event_stream_frame(
            "toolUseEvent",
            json!({
                "toolUseId":"call_tool_name", "input":"}", "stop":true
            }),
        ));
        body.extend(event_stream_frame("messageMetadataEvent", json!({
            "messageMetadataEvent":{"usage":{"inputTokens":100,"outputTokens":10,"creditsConsumed":0.1}}
        })));
        ResponseTemplate::new(200)
            .insert_header("content-type", "application/vnd.amazon.eventstream")
            .set_body_bytes(body)
    }
}

fn request(endpoint: &str, names: &[&str], stream: bool) -> Value {
    let mut value = json!({"model":"source-large","stream":stream});
    if endpoint == "responses" {
        value["max_output_tokens"] = json!(128);
        value["input"] = json!("Use the declared tool.");
        value["tools"] = json!(names.iter().map(|name| {
            json!({"type":"function","name":name,"description":"Test tool","parameters":{"type":"object"}})
        }).collect::<Vec<_>>());
    } else {
        value["max_tokens"] = json!(128);
        value["messages"] = json!([{"role":"user","content":"Use the declared tool."}]);
        value["tools"] = json!(names.iter().map(|name| {
            if endpoint == "messages" {
                json!({"name":name,"description":"Test tool","input_schema":{"type":"object"}})
            } else {
                json!({"type":"function","function":{"name":name,"description":"Test tool","parameters":{"type":"object"}}})
            }
        }).collect::<Vec<_>>());
    }
    value
}

async fn send(daemon: &Daemon, port: u16, endpoint: &str, body: &Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/{endpoint}"))
        .bearer_auth(daemon.api_key.as_deref().unwrap())
        .header(
            "user-agent",
            if endpoint == "messages" {
                "claude-cli/2.1.296 (external, sdk-cli)"
            } else {
                "codex_cli_rs/0.147.0 (test)"
            },
        )
        .json(body)
        .send()
        .await
        .unwrap()
}

fn returned_names(value: &Value, names: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            if matches!(
                object.get("type").and_then(Value::as_str),
                Some("tool_use" | "function_call")
            ) {
                names.push(object["name"].as_str().unwrap().to_owned());
            }
            if let Some(name) = object
                .get("function")
                .and_then(|function| function["name"].as_str())
            {
                if !name.is_empty() {
                    names.push(name.to_owned());
                }
            }
            for child in object.values() {
                returned_names(child, names);
            }
        }
        Value::Array(values) => {
            for value in values {
                returned_names(value, names);
            }
        }
        _ => {}
    }
}

fn values(body: &str, stream: bool) -> Vec<Value> {
    if stream {
        body.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter(|data| *data != "[DONE]")
            .map(|data| serde_json::from_str(data).unwrap())
            .collect()
    } else {
        vec![serde_json::from_str(body).unwrap()]
    }
}

#[tokio::test]
async fn declared_tool_names_restore_across_protocols_and_buffering_modes() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let alias = crosslane_translate::tool_name(ORIGINAL);
    let collision = crosslane_translate::ToolNameRegistry::new(["mcp.a/read", "mcp_a/read"])
        .kiro_name("mcp.a/read");
    let cases = [
        (vec![ORIGINAL], alias.as_str(), ORIGINAL),
        (vec![ORIGINAL], ORIGINAL, ORIGINAL),
        (vec![alias.as_str()], alias.as_str(), alias.as_str()),
        (vec!["read_file"], "read_file", "read_file"),
        (
            vec!["mcp.a/read", "mcp_a/read"],
            collision.as_str(),
            "mcp.a/read",
        ),
    ];
    for buffered in [false, true] {
        let mock = MockServer::start().await;
        mount_context_alignment_models(&mock).await;
        let port = unused_tcp_port();
        let daemon =
            Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
        import_context_alignment_account(&daemon, 0.0).await;
        model_policy::configure(&daemon, |config| {
            config.features.buffer_tool_calls = buffered
        })
        .await;
        for endpoint in ["messages", "chat/completions", "responses"] {
            for stream in [false, true] {
                for (declared, emitted, expected) in &cases {
                    let expected_upstream =
                        crosslane_translate::ToolNameRegistry::new(declared.iter().copied())
                            .kiro_name(declared[0]);
                    Mock::given(method("POST"))
                        .and(path("/generateAssistantResponse"))
                        .respond_with(ToolReply {
                            expected_upstream: Some(expected_upstream),
                            emitted: (*emitted).into(),
                            visible_text: false,
                            malformed_input: false,
                        })
                        .mount(&mock)
                        .await;
                    let response = send(
                        &daemon,
                        port,
                        endpoint,
                        &request(endpoint, declared, stream),
                    )
                    .await;
                    let status = response.status();
                    let body = response.text().await.unwrap();
                    assert_eq!(
                        status, 200,
                        "{endpoint} stream={stream} buffered={buffered}: {body}"
                    );
                    let mut names = Vec::new();
                    for value in values(&body, stream) {
                        returned_names(&value, &mut names);
                    }
                    assert!(!names.is_empty(), "{body}");
                    assert!(
                        names.iter().all(|name| name == expected),
                        "{endpoint} stream={stream} buffered={buffered}: {names:?}"
                    );
                    mock.reset().await;
                    mount_context_alignment_models(&mock).await;
                }
            }
        }
        daemon.stop().await;
    }
}

#[tokio::test]
async fn unknown_tool_names_fail_without_leaking_calls_or_retrying_accounts() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let cases = [
        (UNKNOWN_ALIAS, false, false, true),
        (UNKNOWN_ALIAS, true, false, true),
        (UNKNOWN_ALIAS, false, true, true),
        ("unknown_plain_tool", false, false, true),
        ("", false, false, true),
        (UNKNOWN_ALIAS, false, false, false),
        (UNKNOWN_ALIAS, false, true, false),
    ];
    for buffered in [false, true] {
        let mock = MockServer::start().await;
        mount_context_alignment_models(&mock).await;
        let port = unused_tcp_port();
        let daemon =
            Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
        import_context_alignment_account(&daemon, 0.0).await;
        // Two accounts and a retry budget ensure a name mismatch does not
        // rotate credentials or replay a partially visible response.
        expect_ok(daemon.call("account.import", json!({"accounts":[{
            "id":"acc_99999998", "email":"tool-names@example.com", "machine_id":"8".repeat(64),
            "credentials":{"access_token":"second-test-token","region":"us-east-1","expires_at":4_000_000_000i64,"auth_method":"idc"},
            "usage":{"current":0.0,"limit":100.0,"percent_used":0.0,"updated_at":1},
            "subscription":{"kind":"pro"},"created_at":1
        }]})).await);
        expect_ok(daemon.call("models", json!({"refresh":true})).await);
        model_policy::configure(&daemon, |config| {
            config.features.buffer_tool_calls = buffered;
            config.features.auto_continue_rounds = 0;
            config.upstream.max_retries = 2;
        })
        .await;
        for endpoint in ["messages", "chat/completions", "responses"] {
            for stream in [false, true] {
                for (emitted, visible_text, malformed_input, declared) in cases {
                    Mock::given(method("POST"))
                        .and(path("/generateAssistantResponse"))
                        .respond_with(ToolReply {
                            expected_upstream: declared
                                .then(|| crosslane_translate::tool_name(ORIGINAL)),
                            emitted: emitted.into(),
                            visible_text,
                            malformed_input,
                        })
                        .mount(&mock)
                        .await;
                    let definitions = if declared { vec![ORIGINAL] } else { vec![] };
                    let response = send(
                        &daemon,
                        port,
                        endpoint,
                        &request(endpoint, &definitions, stream),
                    )
                    .await;
                    let status = response.status();
                    let body = response.text().await.unwrap();
                    assert_eq!(
                        status,
                        if stream { 200 } else { 502 },
                        "{endpoint} stream={stream} buffered={buffered}: {body}"
                    );
                    let events = values(&body, stream);
                    let mut names = Vec::new();
                    for event in &events {
                        returned_names(event, &mut names);
                    }
                    assert!(names.is_empty(), "unexpected client tool call: {body}");
                    assert!(
                        events.iter().any(|event| event["type"] == "error"
                            || event["error"].is_object()
                            || event["response"]["error"].is_object()),
                        "missing error: {body}"
                    );
                    assert!(body.contains("upstream_tool_name_mismatch"), "{body}");
                    if !emitted.is_empty() {
                        assert!(!body.contains(emitted), "unknown name leaked: {body}");
                    }
                    assert!(
                        !body.contains("response.completed")
                            && !body.contains("event: message_stop"),
                        "failed response completed: {body}"
                    );
                    if stream && visible_text {
                        assert!(body.contains("Visible before the tool."), "{body}");
                    }
                    let calls = mock
                        .received_requests()
                        .await
                        .unwrap()
                        .iter()
                        .filter(|request| request.url.path() == "/generateAssistantResponse")
                        .count();
                    assert_eq!(calls, 1, "name mismatch retried upstream: {endpoint} stream={stream} buffered={buffered} emitted={emitted:?} declared={declared} malformed={malformed_input} body={body}");
                    mock.reset().await;
                    mount_context_alignment_models(&mock).await;
                }
            }
        }
        daemon.stop().await;
    }
}
