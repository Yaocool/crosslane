use super::*;
use serde_json::{json, Value};

const DISCOVERED: &str = "mcp__mcp-gateway__orch_mcp_server__orch_dingtalk_notification_send";
const DEFERRED_COUNT: usize = 100;

fn tools(deferred: bool) -> Vec<Value> {
    let mut tools = vec![json!({
        "name":"ToolSearch", "description":"Find tools by query before calling them.",
        "input_schema":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}
    })];
    tools.extend((0..DEFERRED_COUNT).map(|index| json!({
        "name":if index == 0 { DISCOVERED.to_owned() } else { format!("mcp__example__unused_{index}") },
        "description":"Synthetic tool documentation. ".repeat(20),
        "input_schema":{"type":"object","properties":{},"additionalProperties":false},
        "defer_loading":deferred
    })));
    tools
}

fn search_call() -> Value {
    json!({"type":"tool_use","id":"toolu_search","name":"ToolSearch","input":{"query":DISCOVERED}})
}

fn references() -> Value {
    json!({"type":"tool_result","tool_use_id":"toolu_search","content":[
        {"type":"text","text":"Matching tools:"},
        {"type":"tool_reference","tool_name":DISCOVERED}
    ]})
}

fn content(body: &str, streaming: bool) -> Vec<Value> {
    if streaming {
        body.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str::<Value>(data).ok())
            .filter_map(|event| event.get("content_block").cloned())
            .collect()
    } else {
        serde_json::from_str::<Value>(body).unwrap()["content"]
            .as_array()
            .unwrap()
            .clone()
    }
}

#[tokio::test]
async fn client_tool_search_loads_only_discovered_tools_in_json_and_sse() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let alias = crosslane_translate::tool_name(DISCOVERED);
    let upstream_name = alias.clone();
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(move |request: &wiremock::Request| {
            let phase = count.fetch_add(1, Ordering::SeqCst) % 5;
            let wire: Value = serde_json::from_slice(&request.body).unwrap();
            let context = &wire["conversationState"]["currentMessage"]["userInputMessage"]
                ["userInputMessageContext"];
            let names = context["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| {
                    tool.pointer("/toolSpecification/name")
                        .and_then(Value::as_str)
                })
                .collect::<Vec<_>>();
            if phase == 0 {
                assert_eq!(
                    names.len(),
                    DEFERRED_COUNT + 1,
                    "non-deferred tools remain available"
                );
            } else if phase <= 2 {
                assert_eq!(names, ["ToolSearch"]);
                assert!(request.body.len() < serde_json::to_vec(&tools(false)).unwrap().len() / 5);
            } else {
                assert_eq!(names, ["ToolSearch", upstream_name.as_str()]);
                let search_result = context["toolResults"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|result| result["toolUseId"] == "toolu_search");
                if phase == 3 {
                    let text = search_result.unwrap()["content"][0]["text"]
                        .as_str()
                        .unwrap();
                    assert!(text.contains("Matching tools:"));
                    assert!(text.contains("Available tool definitions:"));
                    assert!(text.contains(upstream_name.as_str()));
                    assert!(!text.contains("(empty result)"));
                }
            }
            let body = match phase {
                2 => event_stream_frame(
                    "toolUseEvent",
                    json!({
                        "toolUseId":"toolu_search", "name":"ToolSearch",
                        "input":json!({"query":DISCOVERED}).to_string(), "stop":true
                    }),
                ),
                3 => event_stream_frame(
                    "toolUseEvent",
                    json!({
                        "toolUseId":"toolu_discovered", "name":upstream_name,
                        "input":"{}", "stop":true
                    }),
                ),
                _ => generation_body("Hello."),
            };
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(body)
        })
        .expect(10)
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    for streaming in [false, true] {
        for phase in 0..5 {
            let messages = match phase {
                0 | 1 => json!([{"role":"user","content":"hi"}]),
                2 => json!([{"role":"user","content":"Find the notification tool."}]),
                3 => json!([
                    {"role":"user","content":"Find the notification tool."},
                    {"role":"assistant","content":[search_call()]},
                    {"role":"user","content":[references()]}
                ]),
                _ => json!([
                    {"role":"user","content":"Find the notification tool."},
                    {"role":"assistant","content":[search_call()]},
                    {"role":"user","content":[references()]},
                    {"role":"assistant","content":[{
                        "type":"tool_use","id":"toolu_discovered","name":DISCOVERED,"input":{}
                    }]},
                    {"role":"user","content":[{
                        "type":"tool_result","tool_use_id":"toolu_discovered","content":"Notification delivered."
                    }]}
                ]),
            };
            let response = reqwest::Client::new()
                .post(format!("http://127.0.0.1:{port}/v1/messages"))
                .bearer_auth(daemon.api_key.as_deref().unwrap())
                .header("user-agent", "claude-cli/2.1.300 (external, cli)")
                .header("anthropic-version", "2023-06-01")
                .json(&json!({
                    "model":"source-large","max_tokens":128,"stream":streaming,
                    "tools":tools(phase != 0),"messages":messages
                }))
                .send()
                .await
                .unwrap();
            let status = response.status();
            let trace = response.headers()["x-trace-id"]
                .to_str()
                .unwrap()
                .to_owned();
            let body = response.text().await.unwrap();
            assert_eq!(status, 200, "phase={phase} streaming={streaming}: {body}");
            if phase == 2 || phase == 3 {
                let expected = if phase == 2 { "ToolSearch" } else { DISCOVERED };
                let blocks = content(&body, streaming);
                assert!(
                    blocks
                        .iter()
                        .any(|block| block["type"] == "tool_use" && block["name"] == expected),
                    "{body}"
                );
                assert!(
                    !body.contains("server_tool_use"),
                    "client tools must remain client tools: {body}"
                );
                if phase == 3 {
                    assert!(
                        !body.contains(&alias),
                        "upstream aliases must be restored: {body}"
                    );
                }
            }
            let stats = expect_ok(
                daemon
                    .call("stats", json!({"detail":true,"recent":20}))
                    .await,
            );
            let record = stats["stats"]["recent_requests"]
                .as_array()
                .unwrap()
                .iter()
                .find(|record| record["trace_id"] == trace)
                .expect("request statistics");
            assert_eq!(
                record["diagnostics"]["loaded_tool_count"],
                if phase == 0 {
                    DEFERRED_COUNT + 1
                } else if phase <= 2 {
                    1
                } else {
                    2
                }
            );
            assert_eq!(
                record["diagnostics"]["deferred_tool_count"],
                if phase == 0 { 0 } else { DEFERRED_COUNT }
            );
            assert_eq!(
                record["diagnostics"]["tool_search_rounds"], 0,
                "the proxy must not execute the client's ToolSearch"
            );
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 10);
    daemon.stop().await;
}
