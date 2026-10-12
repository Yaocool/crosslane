use super::*;
use axum::body::{Body, Bytes};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use futures::StreamExt;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::sync::atomic::AtomicBool;
use tokio::sync::Notify;

struct DelayedUpstream {
    url: String,
    release: Arc<Notify>,
    finished: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl DelayedUpstream {
    async fn start_with_failed_tail(fail_tail: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/generateAssistantResponse",
            listener.local_addr().unwrap()
        );
        let release = Arc::new(Notify::new());
        let finished = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let (gate, done, count) = (release.clone(), finished.clone(), calls.clone());
        let app = axum::Router::new()
            .route("/ListAvailableModels", get(|| async {
                let models = ["source-large", "mapped-small", "resolved-small", "resolved-tiny", "summary-large"]
                    .map(|id| json!({"modelId":id,"tokenLimits":{"maxInputTokens":1_000_000,"maxOutputTokens":16_384}}));
                axum::Json(json!({"models": models}))
            }))
            .route("/generateAssistantResponse", post(move || {
                let (gate, done, count) = (gate.clone(), done.clone(), count.clone());
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    let stream = async_stream::stream! {
                        yield Ok::<_, Infallible>(Bytes::from(event_stream_frame(
                            "assistantResponseEvent", json!({"content":"Visible answer begins now. "}),
                        )));
                        gate.notified().await;
                        done.store(true, Ordering::SeqCst);
                        let tail = if fail_tail { vec![0,0,0,32,0,0,0,0] }
                            else { generation_body("And ends here.") };
                        yield Ok::<_, Infallible>(Bytes::from(tail));
                    };
                    ([("content-type", "application/vnd.amazon.eventstream")], Body::from_stream(stream)).into_response()
                }
            }));
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            url,
            release,
            finished,
            calls,
            task,
        }
    }
}

impl Drop for DelayedUpstream {
    fn drop(&mut self) {
        self.release.notify_waiters();
        self.release.notify_one();
        self.task.abort();
    }
}

