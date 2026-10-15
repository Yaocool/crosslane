use super::*;
use serde_json::{json, Value};

#[tokio::test]
async fn claude_search_domain_filters_cover_json_sse_parallel_and_pending_calls() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    for streaming in [false, true] {
        for scenario in ["allowed", "blocked", "path", "parallel", "pending"] {
            let mock = MockServer::start().await;
            let final_url = "https://docs.example.test/guide/rust/articles/one";
            let mut search = json!({"type":"web_search_20250305","name":"web_search"});
            let expected_query = match scenario {
                "blocked" => {
                    search["blocked_domains"] = json!(["untrusted.test", "example.test.evil.test"]);
                    "regression source -site:untrusted.test -site:example.test.evil.test"
                }
                "path" => {
                    search["allowed_domains"] = json!(["example.test/guide/*/articles"]);
                    "regression source (site:example.test/guide/*/articles)"
                }
                _ => {
                    search["allowed_domains"] = json!(["EXAMPLE.test"]);
                    "regression source (site:example.test)"
                }
            };
            Mock::given(method("POST"))
                .and(path("/mcp"))
                .respond_with(move |request: &wiremock::Request| {
                    let payload: Value = serde_json::from_slice(&request.body).unwrap();
                    assert_eq!(payload["params"]["arguments"]["query"], expected_query);
                    ResponseTemplate::new(200).set_body_json(json!({"result":{"content":[{
                        "type":"text","text":json!({"query":expected_query,"totalResults":300,"results":[
                            {"title":"Verified source","url":final_url,"snippet":"verified filtered snippet"},
                            {"title":"Untrusted","url":"https://sub.untrusted.test/source","snippet":"EXCLUDED_UNTRUSTED"},
                            {"title":"Lookalike","url":"https://example.test.evil.test/source","snippet":"EXCLUDED_LOOKALIKE"}
                        ]}).to_string()
                    }]}}))
                })
                .expect(1)
                .mount(&mock)
                .await;
            let rounds = Arc::new(AtomicUsize::new(0));
            let count = rounds.clone();
            Mock::given(method("POST"))
                .and(path("/generateAssistantResponse"))
                .respond_with(move |request: &wiremock::Request| {
                    let round = count.fetch_add(1, Ordering::SeqCst);
                    let body =
                        if round == 0 && scenario != "pending" {
                            let mut body = event_stream_frame(
                                "toolUseEvent",
                                json!({
                                    "toolUseId":"search_call","name":"web_search",
                                    "input":"{\"query\":\"regression source\"}","stop":true
                                }),
                            );
                            if scenario == "parallel" {
                                body.extend(event_stream_frame("toolUseEvent", json!({
                                "toolUseId":"discovery_call","name":"tool_search_tool_regex",
                                "input":"{\"pattern\":\"deferred_example\"}","stop":true
                            })));
                            }
                            body
                        } else {
                            let payload: Value = serde_json::from_slice(&request.body).unwrap();
                            let rendered = payload.to_string();
                            assert!(rendered.contains("verified filtered snippet"), "{payload}");
                            assert!(!rendered.contains("EXCLUDED_"), "{payload}");
                            assert!(!rendered.contains("untrusted.test"), "{payload}");
                            assert!(!rendered.contains("example.test.evil.test"), "{payload}");
                            generation_body(&format!("Verified result: {final_url}"))
                        };
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "application/vnd.amazon.eventstream")
                        .set_body_bytes(body)
                })
                .expect(if scenario == "pending" { 1 } else { 2 })
                .mount(&mock)
                .await;
            let port = unused_tcp_port();
            let daemon =
                Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri()))
                    .await;
            expect_ok(daemon.call("account.import",json!({"accounts":[{
                "id":"acc_77777777","email":"search@example.test","machine_id":"7".repeat(64),
                "credentials":{"access_token":"ksk_synthetic-search","region":"us-east-1","expires_at":0,"auth_method":"api_key"},"created_at":1
            }]})).await);
            let mut tools = vec![search];
            let mut messages = vec![json!({"role":"user","content":"search for the source"})];
            if scenario == "parallel" {
                tools.extend([
                    json!({"type":"tool_search_tool_regex_20251119","name":"tool_search_tool_regex"}),
                    json!({"name":"deferred_example","defer_loading":true,"input_schema":{"type":"object","properties":{}}}),
                ]);
            }
            if scenario == "pending" {
                tools.push(
                    json!({"name":"fixture_task","input_schema":{"type":"object","properties":{}}}),
                );
                messages.extend([
                    json!({"role":"assistant","content":[
                        {"type":"server_tool_use","id":"srvtoolu_pending","name":"web_search","input":{"query":"regression source"}},
                        {"type":"tool_use","id":"toolu_client","name":"fixture_task","input":{}}
                    ]}),
                    json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_client","content":"done"}]}),
                ]);
            }
            let response = reqwest::Client::new()
                .post(format!("http://127.0.0.1:{port}/v1/messages"))
                .bearer_auth(daemon.api_key.as_deref().unwrap())
                .header("user-agent", "claude-cli/2.1.300 (external, e2e)")
                .json(&json!({"model":"claude-sonnet-4","max_tokens":4096,"messages":messages,"stream":streaming,"tools":tools}))
                .send().await.unwrap();
            let status = response.status();
            let text = response.text().await.unwrap();
            assert_eq!(status, 200, "{scenario}/{streaming}: {text}");
            assert!(text.contains(final_url), "{scenario}/{streaming}: {text}");
            assert!(text.contains("web_search_tool_result"), "{text}");
            assert!(!text.contains("EXCLUDED_"), "{text}");
            assert!(!text.contains("untrusted.test"), "{text}");
            assert!(!text.contains("example.test.evil.test"), "{text}");
            assert!(!text.contains("event: error"), "{text}");
            mock.verify().await;
            daemon.stop().await;
        }
    }
}