fn tool_result_request() -> Value {
    json!({"model":"source-large", "max_tokens":256, "stream":true,
    "tools":[{"name":"read_file","input_schema":{"type":"object","properties":{"path":{"type":"string"}}}}],
    "messages":[
        {"role":"user","content":"Read the file."},
        {"role":"assistant","content":[{"type":"tool_use","id":"call_read","name":"read_file","input":{"path":"README.md"}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_read","content":"File contents."}]}
    ]})
}

#[derive(Clone, Copy, Debug)]
enum WireProtocol {
    Anthropic,
    Chat,
    Responses,
}

impl WireProtocol {
    const ALL: [Self; 3] = [Self::Anthropic, Self::Chat, Self::Responses];

    fn route(self) -> &'static str {
        match self {
            Self::Anthropic => "/v1/messages",
            Self::Chat => "/v1/chat/completions",
            Self::Responses => "/v1/responses",
        }
    }

    fn user_agent(self) -> &'static str {
        match self {
            Self::Anthropic => "claude-cli/2.1.260 (external, e2e)",
            Self::Chat | Self::Responses => "codex_cli_rs/0.147.0 (e2e)",
        }
    }

    fn request(self, stream: bool) -> Value {
        let mut body = match self {
            Self::Anthropic => tool_result_request(),
            Self::Chat => json!({"model":"source-large","max_tokens":256,
            "tools":[{"type":"function","function":{"name":"read_file","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}}],
            "messages":[
                {"role":"user","content":"Read the file."},
                {"role":"assistant","content":null,"tool_calls":[{"id":"call_read","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"README.md\"}"}}]},
                {"role":"tool","tool_call_id":"call_read","content":"File contents."}
            ]}),
            Self::Responses => json!({"model":"source-large","max_output_tokens":256,"store":false,
            "tools":[{"type":"function","name":"read_file","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}],
            "input":[
                {"role":"user","content":"Read the file."},
                {"type":"function_call","id":"fc_read","call_id":"call_read","name":"read_file","arguments":"{\"path\":\"README.md\"}"},
                {"type":"function_call_output","call_id":"call_read","output":"File contents."}
            ]}),
        };
        body["stream"] = json!(stream);
        body
    }

    fn delta(self, event: &Value) -> Option<&str> {
        match self {
            Self::Anthropic => event.pointer("/delta/text").and_then(Value::as_str),
            Self::Chat => event
                .pointer("/choices/0/delta/content")
                .and_then(Value::as_str),
            Self::Responses if event["type"] == "response.output_text.delta" => {
                event["delta"].as_str()
            }
            Self::Responses => None,
        }
    }

    fn assert_terminal(self, body: &str, failed: bool) {
        match self {
            Self::Anthropic => {
                assert!(body.contains("message_start"), "{body}");
                assert_eq!(body.contains("message_stop"), !failed, "{body}");
            }
            Self::Chat => assert_eq!(body.contains("data: [DONE]"), !failed, "{body}"),
            Self::Responses => {
                assert!(body.contains("event: response.created\n"), "{body}");
                assert_eq!(
                    body.contains("event: response.completed\n"),
                    !failed,
                    "{body}"
                );
                assert_eq!(body.contains("event: response.failed\n"), failed, "{body}");
            }
        }
    }
}

fn sse_events(body: &str) -> Vec<Value> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[tokio::test]
async fn latency_tool_result_stream_releases_valid_text_before_upstream_eof() {
    assert_early_tool_result_text(false).await;
}

#[tokio::test]
async fn latency_tool_result_stream_does_not_replay_after_visible_text_and_failed_tail() {
    assert_early_tool_result_text(true).await;
}

async fn assert_early_tool_result_text(fail_tail: bool) {
    let _guard = HTTP_TEST_LOCK.lock().await;
    for protocol in WireProtocol::ALL {
        let upstream = DelayedUpstream::start_with_failed_tail(fail_tail).await;
        let port = unused_tcp_port();
        let daemon = Daemon::start_http(port, &upstream.url).await;
        import_context_alignment_account(&daemon, 0.0).await;
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            reqwest::Client::new()
                .post(format!("http://127.0.0.1:{port}{}", protocol.route()))
                .bearer_auth(daemon.api_key.as_deref().unwrap())
                .header("user-agent", protocol.user_agent())
                .json(&protocol.request(true))
                .send(),
        )
        .await
        .expect("headers must not wait for EOF")
        .unwrap();
        assert_eq!(response.status(), 200);
        let mut stream = response.bytes_stream();
        let mut output = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                output
                    .extend_from_slice(&stream.next().await.expect("content before EOF").unwrap());
                if String::from_utf8_lossy(&output)
                    .lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    .any(|event| {
                        protocol
                            .delta(&event)
                            .is_some_and(|text| text.contains("Visible answer"))
                    })
                {
                    break;
                }
            }
        })
        .await
        .expect("valid text must be delivered before releasing the upstream tail");
        assert!(!upstream.finished.load(Ordering::SeqCst));
        upstream.release.notify_one();
        while let Some(chunk) = stream.next().await {
            output.extend_from_slice(&chunk.unwrap());
        }
        let body = String::from_utf8(output).unwrap();
        let events = sse_events(&body);
        let text = events
            .iter()
            .filter_map(|event| protocol.delta(event))
            .collect::<String>();
        protocol.assert_terminal(&body, fail_tail);
        if fail_tail {
            assert!(body.contains("upstream_event_stream_corrupt"), "{body}");
            assert_eq!(text, "Visible answer begins now. ", "{body}");
        } else {
            assert_eq!(text, "Visible answer begins now. And ends here.", "{body}");
        }
        assert_eq!(upstream.calls.load(Ordering::SeqCst), 1);
        daemon.stop().await;
    }
}

#[tokio::test]
async fn latency_tool_calls_ignore_legacy_post_eof_delay() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST")).and(path("/generateAssistantResponse"))
        .respond_with(ResponseTemplate::new(200).insert_header("content-type", "application/vnd.amazon.eventstream")
            .set_body_bytes(event_stream_frame("toolUseEvent", json!({"toolUseId":"call_read","name":"read_file","input":"{\"path\":\"README.md\"}","stop":true}))))
        .mount(&mock).await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    model_policy::configure(&daemon, |config| {
        config.features.tool_call_buffer_delay_ms = 60_000;
    })
    .await;
    let mut request = tool_result_request();
    request["messages"] = json!([{"role":"user","content":"Read the file."}]);
    let body = tokio::time::timeout(Duration::from_secs(2), async {
        reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .bearer_auth(daemon.api_key.as_deref().unwrap())
            .header("user-agent", "claude-cli/2.1.260 (external, e2e)")
            .json(&request)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    })
    .await
    .expect("complete tool arguments must not wait for a legacy delay");
    assert!(body.contains("tool_use"), "{body}");
    assert!(body.contains("README.md"), "{body}");
    daemon.stop().await;
}