#[tokio::test]
async fn failed_search_stops_generation_and_reports_unsupported_in_both_protocols() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    for route in ["responses", "messages"] {
        for streaming in [false, true] {
            let mock = MockServer::start().await;
            let name = if route == "responses" {
                "crosslane_web_search"
            } else {
                "web_search"
            };
            Mock::given(method("POST")).and(path("/generateAssistantResponse"))
                .respond_with(ResponseTemplate::new(200)
                    .insert_header("content-type", "application/vnd.amazon.eventstream")
                    .set_body_bytes(event_stream_frame("toolUseEvent", json!({"toolUseId":"search_call","name":name,"input":"{\"query\":\"failure source\"}","stop":true}))))
                .expect(1).mount(&mock).await;
            Mock::given(method("POST"))
                .and(path("/mcp"))
                .respond_with(
                    ResponseTemplate::new(400)
                        .set_body_json(json!({"message":"search backend unavailable"})),
                )
                .expect(1)
                .mount(&mock)
                .await;
            let port = unused_tcp_port();
            let daemon =
                Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri()))
                    .await;
            expect_ok(daemon.call("account.import",json!({"accounts":[{
                "id":"acc_77777777","email":"search@example.test","machine_id":"7".repeat(64),
                "credentials":{"access_token":"ksk_synthetic-search","region":"us-east-1","expires_at":0,"auth_method":"api_key"},"created_at":1
            }]})).await);
            let request = if route == "responses" {
                json!({"model":"claude-sonnet-4","input":"search for a source","stream":streaming,"tools":[{"type":"web_search"}]})
            } else {
                json!({"model":"claude-sonnet-4","max_tokens":4096,"messages":[{"role":"user","content":"search for a source"}],"stream":streaming,"tools":[{"type":"web_search_20250305","name":"web_search"}]})
            };
            let response = reqwest::Client::new()
                .post(format!("http://127.0.0.1:{port}/v1/{route}"))
                .bearer_auth(daemon.api_key.as_deref().unwrap())
                .header(
                    "user-agent",
                    if route == "messages" {
                        "claude-cli/2.1.296 (external, e2e)"
                    } else {
                        "codex_cli_rs/0.154.0"
                    },
                )
                .json(&request)
                .send()
                .await
                .unwrap();
            let status = response.status();
            if !streaming {
                assert_eq!(
                    response.headers()["x-crosslane-error-code"],
                    "unsupported_tool_protocol"
                );
                assert_eq!(response.headers()["x-crosslane-error-stage"], "web_search");
            }
            let text = response.text().await.unwrap();
            assert_eq!(status, if streaming { 200 } else { 400 }, "{route}: {text}");
            assert!(
                text.contains("web_search is not supported:"),
                "{route}: {text}"
            );
            if streaming {
                assert!(
                    text.contains("unsupported_tool_protocol"),
                    "{route}: {text}"
                );
            }
            assert!(!text.contains("response.completed"), "{text}");
            if streaming && route == "responses" {
                let failure: Value = text
                    .lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    .find(|event| event["type"] == "response.failed")
                    .unwrap();
                assert_eq!(failure["response"]["output"][0]["type"], "web_search_call");
                assert_eq!(failure["response"]["output"][0]["status"], "incomplete");
            }
            mock.verify().await;
            daemon.stop().await;
        }
    }
}

#[tokio::test]
async fn hosted_search_executes_replays_and_preserves_streaming_events() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    for streaming in [false, true] {
        let mock = MockServer::start().await;
        let rounds = Arc::new(AtomicUsize::new(0));
        let count = rounds.clone();
        Mock::given(method("POST")).and(path("/generateAssistantResponse"))
            .respond_with(move |request: &wiremock::Request| {
                let payload: Value = serde_json::from_slice(&request.body).unwrap();
                let round = count.fetch_add(1, Ordering::SeqCst);
                let body = if round == 0 {
                    let tools = payload.pointer("/conversationState/currentMessage/userInputMessage/userInputMessageContext/tools").unwrap().as_array().unwrap();
                    assert!(tools.iter().any(|tool| tool["toolSpecification"]["name"] == "crosslane_web_search"));
                    event_stream_frame("toolUseEvent", json!({"toolUseId":"search_call","name":"crosslane_web_search","input":"{\"query\":\"regression source\"}","stop":true}))
                } else {
                    assert!(payload.to_string().contains("verified search snippet"), "{payload}");
                    generation_body("Verified result: https://example.test/source")
                };
                ResponseTemplate::new(200).insert_header("content-type","application/vnd.amazon.eventstream").set_body_bytes(body)
            }).expect(3).mount(&mock).await;
        Mock::given(method("POST")).and(path("/mcp"))
            .respond_with(|request: &wiremock::Request| {
                let payload: Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(payload["params"]["arguments"]["query"], "regression source");
                ResponseTemplate::new(200).set_body_json(json!({"result":{"content":[{"type":"text","text":json!({"query":"regression source","totalResults":1,"results":[{"title":"Verified source","url":"https://example.test/source","snippet":"verified search snippet"}]}).to_string()}]}}))
            }).expect(1).mount(&mock).await;
        let port = unused_tcp_port();
        let daemon =
            Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
        expect_ok(daemon.call("account.import",json!({"accounts":[{
            "id":"acc_77777777","email":"search@example.test","machine_id":"7".repeat(64),
            "credentials":{"access_token":"ksk_synthetic-search","region":"us-east-1","expires_at":0,"auth_method":"api_key"},"created_at":1
        }]})).await);
        let client = reqwest::Client::new();
        let response = client.post(format!("http://127.0.0.1:{port}/v1/responses"))
            .bearer_auth(daemon.api_key.as_deref().unwrap())
            .header("user-agent","codex_cli_rs/0.154.0 (Mac OS 26.6; arm64) Terminal/1.0")
            .json(&json!({"model":"claude-sonnet-4","input":"search for the source","stream":streaming,"tools":[{"type":"web_search"}]}))
            .send().await.unwrap();
        assert_eq!(response.status(), 200);
        let text = response.text().await.unwrap();
        let response: Value = if streaming {
            let events = text
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .collect::<Vec<_>>();
            assert!(events
                .iter()
                .any(|event| event["type"] == "response.web_search_call.completed"));
            assert!(!text.contains("crosslane_web_search"), "{text}");
            for (index, event) in events.iter().enumerate() {
                assert_eq!(event["sequence_number"], index);
            }
            events.last().unwrap()["response"].clone()
        } else {
            serde_json::from_str(&text).unwrap()
        };
        let search = response["output"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "web_search_call")
            .unwrap();
        assert_eq!(
            search["action"]["sources"][0]["url"],
            "https://example.test/source"
        );
        let message = response["output"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "message")
            .unwrap();
        assert_eq!(
            message["content"][0]["annotations"][0]["type"],
            "url_citation"
        );
        let continuation = client.post(format!("http://127.0.0.1:{port}/v1/responses"))
            .bearer_auth(daemon.api_key.as_deref().unwrap())
            .header("user-agent","codex_cli_rs/0.154.0 (Mac OS 26.6; arm64) Terminal/1.0")
            .json(&json!({"model":"claude-sonnet-4","input":"repeat the verified snippet","previous_response_id":response["id"]}))
            .send().await.unwrap();
        let status = continuation.status();
        let body = continuation.text().await.unwrap();
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("Verified result"), "{body}");
        assert_eq!(rounds.load(Ordering::SeqCst), 3);
        mock.verify().await;
        daemon.stop().await;
    }
}
