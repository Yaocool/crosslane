//! 端到端测试：拉起真实 crosslaned，通过 Unix socket 走完整协议。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crosslane_core::config::{
    ApiKeyConfig, ApiKeyFormat, Config, ModelMappingRule, ProviderConfig, ProxyServiceConfig,
};
use crosslane_ipc::protocol::{decode_line, encode_line, Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

static HTTP_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn sse_error_payload(body: &str) -> serde_json::Value {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<serde_json::Value>(data).ok())
        .find(|event| event["type"] == "error")
        .expect("SSE error event")
}

#[path = "end_to_end/model_policy.rs"]
mod model_policy;

#[path = "end_to_end/copilot_model_aliases.rs"]
mod copilot_model_aliases;

#[path = "end_to_end/copilot_account_info.rs"]
mod copilot_account_info;

#[path = "end_to_end/responses.rs"]
mod responses;

#[path = "end_to_end/thinking_controls.rs"]
mod thinking_controls;

#[path = "end_to_end/compatibility_controls.rs"]
mod compatibility_controls;

#[path = "end_to_end/claude_gateway.rs"]
mod claude_gateway;

#[path = "end_to_end/crs_errors.rs"]
mod crs_errors;

#[path = "end_to_end/cache_controls.rs"]
mod cache_controls;

#[path = "end_to_end/protocol_parameters.rs"]
mod protocol_parameters;

#[path = "end_to_end/warning_regressions.rs"]
mod warning_regressions;

#[path = "end_to_end/manual_compaction.rs"]
mod manual_compaction;

#[path = "end_to_end/automatic_compaction.rs"]
mod automatic_compaction;

struct Daemon {
    child: Child,
    socket: PathBuf,
    home: tempfile::TempDir,
    api_key: Option<String>,
}

impl Daemon {
    async fn start() -> Self {
        let home = tempfile::tempdir().expect("tempdir");
        let socket = home.path().join("admin.sock");
        let child = Command::new(env!("CARGO_BIN_EXE_crosslaned"))
            .env("CROSSLANE_HOME", home.path())
            .env("CROSSLANE_HTTP_PORT", "0")
            .env("CROSSLANE_DISABLE_HTTP", "1")
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn crosslaned");
        wait_for_socket(&socket).await;
        Self {
            child,
            socket,
            home,
            api_key: None,
        }
    }

    async fn call(&self, method: &str, params: serde_json::Value) -> Response {
        rpc_call(&self.socket, method, params).await
    }

    async fn start_http(port: u16, upstream_url: &str) -> Self {
        let home = tempfile::tempdir().expect("tempdir");
        let socket = home.path().join("admin.sock");
        let child = Command::new(env!("CARGO_BIN_EXE_crosslaned"))
            .env("CROSSLANE_HOME", home.path())
            .env("CROSSLANE_CODEWHISPERER_URL", upstream_url)
            .env("CROSSLANE_AMAZONQ_URL", upstream_url)
            .env("CROSSLANE_RUNTIME_URL", upstream_url)
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn crosslaned with HTTP");
        wait_for_socket(&socket).await;
        let created = expect_ok(
            rpc_call(
                &socket,
                "service.create",
                serde_json::json!({"name":"e2e","host":"127.0.0.1","port":port}),
            )
            .await,
        );
        let api_key = created["api_key"]["key"]
            .as_str()
            .expect("created API key")
            .to_string();
        wait_for_http(port).await;
        Self {
            child,
            socket,
            home,
            api_key: Some(api_key),
        }
    }

    async fn start_copilot(port: u16, github_base: &str) -> Self {
        let home = tempfile::tempdir().expect("tempdir");
        let socket = home.path().join("admin.sock");
        let mut copilot = ProviderConfig {
            id: "copilot".into(),
            kind: "copilot".into(),
            ..ProviderConfig::default()
        };
        copilot.settings.insert(
            "github_api_base".into(),
            serde_json::Value::String(github_base.into()),
        );
        copilot.settings.insert(
            "oauth_base".into(),
            serde_json::Value::String(github_base.into()),
        );
        copilot
            .settings
            .insert("allow_insecure_http".into(), serde_json::Value::Bool(true));
        let mut config = Config::default();
        config.admin.socket = socket.to_string_lossy().into_owned();
        config.provider = vec![copilot];
        config.api_key = vec![ApiKeyConfig {
            id: Some("ak_copilot_e2e".into()),
            name: "copilot-e2e".into(),
            key: "copilot-e2e-secret".into(),
            format: ApiKeyFormat::Simple,
            enabled: true,
            skip_user_agent_check: true,
            credits_limit: None,
            allowed_providers: vec!["copilot".into()],
            allowed_models: vec!["copilot/*".into()],
        }];
        config.proxy_service = vec![ProxyServiceConfig {
            id: "svc_copilot_e2e".into(),
            name: "copilot-e2e".into(),
            host: "127.0.0.1".into(),
            port,
            enabled: true,
            skip_user_agent_check: true,
            api_key_ids: vec!["ak_copilot_e2e".into()],
            default_provider: "copilot".into(),
            allowed_providers: vec!["copilot".into()],
            created_at: 1,
            account_tag: None,
            account_ids: Vec::new(),
            excluded_account_ids: Vec::new(),
        }];
        config.model_mapping = vec![ModelMappingRule {
            name: "copilot-fast".into(),
            source_models: vec!["team-fast".into()],
            target_models: vec!["gpt-test".into()],
            providers: vec!["copilot".into()],
            service_ids: vec!["svc_copilot_e2e".into()],
            ..ModelMappingRule::default()
        }];
        config.validate().expect("valid Copilot e2e config");
        tokio::fs::write(
            home.path().join("config.toml"),
            toml::to_string_pretty(&config).expect("serialize Copilot config"),
        )
        .await
        .expect("write Copilot config");
        let child = Command::new(env!("CARGO_BIN_EXE_crosslaned"))
            .env("CROSSLANE_HOME", home.path())
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn crosslaned with Copilot");
        wait_for_socket(&socket).await;
        wait_for_http(port).await;
        Self {
            child,
            socket,
            home,
            api_key: Some("copilot-e2e-secret".into()),
        }
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    async fn stop(mut self) {
        let _kill_result = self.child.kill().await;
        let _wait_result = self.child.wait().await;
    }
}

async fn rpc_call(socket: &Path, method: &str, params: serde_json::Value) -> Response {
    let stream = UnixStream::connect(socket).await.expect("connect");
    let (read_half, mut write_half) = stream.into_split();
    let line = encode_line(&Request::new(1, method, params)).expect("encode");
    write_half.write_all(line.as_bytes()).await.expect("write");
    write_half.flush().await.expect("flush");
    let raw = BufReader::new(read_half)
        .lines()
        .next_line()
        .await
        .expect("read")
        .expect("response");
    decode_line(&raw).expect("decode")
}

async fn wait_for_socket(socket: &Path) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if socket.exists() && UnixStream::connect(socket).await.is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "crosslaned did not start: {}",
            socket.display()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_http(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(20);
    let client = reqwest::Client::new();
    loop {
        if client
            .get(format!("http://127.0.0.1:{port}/health"))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "crosslaned HTTP plane did not start"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn expect_ok(response: Response) -> serde_json::Value {
    match response {
        Response::Ok { result, .. } => result,
        Response::Err { error, .. } => panic!("RPC failed {}: {}", error.code, error.message),
    }
}

fn unused_tcp_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
    listener.local_addr().expect("local address").port()
}

fn event_stream_frame(event_type: &str, payload: serde_json::Value) -> Vec<u8> {
    let mut headers = Vec::new();
    for (name, value) in [
        (":message-type", "event"),
        (":event-type", event_type),
        (":content-type", "application/json"),
    ] {
        headers.push(name.len() as u8);
        headers.extend_from_slice(name.as_bytes());
        headers.push(7);
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    }
    let payload = serde_json::to_vec(&payload).expect("serialize event payload");
    let total_length = 16 + headers.len() + payload.len();
    let mut frame = Vec::with_capacity(total_length);
    frame.extend_from_slice(&(total_length as u32).to_be_bytes());
    frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    frame.extend_from_slice(&0u32.to_be_bytes());
    frame.extend_from_slice(&headers);
    frame.extend_from_slice(&payload);
    frame.extend_from_slice(&0u32.to_be_bytes());
    frame
}

fn generation_body(content: &str) -> Vec<u8> {
    let mut body = event_stream_frame(
        "assistantResponseEvent",
        serde_json::json!({"content":content}),
    );
    body.extend(event_stream_frame(
        "messageMetadataEvent",
        serde_json::json!({
            "messageMetadataEvent": {
                "usage": {
                    "inputTokens": 100,
                    "outputTokens": 20,
                    "creditsConsumed": 0.25
                }
            }
        }),
    ));
    body
}

#[derive(Clone)]
struct CompactionResponder;

impl wiremock::Respond for CompactionResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let payload =
            serde_json::from_slice::<serde_json::Value>(&request.body).unwrap_or_default();
        let current = payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
            .as_str()
            .unwrap_or_default();
        let content = if current.contains("durable conversation checkpoint") {
            "<summary>Task Overview: migrate compact to semantic Kiro summaries. Current State: implementation is active. Next Steps: verify the main response.</summary>"
        } else {
            "main request completed"
        };
        ResponseTemplate::new(200)
            .insert_header("content-type", "application/vnd.amazon.eventstream")
            .set_body_bytes(generation_body(content))
    }
}

#[derive(Clone, Default)]
struct UpstreamOverflowCompactionResponder {
    main_calls: Arc<AtomicUsize>,
    first_main_body_bytes: Arc<AtomicUsize>,
    summary_body_bytes: Arc<AtomicUsize>,
    rejection_message: Option<String>,
}

impl wiremock::Respond for UpstreamOverflowCompactionResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let payload =
            serde_json::from_slice::<serde_json::Value>(&request.body).unwrap_or_default();
        let current = payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
            .as_str()
            .unwrap_or_default();
        if current.contains("durable conversation checkpoint") {
            self.summary_body_bytes
                .store(request.body.len(), Ordering::SeqCst);
            return ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(generation_body(
                    "<summary>Task Overview: recover from the upstream context rejection. Current State: local preprocessing bounded the summary input. Next Steps: retry once.</summary>",
                ));
        }
        if self.main_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.first_main_body_bytes
                .store(request.body.len(), Ordering::SeqCst);
            ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "message": self.rejection_message.as_deref()
                    .unwrap_or("prompt is too long: context length exceeded")
            }))
        } else {
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(generation_body(
                    "main request completed after overflow retry",
                ))
        }
    }
}

#[derive(Clone)]
struct LongCompactionResponder;

impl wiremock::Respond for LongCompactionResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let payload =
            serde_json::from_slice::<serde_json::Value>(&request.body).unwrap_or_default();
        let current = payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
            .as_str()
            .unwrap_or_default();
        let content = if current.contains("durable conversation checkpoint") {
            format!(
                "<summary>{}</summary>",
                "durable checkpoint detail ".repeat(2_000)
            )
        } else {
            "main request completed".into()
        };
        ResponseTemplate::new(200)
            .insert_header("content-type", "application/vnd.amazon.eventstream")
            .set_body_bytes(generation_body(&content))
    }
}

#[derive(Clone)]
struct SlowCompactionResponder;

impl wiremock::Respond for SlowCompactionResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let payload =
            serde_json::from_slice::<serde_json::Value>(&request.body).unwrap_or_default();
        let current = payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
            .as_str()
            .unwrap_or_default();
        let summary = current.contains("durable conversation checkpoint");
        let content = if summary {
            "<summary>Task Overview: finish accounting after the caller timeout.</summary>"
        } else {
            "main request completed while summary accounting continued"
        };
        let response = ResponseTemplate::new(200)
            .insert_header("content-type", "application/vnd.amazon.eventstream")
            .set_body_bytes(generation_body(content));
        if summary {
            response.set_delay(Duration::from_millis(100))
        } else {
            response
        }
    }
}

#[derive(Clone)]
struct PartialFailureCompactionResponder;

impl wiremock::Respond for PartialFailureCompactionResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let payload =
            serde_json::from_slice::<serde_json::Value>(&request.body).unwrap_or_default();
        let current = payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
            .as_str()
            .unwrap_or_default();
        let body = if current.contains("durable conversation checkpoint") {
            let mut body = generation_body(
                "<summary>This summary is discarded because the stream ends malformed.</summary>",
            );
            // Preserve two complete events (including authoritative usage),
            // then force decode_eof to report a truncated trailing frame.
            body.extend_from_slice(&[0, 0, 0, 32, 0, 0, 0, 0]);
            body
        } else {
            generation_body("main request completed after the failed summary was accounted")
        };
        ResponseTemplate::new(200)
            .insert_header("content-type", "application/vnd.amazon.eventstream")
            .set_body_bytes(body)
    }
}

#[derive(Clone, Default)]
struct RetryingCompactionStreamResponder {
    main_calls: Arc<AtomicUsize>,
}

impl wiremock::Respond for RetryingCompactionStreamResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let payload =
            serde_json::from_slice::<serde_json::Value>(&request.body).unwrap_or_default();
        let current = payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
            .as_str()
            .unwrap_or_default();
        let body = if current.contains("durable conversation checkpoint") {
            generation_body(
                "<summary>Task Overview: preserve the compacted stream across a retry.</summary>",
            )
        } else if self.main_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            event_stream_frame(
                "toolUseEvent",
                serde_json::json!({
                    "toolUseEvent": {
                        "toolUseId":"broken-write",
                        "name":"write_file",
                        "input":"{\"path\":",
                        "stop":false
                    }
                }),
            )
        } else {
            generation_body("main request completed after retry")
        };
        ResponseTemplate::new(200)
            .insert_header("content-type", "application/vnd.amazon.eventstream")
            .set_body_bytes(body)
    }
}

#[derive(Clone, Default)]
struct ModelUnavailableFallbackResponder {
    stream_failure: bool,
}

impl wiremock::Respond for ModelUnavailableFallbackResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let payload =
            serde_json::from_slice::<serde_json::Value>(&request.body).unwrap_or_default();
        let model = payload["conversationState"]["currentMessage"]["userInputMessage"]["modelId"]
            .as_str()
            .unwrap_or_default();
        if model == "claude-opus-5" {
            if self.stream_failure {
                return ResponseTemplate::new(200)
                    .insert_header("content-type", "application/vnd.amazon.eventstream")
                    .set_body_bytes(event_stream_frame("error", serde_json::json!({
                        "__type":"MODEL_TEMPORARILY_UNAVAILABLE",
                        "message":"Encountered unexpectedly high load when processing the request, please try again."
                    })));
            }
            ResponseTemplate::new(500).set_body_json(serde_json::json!({
                "message": "Encountered unexpectedly high load when processing the request, please try again.",
                "reason": "MODEL_TEMPORARILY_UNAVAILABLE"
            }))
        } else if payload
            .pointer("/additionalModelRequestFields/top_k")
            .is_some()
        {
            ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "message":"top_k is not allowed by the fallback model"
            }))
        } else {
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(generation_body("completed with the fallback model"))
        }
    }
}

async fn mount_context_alignment_models(mock: &MockServer) {
    mount_context_alignment_models_with_tiny_limit(mock, 500).await;
}

async fn mount_context_alignment_models_with_tiny_limit(mock: &MockServer, tiny_limit: u32) {
    Mock::given(method("GET"))
        .and(path("/ListAvailableModels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models":[
                {
                    "modelId":"source-large",
                    "additionalModelRequestFieldsSchema":{"properties":{"output_config":{"properties":{
                        "effort":{"enum":["low","medium","high"]}
                    }}}},
                    "tokenLimits":{"maxInputTokens":1000000,"maxOutputTokens":16384}
                },
                {
                    "modelId":"mapped-small",
                    "tokenLimits":{"maxInputTokens":128000,"maxOutputTokens":16384}
                },
                {
                    "modelId":"resolved-small",
                    "tokenLimits":{"maxInputTokens":64000,"maxOutputTokens":16384}
                },
                {
                    "modelId":"resolved-tiny",
                    "tokenLimits":{"maxInputTokens":tiny_limit,"maxOutputTokens":16384}
                },
                {
                    "modelId":"summary-large",
                    "tokenLimits":{"maxInputTokens":1000000,"maxOutputTokens":16384}
                }
            ]
        })))
        .mount(mock)
        .await;
}

async fn import_context_alignment_account(daemon: &Daemon, used_credits: f64) {
    assert_eq!(
        expect_ok(
            daemon
                .call(
                    "account.import",
                    serde_json::json!({
                        "accounts": [{
                            "id": "acc_99999999",
                            "email": "alignment@example.com",
                            "machine_id": "9".repeat(64),
                            "credentials": {
                                "access_token": "alignment-token",
                                "region": "us-east-1",
                                "expires_at": 4_000_000_000i64,
                                "auth_method": "idc"
                            },
                            "usage": {
                                "current": used_credits,
                                "limit": 100.0,
                                "percent_used": used_credits,
                                "updated_at": 1
                            },
                            "subscription": {"kind": "pro"},
                            "created_at": 1
                        }]
                    }),
                )
                .await,
        )["imported"],
        1
    );
    let models = expect_ok(daemon.call("models", serde_json::json!({})).await);
    assert_eq!(models.as_array().map(Vec::len), Some(5));
}

async fn configure_context_alignment(daemon: &Daemon, summary_model: &str, mapping: &str) {
    let config_path = daemon.home().join("config.toml");
    let raw = tokio::fs::read_to_string(&config_path)
        .await
        .expect("read config");
    let edited = raw.replace("model_mapping = []", "").replace(
        "compaction_summary_model = \"\"",
        &format!("compaction_summary_model = \"{summary_model}\""),
    );
    let edited = format!("{edited}\n{mapping}\n");
    toml::from_str::<crosslane_core::config::Config>(&edited).expect("parse edited config");
    tokio::fs::write(&config_path, edited)
        .await
        .expect("write config");
    let reload = expect_ok(daemon.call("config.reload", serde_json::json!({})).await);
    assert_eq!(reload["applied"], true, "config reload failed: {reload}");
    let shown = expect_ok(daemon.call("config.show", serde_json::json!({})).await);
    assert_eq!(
        shown["effective_json"]["context"]["auto_compact_on_overflow"],
        true
    );
    assert!(!shown["effective_json"]["model_mapping"]
        .as_array()
        .expect("model mappings")
        .is_empty());
}

#[tokio::test]
async fn copilot_provider_routes_native_chat_models_mapping_and_usage_end_to_end() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .and(header("authorization", "Bearer github-e2e-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 7,
            "login": "copilot-e2e",
            "email": "copilot-e2e@example.com"
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/copilot_internal/v2/token"))
        .and(header("authorization", "token github-e2e-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "copilot-e2e-api-token",
            "expires_at": 4_000_000_000i64,
            "endpoints": {"api": upstream.uri()}
        })))
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("authorization", "Bearer copilot-e2e-api-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{
                "id": "gpt-test",
                "name": "GPT Test",
                "capabilities": {
                    "limits": {"max_prompt_tokens": 8192, "max_output_tokens": 1024},
                    "supported_endpoints": ["/chat/completions"]
                }
            }]
        })))
        .expect(2)
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer copilot-e2e-api-token"))
        .and(body_json(serde_json::json!({
            "model": "gpt-test",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": false
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-e2e",
            "object": "chat.completion",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
            "copilot_usage": {"total_nano_aiu": 12345, "cost_per_batch": 9}
        })))
        .expect(1)
        .mount(&upstream)
        .await;

    let port = unused_tcp_port();
    let daemon = Daemon::start_copilot(port, &upstream.uri()).await;
    let imported = expect_ok(
        daemon
            .call(
                "v2.account.importToken",
                serde_json::json!({
                    "provider": "copilot",
                    "token": "github-e2e-token",
                    "label": "e2e"
                }),
            )
            .await,
    );
    assert_eq!(imported["provider_id"], "copilot");
    assert_eq!(
        imported["supported_models"],
        serde_json::json!(["gpt-test"])
    );
    let refreshed = expect_ok(
        daemon
            .call(
                "tasks.run",
                serde_json::json!({
                    "name":"model_cache_refresh",
                    "provider":"copilot"
                }),
            )
            .await,
    );
    assert_eq!(refreshed["provider"], "copilot");
    assert!(refreshed["result"]
        .as_str()
        .is_some_and(|result| result.contains("copilot: 1 models")));
    let subscriptions = expect_ok(
        daemon
            .call("subscriptions", serde_json::json!({"provider":"copilot"}))
            .await,
    );
    assert_eq!(subscriptions["provider_id"], "copilot");
    assert_eq!(subscriptions["supported"], false);
    let account_id = imported["id"].as_str().expect("imported account ID");
    let tagged = expect_ok(
        daemon
            .call(
                "v2.account.tag",
                serde_json::json!({
                    "provider":"copilot",
                    "id":account_id,
                    "add":["e2e","team"],
                    "remove":[]
                }),
            )
            .await,
    );
    assert_eq!(tagged["tags"], serde_json::json!(["e2e", "team"]));
    let reset = expect_ok(
        daemon
            .call(
                "v2.account.resetHealth",
                serde_json::json!({"provider":"copilot","id":account_id}),
            )
            .await,
    );
    assert_eq!(reset["reset"], serde_json::json!([account_id]));
    let ready = expect_ok(
        daemon
            .call("status", serde_json::json!({"provider":"copilot"}))
            .await,
    );
    assert_eq!(ready["provider_scope"], "copilot");
    assert_eq!(ready["providers"][0]["id"], "copilot");
    assert_eq!(ready["providers"][0]["account_available"], 1);
    assert_eq!(
        ready["ready"], true,
        "Copilot status was not ready: {ready}"
    );

    let client = reqwest::Client::new();
    let service_ready = client
        .get(format!("http://127.0.0.1:{port}/ready"))
        .send()
        .await
        .expect("service readiness request");
    assert_eq!(service_ready.status(), reqwest::StatusCode::OK);
    let service_ready: serde_json::Value = service_ready.json().await.expect("readiness JSON");
    assert_eq!(
        service_ready["provider_scope"],
        serde_json::json!(["copilot"])
    );
    assert_eq!(service_ready["available_accounts"], 1);
    let service_health: serde_json::Value = client
        .get(format!("http://127.0.0.1:{port}/health"))
        .send()
        .await
        .expect("service health request")
        .json()
        .await
        .expect("health JSON");
    assert_eq!(service_health["total_accounts"], 1);
    assert_eq!(service_health["available_accounts"], 1);
    let catalog = client
        .get(format!("http://127.0.0.1:{port}/v1/models"))
        .bearer_auth(daemon.api_key.as_deref().expect("API key"))
        .send()
        .await
        .expect("model-list request");
    assert_eq!(catalog.status(), reqwest::StatusCode::OK);
    let catalog: serde_json::Value = catalog.json().await.expect("model-list JSON");
    assert_eq!(catalog["data"][0]["id"], "gpt-test");
    assert_eq!(catalog["data"][0]["owned_by"], "copilot");

    let response = client
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .bearer_auth(daemon.api_key.as_deref().expect("API key"))
        .header("user-agent", "codex_cli_rs/0.147.0 (e2e)")
        .json(&serde_json::json!({
            "model": "team-fast",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": false
        }))
        .send()
        .await
        .expect("Copilot chat request");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let response: serde_json::Value = response.json().await.expect("Copilot chat JSON");
    assert_eq!(response["choices"][0]["message"]["content"], "ok");

    let logs = expect_ok(
        daemon
            .call(
                "logs.follow",
                serde_json::json!({
                    "after_request_id": null,
                    "tail": 10,
                    "wait_ms": 0,
                    "provider": "copilot"
                }),
            )
            .await,
    );
    let entry = logs["entries"].as_array().unwrap().last().unwrap();
    assert_eq!(entry["provider_id"], "copilot");
    assert_eq!(entry["original_model"], "team-fast");
    assert_eq!(entry["model"], "gpt-test");
    assert_eq!(entry["model_mapping_rule"], "copilot-fast");
    assert_eq!(entry["user_agent"], "codex_cli_rs/0.147.0 (e2e)");

    let stats = expect_ok(
        daemon
            .call(
                "stats",
                serde_json::json!({
                    "detail":true,
                    "recent":10,
                    "provider":"copilot",
                    "by":"model"
                }),
            )
            .await,
    );
    assert_eq!(stats["stats"]["total"]["requests"], 1);
    assert_eq!(stats["grouped"]["gpt-test"]["requests"], 1);
    assert_eq!(
        stats["stats"]["recent_requests"][0]["provider_id"],
        "copilot"
    );

    let keys = expect_ok(
        daemon
            .call("apikey.list", serde_json::json!({"provider":"copilot"}))
            .await,
    );
    let history = keys[0]["usage"]["history"]
        .as_array()
        .expect("API key usage history");
    let usage = history.last().expect("Copilot usage record");
    assert_eq!(usage["provider_id"], "copilot");
    assert_eq!(usage["token_usage_source"], "provider");
    assert_eq!(usage["provider_billing"]["unit"], "nano_aiu");
    assert_eq!(usage["provider_billing"]["amount"], 12_345);
    assert_eq!(usage["provider_billing"]["raw"]["cost_per_batch"], 9);
    let services = expect_ok(
        daemon
            .call("service.list", serde_json::json!({"provider":"copilot"}))
            .await,
    );
    assert_eq!(services["services"].as_array().unwrap().len(), 1);
    assert!(matches!(
        daemon
            .call("service.list", serde_json::json!({"provider":"kiro"}))
            .await,
        Response::Err { .. }
    ));

    // A Chat-only upstream model is available through both other public APIs.
    for streaming in [false, true] {
        let reply = if streaming {
            ResponseTemplate::new(200).set_body_raw(
                "data: {\"id\":\"bridge\",\"model\":\"gpt-test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"adapted\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":6,\"completion_tokens\":2,\"prompt_tokens_details\":{\"cached_tokens\":2}},\"copilot_usage\":{\"total_nano_aiu\":55}}\n\ndata: [DONE]\n\n",
                "text/event-stream")
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id":"bridge","model":"gpt-test","choices":[{"message":{"role":"assistant","content":"adapted"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":6,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":2}},
                "copilot_usage":{"total_nano_aiu":55}
            }))
        };
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"stream":streaming,"max_completion_tokens":64}),
            ))
            .respond_with(reply)
            .expect(2)
            .mount(&upstream)
            .await;
        for endpoint in ["messages", "responses"] {
            let body = if endpoint == "messages" {
                serde_json::json!({"model":"gpt-test","messages":[{"role":"user","content":"hello"}],"max_tokens":64,"stream":streaming})
            } else {
                serde_json::json!({"model":"gpt-test","input":"hello","max_output_tokens":64,"stream":streaming})
            };
            let response = client
                .post(format!("http://127.0.0.1:{port}/v1/{endpoint}"))
                .bearer_auth(daemon.api_key.as_deref().unwrap())
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            let text = response.text().await.unwrap();
            assert!(text.contains("adapted"), "{endpoint}: {text}");
            assert!(text.contains("total_nano_aiu"));
            if streaming {
                assert!(text.contains(if endpoint == "messages" {
                    "message_stop"
                } else {
                    "response.completed"
                }));
            }
        }
    }
    // Token counting is explicit local estimation on non-Messages models.
    let counted = client.post(format!("http://127.0.0.1:{port}/v1/messages/count_tokens"))
        .bearer_auth(daemon.api_key.as_deref().unwrap())
        .json(&serde_json::json!({"model":"gpt-test","messages":[{"role":"user","content":"estimate input"}]}))
        .send().await.unwrap();
    assert_eq!(counted.status(), reqwest::StatusCode::OK);
    assert_eq!(
        counted.headers()["x-crosslane-token-count-source"],
        "estimate"
    );
    let counted: serde_json::Value = counted.json().await.unwrap();
    assert_eq!(counted["estimated"], true);
    assert!(counted["input_tokens"].as_u64().unwrap() > 0);
    let keys = expect_ok(
        daemon
            .call("apikey.list", serde_json::json!({"provider":"copilot"}))
            .await,
    );
    let history = keys[0]["usage"]["history"].as_array().unwrap();
    let adapted = history
        .iter()
        .filter(|record| record["provider_billing"]["amount"] == 55)
        .count();
    assert_eq!(adapted, 4);
    assert_eq!(history.last().unwrap()["token_usage_source"], "unreported");

    // The same service account commands persist qualified Copilot membership.
    let service_id = services["services"][0]["id"].as_str().unwrap();
    let removed = expect_ok(
        daemon
            .call(
                "service.account.remove",
                serde_json::json!({
                    "service":service_id,"accounts":[account_id],"provider":"copilot"
                }),
            )
            .await,
    );
    assert_eq!(removed["provider_accounts"], serde_json::json!([]));
    let health: serde_json::Value = client
        .get(format!("http://127.0.0.1:{port}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["total_accounts"], 0);
    let blocked = client.post(format!("http://127.0.0.1:{port}/v1/messages"))
        .bearer_auth(daemon.api_key.as_deref().unwrap())
        .json(&serde_json::json!({"model":"gpt-test","messages":[{"role":"user","content":"excluded"}],"max_tokens":64}))
        .send().await.unwrap();
    assert_eq!(blocked.status(), reqwest::StatusCode::BAD_REQUEST);
    let added = expect_ok(
        daemon
            .call(
                "service.account.add",
                serde_json::json!({
                    "service":service_id,"accounts":[format!("copilot/{account_id}")]
                }),
            )
            .await,
    );
    assert_eq!(added["provider_accounts"][0]["id"], account_id);
    let stored = std::fs::read_to_string(daemon.home.path().join("config.toml")).unwrap();
    assert!(stored.contains(&format!("copilot/{account_id}")));

    upstream.verify().await;
    daemon.stop().await;
}

#[tokio::test]
async fn dotenv_is_loaded_before_daemon_startup_configuration() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let home = workspace.path().join("dotenv-home");
    let socket = home.join("admin.sock");
    std::fs::write(
        workspace.path().join(".env"),
        format!(
            "CROSSLANE_HOME={}\nCROSSLANE_HTTP_PORT=0\nCROSSLANE_DISABLE_HTTP=1\nRUST_LOG=warn\n",
            home.display()
        ),
    )
    .expect("write .env");

    let mut child = Command::new(env!("CARGO_BIN_EXE_crosslaned"))
        .current_dir(workspace.path())
        .env_remove("CROSSLANE_HOME")
        .env_remove("CROSSLANE_HTTP_PORT")
        .env_remove("CROSSLANE_DISABLE_HTTP")
        .env_remove("RUST_LOG")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn crosslaned from .env");

    wait_for_socket(&socket).await;
    assert!(home.join("config.toml").exists());
    assert!(home.join("accounts.json").exists());

    let _kill_result = child.kill().await;
    let _wait_result = child.wait().await;
}

#[tokio::test]
async fn compact_model_alias_runs_through_translation_pool_and_mock_upstream() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    let mut upstream_body = event_stream_frame(
        "assistantResponseEvent",
        serde_json::json!({"content":"hello through the complete Rust chain"}),
    );
    upstream_body.extend(event_stream_frame(
        "messageMetadataEvent",
        serde_json::json!({
            "messageMetadataEvent": {
                "usage": {
                    "inputTokens": 11,
                    "outputTokens": 7,
                    "creditsConsumed": 0.25
                }
            }
        }),
    ));
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(upstream_body),
        )
        .mount(&mock)
        .await;

    let port = unused_tcp_port();
    let upstream_url = format!("{}/generateAssistantResponse", mock.uri());
    let daemon = Daemon::start_http(port, &upstream_url).await;
    assert_eq!(
        expect_ok(
            daemon
                .call(
                    "account.import",
                    serde_json::json!({
                        "accounts": [{
                            "id": "acc_77777777",
                            "email": "wiremock@example.com",
                            "machine_id": "7".repeat(64),
                            "credentials": {
                                "access_token": "wiremock-token",
                                "region": "us-east-1",
                                "expires_at": 4_000_000_000i64,
                                "auth_method": "idc"
                            },
                            "usage": {
                                "current": 0.0,
                                "limit": 100.0,
                                "percent_used": 0.0,
                                "updated_at": 1
                            },
                            "subscription": {"kind": "pro"},
                            "created_at": 1
                        }]
                    }),
                )
                .await,
        )["imported"],
        1
    );

    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .header("user-agent", "codex_cli_rs/0.147.0 (e2e)")
        .bearer_auth(daemon.api_key.as_deref().expect("service API key"))
        .json(&serde_json::json!({
            "model": "opus5",
            "messages": [{"role":"user", "content":"exercise every layer"}],
            "max_tokens": 64,
            "stream": false
        }))
        .send()
        .await
        .expect("call Rust proxy");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("OpenAI response JSON");
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "hello through the complete Rust chain"
    );
    assert_eq!(body["usage"]["prompt_tokens"], 11);
    assert_eq!(body["usage"]["completion_tokens"], 7);

    let received = mock.received_requests().await.expect("received requests");
    assert!(!received.is_empty());
    let generation_payloads = received
        .iter()
        .filter_map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).ok())
        .filter(|payload| payload.get("conversationState").is_some())
        .collect::<Vec<_>>();
    assert_eq!(generation_payloads.len(), 1);
    for payload in generation_payloads {
        assert_eq!(
            payload["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
            "claude-opus-5"
        );
        assert!(
            payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
                .as_str()
                .is_some_and(|content| content.ends_with("exercise every layer"))
        );
    }
    daemon.stop().await;
}

#[tokio::test]
async fn claude_document_blocks_reach_the_native_kiro_wire_format() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(generation_body("document received")),
        )
        .mount(&mock)
        .await;

    let port = unused_tcp_port();
    let upstream_url = format!("{}/generateAssistantResponse", mock.uri());
    let daemon = Daemon::start_http(port, &upstream_url).await;
    import_context_alignment_account(&daemon, 0.0).await;

    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header(
            "x-api-key",
            daemon.api_key.as_deref().expect("service API key"),
        )
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", "claude-cli/2.1.0 (external, e2e)")
        .json(&serde_json::json!({
            "model":"source-large",
            "max_tokens":64,
            "top_k":42,
            "stream":false,
            "messages":[{"role":"user","content":[
                {
                    "type":"document",
                    "title":"guide.pdf",
                    "context":"Use the approved architecture requirements.",
                    "citations":{"enabled":false},
                    "source":{
                        "type":"base64",
                        "media_type":"application/pdf",
                        "data":"JVBERi0xLjQKJSVFT0YK"
                    }
                },
                {"type":"text","text":"Summarize the guide."}
            ]}]
        }))
        .send()
        .await
        .expect("call Claude document path");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let response_body: serde_json::Value = response.json().await.expect("Claude response JSON");
    assert_eq!(response_body["content"][0]["text"], "document received");

    let received = mock.received_requests().await.expect("received requests");
    assert_eq!(
        received
            .iter()
            .filter(|request| request.url.path() == "/generateAssistantResponse")
            .count(),
        1
    );
    let payload = received
        .iter()
        .filter(|request| request.url.path() == "/generateAssistantResponse")
        .find_map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).ok())
        .expect("Kiro generation payload");
    let current = &payload["conversationState"]["currentMessage"]["userInputMessage"];
    assert_eq!(current["documents"][0]["format"], "pdf");
    assert_eq!(current["documents"][0]["name"], "guide");
    assert_eq!(
        current["documents"][0]["source"]["bytes"],
        "JVBERi0xLjQKJSVFT0YK"
    );
    assert!(current["documents"][0].get("context").is_none());
    assert!(current["content"]
        .as_str()
        .expect("user text")
        .contains("Use the approved architecture requirements."));
    assert_eq!(current["documents"][0]["citations"]["enabled"], false);
    assert!(payload
        .pointer("/additionalModelRequestFields/top_k")
        .is_none());

    daemon.stop().await;
}

#[tokio::test]
async fn openai_model_controls_follow_reference_omission_rules_through_http() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(generation_body("reference controls accepted")),
        )
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    let cases = [
        (serde_json::json!({}), serde_json::Value::Null),
        (
            serde_json::json!({"max_completion_tokens":64}),
            serde_json::json!({"maxTokens":64}),
        ),
        (
            serde_json::json!({"temperature":0,"top_p":0}),
            serde_json::json!({"temperature":0.0,"topP":0.0}),
        ),
        (
            serde_json::json!({"max_tokens":64,"max_completion_tokens":128}),
            serde_json::json!({"maxTokens":128}),
        ),
    ];
    for (controls, _) in &cases {
        let mut request = serde_json::json!({
            "model":"source-large", "messages":[{"role":"user","content":"explain"}]
        });
        request
            .as_object_mut()
            .unwrap()
            .extend(controls.as_object().unwrap().clone());
        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
            .header("user-agent", "codex_cli_rs/0.147.0 (e2e)")
            .header("x-api-key", daemon.api_key.as_deref().unwrap())
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(
            body["choices"][0]["message"]["content"],
            "reference controls accepted"
        );
    }
    let payloads = mock
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/generateAssistantResponse")
        .map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(payloads.len(), cases.len());
    for (payload, (_, expected)) in payloads.iter().zip(&cases) {
        assert_eq!(&payload["inferenceConfig"], expected);
    }
    daemon.stop().await;
}

#[tokio::test]
async fn openai_omitted_output_limits_do_not_report_false_truncation() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    let mut body = event_stream_frame(
        "assistantResponseEvent",
        serde_json::json!({
            "content":"token ".repeat(9000)
        }),
    );
    body.extend(event_stream_frame("messageMetadataEvent", serde_json::json!({
        "messageMetadataEvent":{"usage":{"inputTokens":100,"outputTokens":9000,"creditsConsumed":0.25}}
    })));
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(body),
        )
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    for stream in [false, true] {
        for (controls, expected) in [
            (serde_json::json!({}), "stop"),
            (serde_json::json!({"max_completion_tokens":64}), "length"),
            (serde_json::json!({"max_tokens":8192}), "length"),
        ] {
            let mut request = serde_json::json!({
                "model":"source-large", "stream":stream,
                "messages":[{"role":"user","content":"explain"}]
            });
            request
                .as_object_mut()
                .unwrap()
                .extend(controls.as_object().unwrap().clone());
            let response = reqwest::Client::new()
                .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
                .header("user-agent", "codex_cli_rs/0.147.0 (e2e)")
                .header("x-api-key", daemon.api_key.as_deref().unwrap())
                .json(&request)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            let text = response.text().await.unwrap();
            let finish = if stream {
                text.lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                    .filter_map(|event| {
                        event["choices"][0]["finish_reason"]
                            .as_str()
                            .map(str::to_owned)
                    })
                    .next_back()
                    .expect("finish reason")
            } else {
                serde_json::from_str::<serde_json::Value>(&text).unwrap()["choices"][0]
                    ["finish_reason"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            };
            assert_eq!(finish, expected, "stream={stream}, controls={controls}");
        }
    }
    let requests = mock.received_requests().await.unwrap();
    let payloads = requests
        .iter()
        .filter(|request| request.url.path() == "/generateAssistantResponse")
        .map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(payloads.len(), 6);
    for (index, payload) in payloads.iter().enumerate() {
        let limit = payload
            .pointer("/inferenceConfig/maxTokens")
            .and_then(serde_json::Value::as_u64);
        assert_eq!(limit, [None, Some(64), Some(8192)][index % 3]);
    }
    daemon.stop().await;
}

#[tokio::test]
async fn openai_internal_continuations_only_apply_explicit_output_limits() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(|request: &wiremock::Request| {
            let payload: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let continuation = payload["conversationState"]["currentMessage"]["userInputMessage"]
                ["content"]
                .as_str()
                .is_some_and(|content| content.contains("Continue with the next step."));
            let mut body = event_stream_frame(
                "assistantResponseEvent",
                serde_json::json!({"content":if continuation {
                    "continuation completed".to_owned()
                } else {
                    "token ".repeat(9000)
                }}),
            );
            if !continuation {
                body.extend(event_stream_frame(
                    "toolUseEvent",
                    serde_json::json!({
                        "toolUseId":"internal-step", "name":"internal_step",
                        "input":"{}", "stop":true
                    }),
                ));
            }
            body.extend(event_stream_frame(
                "messageMetadataEvent",
                serde_json::json!({
                    "messageMetadataEvent":{"usage":{
                        "inputTokens":100, "outputTokens":if continuation {100} else {9000},
                        "creditsConsumed":0.25
                    }}
                }),
            ));
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(body)
        })
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    let config_path = daemon.home().join("config.toml");
    let mut config: crosslane_core::config::Config =
        toml::from_str(&tokio::fs::read_to_string(&config_path).await.unwrap()).unwrap();
    config.features.auto_continue_rounds = 1;
    tokio::fs::write(&config_path, toml::to_string(&config).unwrap())
        .await
        .unwrap();
    assert_eq!(
        expect_ok(daemon.call("config.reload", serde_json::json!({})).await)["applied"],
        true
    );
    for stream in [false, true] {
        for maximum in [None, Some(10_000)] {
            let mut request = serde_json::json!({
                "model":"source-large", "stream":stream,
                "messages":[{"role":"user","content":"complete the task"}]
            });
            if let Some(maximum) = maximum {
                request["max_tokens"] = serde_json::json!(maximum);
            }
            let response = reqwest::Client::new()
                .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
                .header("user-agent", "codex_cli_rs/0.147.0 (e2e)")
                .header("x-api-key", daemon.api_key.as_deref().unwrap())
                .json(&request)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            let body = response.text().await.unwrap();
            assert!(
                body.contains("continuation completed"),
                "continuation was skipped: stream={stream}, maximum={maximum:?}, response tail={:?}",
                body.get(body.len().saturating_sub(500)..)
            );
            let finish = if stream {
                body.lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                    .filter_map(|event| {
                        event["choices"][0]["finish_reason"]
                            .as_str()
                            .map(str::to_owned)
                    })
                    .next_back()
                    .expect("finish reason")
            } else {
                serde_json::from_str::<serde_json::Value>(&body).unwrap()["choices"][0]
                    ["finish_reason"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            };
            assert_eq!(finish, "stop", "stream={stream}, maximum={maximum:?}");
        }
    }
    let requests = mock.received_requests().await.unwrap();
    let payloads = requests
        .iter()
        .filter(|request| request.url.path() == "/generateAssistantResponse")
        .map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        payloads.len(),
        8,
        "each response requires two upstream rounds"
    );
    for (index, payload) in payloads.iter().enumerate() {
        let expected = match index % 4 {
            2 => Some(10_000),
            3 => Some(1_000),
            _ => None,
        };
        assert_eq!(
            payload
                .pointer("/inferenceConfig/maxTokens")
                .and_then(serde_json::Value::as_u64),
            expected,
            "unexpected budget in generation {index}"
        );
    }
    daemon.stop().await;
}

#[tokio::test]
async fn account_conditional_larger_window_is_selected_before_context_validation() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(CompactionResponder)
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 50.0).await;
    configure_context_alignment(
        &daemon,
        "summary-large",
        r#"
[[model_mapping]]
name = "account-large"
type = "replace"
source_models = ["source-large"]
target_models = ["source-large"]
max_remaining_credit_percent = 90.0
priority = 5

[[model_mapping]]
name = "provisional-tiny"
type = "replace"
source_models = ["source-large"]
target_models = ["resolved-tiny"]
priority = 10
"#,
    )
    .await;
    let content = "input ".repeat(2000).trim_end().to_owned();
    for endpoint in ["/v1/messages", "/v1/chat/completions"] {
        let user_agent = if endpoint == "/v1/messages" {
            "claude-cli/2.1.235 (external, e2e)"
        } else {
            "codex_cli_rs/0.147.0 (e2e)"
        };
        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}{endpoint}"))
            .header("x-api-key", daemon.api_key.as_deref().unwrap())
            .header("anthropic-version", "2023-06-01")
            .header("user-agent", user_agent)
            .json(&serde_json::json!({
                "model":"source-large", "max_tokens":64,
                "messages":[{"role":"user","content":content}]
            }))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(status, reqwest::StatusCode::OK, "{endpoint}: {body}");
        assert!(body.get("context_management").is_none());
        if endpoint == "/v1/messages" {
            assert_eq!(body["content"][0]["type"], "text");
        }
    }
    let requests = mock.received_requests().await.unwrap();
    let payloads = requests
        .iter()
        .filter(|request| request.url.path() == "/generateAssistantResponse")
        .map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(payloads.len(), 2, "neither valid request needs a summary");
    for payload in payloads {
        let current = &payload["conversationState"]["currentMessage"]["userInputMessage"];
        assert_eq!(current["modelId"], "source-large");
        assert_eq!(
            current["content"], content,
            "input must not be prematurely compacted"
        );
    }
    daemon.stop().await;
}

#[tokio::test]
async fn temporarily_unavailable_model_falls_back_to_lower_same_family_model() {
    assert_model_fallback_controls(false, false, false).await;
}

#[tokio::test]
async fn streaming_http_fallback_rebuilds_model_controls() {
    assert_model_fallback_controls(true, false, false).await;
}

#[tokio::test]
async fn streaming_event_fallback_rebuilds_model_controls() {
    assert_model_fallback_controls(true, true, false).await;
}

#[tokio::test]
async fn disabled_models_never_receive_capacity_or_stream_fallback_requests() {
    for (stream, stream_failure) in [(false, false), (true, false), (true, true)] {
        assert_model_fallback_controls(stream, stream_failure, true).await;
    }
}

async fn assert_model_fallback_controls(
    stream: bool,
    stream_failure: bool,
    disable_fallback: bool,
) {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ListAvailableModels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [
                {
                    "modelId": "claude-opus-5",
                    "tokenLimits": {"maxInputTokens": 1000000, "maxOutputTokens": 64000},
                    "additionalModelRequestFieldsSchema":{"properties":{
                        "top_k":{"type":"integer"},
                        "output_config":{"properties":{"effort":{"enum":["low","medium"]}}}
                    }}
                },
                {
                    "modelId": "claude-opus-4.8",
                    "tokenLimits": {"maxInputTokens": 1000000, "maxOutputTokens": 64000},
                    "additionalModelRequestFieldsSchema":{"properties":{
                        "reasoning":{"properties":{"effort":{"enum":["low","high","xhigh"]}}}
                    }}
                }
            ]
        })))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(ModelUnavailableFallbackResponder { stream_failure })
        .mount(&mock)
        .await;

    let port = unused_tcp_port();
    let upstream_url = format!("{}/generateAssistantResponse", mock.uri());
    let daemon = Daemon::start_http(port, &upstream_url).await;
    assert_eq!(
        expect_ok(
            daemon
                .call(
                    "account.import",
                    serde_json::json!({
                        "accounts": [{
                            "id": "acc_66666666",
                            "email": "fallback@example.com",
                            "machine_id": "6".repeat(64),
                            "credentials": {
                                "access_token": "fallback-token",
                                "region": "us-east-1",
                                "expires_at": 4_000_000_000i64,
                                "auth_method": "idc"
                            },
                            "usage": {
                                "current": 0.0,
                                "limit": 100.0,
                                "percent_used": 0.0,
                                "updated_at": 1
                            },
                            "subscription": {"kind": "pro"},
                            "created_at": 1
                        }]
                    }),
                )
                .await,
        )["imported"],
        1
    );
    let models = expect_ok(daemon.call("models", serde_json::json!({})).await);
    assert_eq!(models.as_array().map(Vec::len), Some(2));

    if disable_fallback {
        model_policy::configure(&daemon, |config| {
            config.models.disabled = vec!["claude-opus-4.8".into()]
        })
        .await;
    }

    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header(
            "x-api-key",
            daemon.api_key.as_deref().expect("service API key"),
        )
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", "claude-cli/2.1.0 (external, e2e)")
        .json(&serde_json::json!({
            "model": "claude-opus-5",
            "max_tokens": 64000,
            "top_k":42,
            "thinking":{"type":"enabled","budget_tokens":32000,"display":"omitted"},
            "output_config":{"effort":"xhigh"},
            "stream": stream,
            "messages": [{"role": "user", "content": "exercise model fallback"}]
        }))
        .send()
        .await
        .expect("call Claude fallback path");
    if disable_fallback {
        let body = response.text().await.unwrap();
        assert!(
            !body.contains("completed with the fallback model"),
            "{body}"
        );
        let requests = mock.received_requests().await.unwrap();
        let generated = requests
            .iter()
            .filter(|request| request.method.as_str() == "POST")
            .collect::<Vec<_>>();
        assert!(!generated.is_empty());
        for request in generated {
            let payload: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(
                payload["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
                "claude-opus-5"
            );
        }
        daemon.stop().await;
        return;
    }
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body = response.text().await.expect("Claude response body");
    assert!(body.contains("completed with the fallback model"), "{body}");

    let generated_payloads = mock
        .received_requests()
        .await
        .expect("received requests")
        .iter()
        .filter(|request| request.url.path() == "/generateAssistantResponse")
        .filter_map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).ok())
        .collect::<Vec<_>>();
    let generated_models = generated_payloads
        .iter()
        .filter_map(|payload| {
            payload["conversationState"]["currentMessage"]["userInputMessage"]["modelId"]
                .as_str()
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    assert_eq!(generated_models, vec!["claude-opus-5", "claude-opus-4.8"]);
    for payload in &generated_payloads {
        assert!(payload
            .pointer("/additionalModelRequestFields/top_k")
            .is_none());
        assert!(payload.get("modelRequestIntent").is_none());
    }
    assert_eq!(
        generated_payloads[0]["additionalModelRequestFields"],
        serde_json::json!({
            "thinking":{"type":"adaptive","display":"summarized"},"output_config":{"effort":"medium"}
        })
    );
    assert_eq!(
        generated_payloads[1]["additionalModelRequestFields"],
        serde_json::json!({
            "reasoning":{"effort":"xhigh"}
        })
    );

    daemon.stop().await;
}

#[tokio::test]
async fn mcp_tool_inputs_are_consistent_across_http_protocols_and_buffering_modes() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    let config_path = daemon.home().join("config.toml");
    let mut config: crosslane_core::config::Config =
        toml::from_str(&tokio::fs::read_to_string(&config_path).await.unwrap()).unwrap();
    config.upstream.max_retries = 0;
    config.features.enable_model_fallback = false;
    config.features.tool_call_buffer_delay_ms = 0;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    let name = "mcp__relayer__memory_list_editable_atoms";

    for buffered in [false, true] {
        config.features.buffer_tool_calls = buffered;
        tokio::fs::write(&config_path, toml::to_string(&config).unwrap())
            .await
            .unwrap();
        assert_eq!(
            expect_ok(daemon.call("config.reload", serde_json::json!({})).await)["applied"],
            true
        );
        for (input, stop, expected) in [
            ("", false, Some(serde_json::json!({}))),
            (" \n\t", false, Some(serde_json::json!({}))),
            (" \n\t", true, Some(serde_json::json!({}))),
            (
                r#" {"query":"editable","raw":"keep"} "#,
                false,
                Some(serde_json::json!({"query":"editable","raw":"keep"})),
            ),
            (r#"{"query":"unfinished"#, false, None),
            (r#"{"query":"editable",}"#, true, None),
        ] {
            // Emit a real frame sequence with fragmented arguments, including
            // clean EOF without tool stop (the reported production failure).
            let mut upstream_body = event_stream_frame(
                "toolUseEvent",
                serde_json::json!({
                    "toolUseId":"call", "name":name, "input":"", "stop":false
                }),
            );
            let (first, second) = input.split_at(input.len() / 2);
            for fragment in [first, second] {
                upstream_body.extend(event_stream_frame(
                    "toolUseEvent",
                    serde_json::json!({
                        "toolUseId":"call", "name":name, "input":fragment, "stop":false
                    }),
                ));
            }
            if stop {
                upstream_body.extend(event_stream_frame(
                    "toolUseEvent",
                    serde_json::json!({
                        "toolUseId":"call", "name":name, "stop":true
                    }),
                ));
            }
            let _fixture = Mock::given(method("POST"))
                .and(path("/generateAssistantResponse"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "application/vnd.amazon.eventstream")
                        .set_body_bytes(upstream_body),
                )
                .mount_as_scoped(&mock)
                .await;

            for claude in [false, true] {
                for stream in [false, true] {
                    let route = if claude {
                        "messages"
                    } else {
                        "chat/completions"
                    };
                    let mut request = serde_json::json!({
                        "model":"source-large", "max_tokens":256, "stream":stream,
                        "messages":[{"role":"user","content":"Use the provided tool."}]
                    });
                    request["tools"] = if claude {
                        serde_json::json!([{"name":name,"description":"List editable atoms","input_schema":{"type":"object"}}])
                    } else {
                        serde_json::json!([{"type":"function","function":{"name":name,"description":"List editable atoms","parameters":{"type":"object"}}}])
                    };
                    let response = client
                        .post(format!("http://127.0.0.1:{port}/v1/{route}"))
                        .header("x-api-key", daemon.api_key.as_deref().unwrap())
                        .bearer_auth(daemon.api_key.as_deref().unwrap())
                        .header("anthropic-version", "2023-06-01")
                        .header(
                            "user-agent",
                            if claude {
                                "claude-cli/2.1.235 (external, e2e)"
                            } else {
                                "codex_cli_rs/0.147.0 (e2e)"
                            },
                        )
                        .json(&request)
                        .send()
                        .await
                        .unwrap();
                    let status = response.status();
                    let body = response.text().await.unwrap();
                    let context = format!("claude={claude}, stream={stream}, buffered={buffered}, input={input:?}, stop={stop}: {body}");
                    assert!(!body.contains("before write tool"), "{context}");
                    let Some(expected) = expected.as_ref() else {
                        assert!(body.contains("produced invalid JSON input"), "{context}");
                        if stream {
                            assert!(!body.contains("data: [DONE]"), "{context}");
                            assert!(!body.contains("\"type\":\"message_stop\""), "{context}");
                        } else {
                            assert_eq!(status, reqwest::StatusCode::BAD_GATEWAY, "{context}");
                        }
                        continue;
                    };
                    assert_eq!(status, reqwest::StatusCode::OK, "{context}");
                    assert!(!body.contains("\"error\""), "{context}");
                    let actual: serde_json::Value = if stream {
                        let events = body
                            .lines()
                            .filter_map(|line| line.strip_prefix("data: "))
                            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                            .collect::<Vec<_>>();
                        let arguments = if claude {
                            let tool = events
                                .iter()
                                .find_map(|event| {
                                    event
                                        .get("content_block")
                                        .filter(|block| block["type"] == "tool_use")
                                })
                                .expect("tool block");
                            assert_eq!(tool["name"], name, "{context}");
                            events
                                .iter()
                                .filter_map(|event| {
                                    event
                                        .pointer("/delta/partial_json")
                                        .and_then(serde_json::Value::as_str)
                                })
                                .collect::<String>()
                        } else {
                            let tools = events
                                .iter()
                                .filter_map(|event| {
                                    event
                                        .pointer("/choices/0/delta/tool_calls")
                                        .and_then(serde_json::Value::as_array)
                                })
                                .flatten()
                                .collect::<Vec<_>>();
                            assert!(
                                tools.iter().any(|tool| tool["function"]["name"] == name),
                                "{context}"
                            );
                            tools
                                .iter()
                                .filter_map(|tool| {
                                    tool.pointer("/function/arguments")
                                        .and_then(serde_json::Value::as_str)
                                })
                                .collect::<String>()
                        };
                        if claude && arguments.is_empty() {
                            serde_json::json!({})
                        } else {
                            serde_json::from_str(&arguments).unwrap_or_else(|error| {
                                panic!("{context}; invalid arguments {arguments:?}: {error}")
                            })
                        }
                    } else {
                        let response: serde_json::Value = serde_json::from_str(&body).unwrap();
                        if claude {
                            let tool = response["content"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .find(|block| block["type"] == "tool_use")
                                .expect("tool block");
                            assert_eq!(tool["name"], name, "{context}");
                            tool["input"].clone()
                        } else {
                            let tool = &response["choices"][0]["message"]["tool_calls"][0];
                            assert_eq!(tool["function"]["name"], name, "{context}");
                            serde_json::from_str(tool["function"]["arguments"].as_str().unwrap())
                                .unwrap()
                        }
                    };
                    assert_eq!(&actual, expected, "{context}");
                }
            }
        }
    }
    daemon.stop().await;
}

#[tokio::test]
async fn claude_stream_stops_before_later_frames_in_the_same_upstream_chunk() {
    assert_claude_stops_locally(true).await;
}

#[tokio::test]
async fn claude_nonstream_stops_locally_without_native_stop_fields() {
    assert_claude_stops_locally(false).await;
}

async fn assert_claude_stops_locally(stream: bool) {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    let mut upstream_body = event_stream_frame(
        "assistantResponseEvent",
        serde_json::json!({"content":"before <END> ignored"}),
    );
    upstream_body.extend(event_stream_frame(
        "reasoningContentEvent",
        serde_json::json!({"reasoningContentEvent":{"text":"must not leak"}}),
    ));
    upstream_body.extend(event_stream_frame(
        "messageMetadataEvent",
        serde_json::json!({
            "messageMetadataEvent": {
                "usage": {
                    "inputTokens": 100,
                    "outputTokens": 20,
                    "creditsConsumed": 0.25
                }
            }
        }),
    ));
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(upstream_body),
        )
        .mount(&mock)
        .await;

    let port = unused_tcp_port();
    let upstream_url = format!("{}/generateAssistantResponse", mock.uri());
    let daemon = Daemon::start_http(port, &upstream_url).await;
    import_context_alignment_account(&daemon, 0.0).await;
    let config_path = daemon.home().join("config.toml");
    let config = tokio::fs::read_to_string(&config_path)
        .await
        .expect("read config")
        .replace("enable_prompt_cache = false", "enable_prompt_cache = true");
    tokio::fs::write(&config_path, config)
        .await
        .expect("enable prompt cache");
    let reload = expect_ok(daemon.call("config.reload", serde_json::json!({})).await);
    assert_eq!(reload["applied"], true, "config reload failed: {reload}");

    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header(
            "x-api-key",
            daemon.api_key.as_deref().expect("service API key"),
        )
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", "claude-cli/2.1.235 (external, e2e)")
        .json(&serde_json::json!({
            "model":"source-large",
            "max_tokens":64,
            "stop_sequences":["<END>"],
            "stream":stream,
            "system":[{"type":"text","text":"cacheable system ".repeat(1500),
                "cache_control":{"type":"ephemeral"}}],
            "messages":[{"role":"user","content":"stop at the delimiter"}]
        }))
        .send()
        .await
        .expect("call Claude stop sequence stream");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body = response.text().await.expect("stream body");
    assert!(body.contains("before "), "{body}");
    assert!(!body.contains("ignored"), "{body}");
    assert!(!body.contains("must not leak"), "{body}");
    assert!(body.contains(r#""stop_reason":"stop_sequence""#), "{body}");
    assert!(body.contains(r#""stop_sequence":"<END>""#), "{body}");
    let usage = if stream {
        let message_start = body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|event| event["type"] == "message_start")
            .expect("message_start event");
        message_start["message"]["usage"].clone()
    } else {
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["usage"].clone()
    };
    assert_eq!(
        usage["cache_creation_input_tokens"], 0,
        "cache usage must come from Kiro rather than a local estimate"
    );
    let received = mock.received_requests().await.unwrap();
    let generation = received
        .iter()
        .find(|request| request.url.path() == "/generateAssistantResponse")
        .unwrap();
    let wire: serde_json::Value = serde_json::from_slice(&generation.body).unwrap();
    assert!(wire["inferenceConfig"].get("stopSequences").is_none());

    daemon.stop().await;
}

#[tokio::test]
async fn claude_thinking_follows_body_state_and_preserves_unsigned_tagged_content() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    let mut upstream_body = event_stream_frame(
        "assistantResponseEvent",
        serde_json::json!({"content":"<thinking>tagged secret</thinking>Hello"}),
    );
    upstream_body.extend(event_stream_frame(
        "reasoningContentEvent",
        serde_json::json!({"reasoningContentEvent":{"text":"native secret"}}),
    ));
    upstream_body.extend(event_stream_frame(
        "messageMetadataEvent",
        serde_json::json!({
            "messageMetadataEvent": {
                "usage": {
                    "inputTokens": 100,
                    "outputTokens": 20,
                    "creditsConsumed": 0.25
                }
            }
        }),
    ));
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(upstream_body),
        )
        .mount(&mock)
        .await;

    let port = unused_tcp_port();
    let upstream_url = format!("{}/generateAssistantResponse", mock.uri());
    let daemon = Daemon::start_http(port, &upstream_url).await;
    import_context_alignment_account(&daemon, 0.0).await;
    let client = reqwest::Client::new();
    let endpoint = format!("http://127.0.0.1:{port}/v1/messages");
    let beta = "interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13";

    let disabled = client
        .post(&endpoint)
        .header(
            "x-api-key",
            daemon.api_key.as_deref().expect("service API key"),
        )
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", beta)
        .header("user-agent", "claude-cli/2.1.235 (external, e2e)")
        .json(&serde_json::json!({
            "model":"source-large",
            "max_tokens":4096,
            "stream":true,
            "thinking":{"type":"disabled"},
            "messages":[{"role":"user","content":"thinking disabled"}]
        }))
        .send()
        .await
        .expect("call Claude with thinking disabled");
    assert_eq!(disabled.status(), reqwest::StatusCode::OK);
    let disabled_body = disabled.text().await.expect("disabled stream body");
    assert!(disabled_body.contains("Hello"), "{disabled_body}");
    assert!(!disabled_body.contains("tagged secret"), "{disabled_body}");
    assert!(!disabled_body.contains("native secret"), "{disabled_body}");
    assert!(!disabled_body.contains("thinking_delta"), "{disabled_body}");
    assert!(!disabled_body.contains("<thinking>"), "{disabled_body}");

    let enabled = client
        .post(&endpoint)
        .header(
            "x-api-key",
            daemon.api_key.as_deref().expect("service API key"),
        )
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", beta)
        .header("user-agent", "claude-cli/2.1.235 (external, e2e)")
        .json(&serde_json::json!({
            "model":"source-large",
            "max_tokens":4096,
            "stream":true,
            "thinking":{"type":"adaptive"},
            "context_management":{"edits":[{
                "type":"clear_thinking_20251015","keep":"all"
            }]},
            "messages":[{"role":"user","content":"thinking enabled"}]
        }))
        .send()
        .await
        .expect("call Claude with thinking enabled");
    assert_eq!(enabled.status(), reqwest::StatusCode::OK);
    let enabled_body = enabled.text().await.expect("enabled stream body");
    assert!(enabled_body.contains("Hello"), "{enabled_body}");
    assert!(enabled_body.contains("tagged secret"), "{enabled_body}");
    assert!(enabled_body.contains("native secret"), "{enabled_body}");
    assert!(enabled_body.contains("thinking_delta"), "{enabled_body}");
    assert!(!enabled_body.contains("signature_delta"), "{enabled_body}");
    assert!(enabled_body.contains("<thinking>"), "{enabled_body}");

    let received = mock.received_requests().await.expect("received requests");
    let payloads = received
        .iter()
        .filter(|request| request.url.path() == "/generateAssistantResponse")
        .filter_map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).ok())
        .collect::<Vec<_>>();
    assert_eq!(payloads.len(), 2);
    for payload in &payloads {
        assert!(
            !payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
                .as_str()
                .is_some_and(|prompt| prompt.contains("<thinking_mode>"))
        );
    }
    assert!(payloads[0].get("additionalModelRequestFields").is_none());
    assert_eq!(
        payloads[1]["additionalModelRequestFields"]["thinking"]["type"],
        "adaptive"
    );

    daemon.stop().await;
}

#[tokio::test]
async fn omitted_thinking_preserves_signatures_in_streaming_and_nonstreaming_responses() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    let mut body = event_stream_frame(
        "reasoningContentEvent",
        serde_json::json!({
            "reasoningContentEvent":{"text":"hidden native summary","signature":"native-signature"}
        }),
    );
    body.extend(generation_body(
        "<thinking>hidden tagged summary</thinking>Visible answer",
    ));
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(body),
        )
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    import_context_alignment_account(&daemon, 0.0).await;
    for stream in [false, true] {
        let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("x-api-key", daemon.api_key.as_deref().unwrap())
            .header("anthropic-version", "2023-06-01")
            .header("user-agent", "claude-cli/2.1.235 (external, e2e)")
            .json(&serde_json::json!({
                "model":"source-large", "max_tokens":4096,"stream":stream,
                "thinking":{"type":"adaptive","display":"omitted"},
                "messages":[{"role":"user","content":"explain"}]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body = response.text().await.unwrap();
        assert!(body.contains("Visible answer"), "{body}");
        assert!(body.contains("native-signature"), "{body}");
        assert!(!body.contains("hidden native summary"), "{body}");
        assert!(!body.contains("hidden tagged summary"), "{body}");
        assert!(!body.contains("thinking_delta"), "{body}");
    }
    daemon.stop().await;
}

#[tokio::test]
async fn claude_compaction_uses_a_separate_semantic_kiro_request() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(CompactionResponder)
        .mount(&mock)
        .await;

    let port = unused_tcp_port();
    let upstream_url = format!("{}/generateAssistantResponse", mock.uri());
    let daemon = Daemon::start_http(port, &upstream_url).await;
    assert_eq!(
        expect_ok(
            daemon
                .call(
                    "account.import",
                    serde_json::json!({
                        "accounts": [{
                            "id": "acc_88888888",
                            "email": "compact@example.com",
                            "machine_id": "8".repeat(64),
                            "credentials": {
                                "access_token": "compact-token",
                                "region": "us-east-1",
                                "expires_at": 4_000_000_000i64,
                                "auth_method": "idc"
                            },
                            "usage": {
                                "current": 0.0,
                                "limit": 100.0,
                                "percent_used": 0.0,
                                "updated_at": 1
                            },
                            "subscription": {"kind": "pro"},
                            "created_at": 1
                        }]
                    }),
                )
                .await,
        )["imported"],
        1
    );

    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header(
            "x-api-key",
            daemon.api_key.as_deref().expect("service API key"),
        )
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", "claude-cli/2.1.0 (external, e2e)")
        .json(&serde_json::json!({
            "model":"minimax-m2.5",
            "max_tokens":64,
            "stream":false,
            "context_management":{"edits":[{
                "type":"compact_20260112",
                "trigger":{"type":"input_tokens","value":50000}
            }]},
            "messages":[
                {"role":"user","content":"old context ".repeat(35_000)},
                {"role":"assistant","content":"an earlier conclusion"},
                {"role":"user","content":"continue the implementation"}
            ]
        }))
        .send()
        .await
        .expect("call Claude compact path");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("Claude response JSON");
    assert_eq!(body["content"][0]["type"], "compaction");
    assert!(body["content"][0]["content"]
        .as_str()
        .is_some_and(|content| content.contains("migrate compact to semantic Kiro summaries")));
    assert_eq!(body["content"][1]["text"], "main request completed");
    assert_eq!(body["usage"]["iterations"][0]["type"], "compaction");
    assert_eq!(body["usage"]["iterations"][1]["type"], "message");

    let requests = mock.received_requests().await.expect("received requests");
    let payloads = requests
        .iter()
        .filter_map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).ok())
        .filter(|payload| payload.get("conversationState").is_some())
        .collect::<Vec<_>>();
    assert_eq!(payloads.len(), 2, "summary plus main Kiro generation");
    let summary_request = payloads
        .iter()
        .find(|payload| {
            payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
                .as_str()
                .is_some_and(|content| content.contains("durable conversation checkpoint"))
        })
        .expect("semantic summary request");
    assert!(
        summary_request["conversationState"]["currentMessage"]["userInputMessage"]
            .get("userInputMessageContext")
            .is_none()
    );
    let main_request = payloads
        .iter()
        .find(|payload| {
            payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
                .as_str()
                .is_some_and(|content| {
                    content.contains("continue the implementation")
                        && !content.contains("durable conversation checkpoint")
                })
        })
        .expect("main request");
    assert!(main_request["conversationState"]["history"]
        .as_array()
        .expect("main history")
        .iter()
        .filter_map(|message| message["userInputMessage"]["content"].as_str())
        .any(|content| content.contains("System-generated conversation checkpoint")));

    daemon.stop().await;
}

#[tokio::test]
async fn upstream_context_rejection_partitions_compacts_and_retries_once() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    let responder = UpstreamOverflowCompactionResponder::default();
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(responder.clone())
        .mount(&mock)
        .await;

    let port = unused_tcp_port();
    let upstream_url = format!("{}/generateAssistantResponse", mock.uri());
    let daemon = Daemon::start_http(port, &upstream_url).await;
    import_context_alignment_account(&daemon, 0.0).await;

    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header(
            "x-api-key",
            daemon.api_key.as_deref().expect("service API key"),
        )
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", "claude-cli/2.1.235 (external, e2e)")
        .json(&serde_json::json!({
            "model":"source-large",
            "max_tokens":64,
            "stream":false,
            "messages":[
                {"role":"user","content":"overflow history ".repeat(100000)},
                {"role":"assistant","content":"keep the earlier decision"},
                {"role":"user","content":"retry after upstream overflow"}
            ]
        }))
        .send()
        .await
        .expect("call upstream overflow retry path");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("Claude response JSON");
    assert_eq!(body["content"][0]["type"], "compaction");
    assert_eq!(
        body["content"][1]["text"],
        "main request completed after overflow retry"
    );
    assert_eq!(responder.main_calls.load(Ordering::SeqCst), 2);
    let first_main_bytes = responder.first_main_body_bytes.load(Ordering::SeqCst);
    let summary_bytes = responder.summary_body_bytes.load(Ordering::SeqCst);
    assert!(first_main_bytes > 0);
    assert!(summary_bytes > 0);
    assert!(
        summary_bytes < first_main_bytes,
        "summary request was not bounded: {summary_bytes} >= {first_main_bytes}"
    );

    let requests = mock.received_requests().await.expect("received requests");
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/generateAssistantResponse")
            .count(),
        4,
        "expected first main request, two bounded summary parts, and one main retry"
    );

    daemon.stop().await;
}

#[tokio::test]
async fn model_mapping_overflow_auto_compacts_before_the_first_generation() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(CompactionResponder)
        .mount(&mock)
        .await;

    let port = unused_tcp_port();
    let upstream_url = format!("{}/generateAssistantResponse", mock.uri());
    let daemon = Daemon::start_http(port, &upstream_url).await;
    import_context_alignment_account(&daemon, 0.0).await;
    configure_context_alignment(
        &daemon,
        "summary-large",
        r#"
[[model_mapping]]
name = "large-to-small"
type = "replace"
source_models = ["source-large"]
target_models = ["mapped-small"]
priority = 10
"#,
    )
    .await;

    let config_path = daemon.home().join("config.toml");
    let raw = tokio::fs::read_to_string(&config_path).await.unwrap();
    let mut config: crosslane_core::config::Config = toml::from_str(&raw).unwrap();
    config.pool.max_concurrent_per_account = 1;
    tokio::fs::write(&config_path, toml::to_string(&config).unwrap())
        .await
        .unwrap();
    assert_eq!(
        expect_ok(daemon.call("config.reload", serde_json::json!({})).await)["applied"],
        true
    );

    // The oversized fixture takes nearly ten seconds even in isolation;
    // allow headroom when the full workspace suite runs concurrently.
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header(
            "x-api-key",
            daemon.api_key.as_deref().expect("service API key"),
        )
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", "claude-cli/2.1.235 (external, e2e)")
        .json(&serde_json::json!({
            "model":"source-large",
            "max_tokens":64,
            "stream":false,
            "system":"Never lose this governing instruction.",
            "messages":[
                {"role":"user","content":"old context ".repeat(140000)},
                {"role":"assistant","content":"an earlier conclusion"},
                {"role":"user","content":"continue the implementation"}
            ]
        }))
        .send()
        .await
        .expect("call automatic compact path");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("Claude response JSON");
    assert_eq!(body["content"][0]["type"], "compaction");
    assert!(body["content"][0]["content"]
        .as_str()
        .is_some_and(|content| content.contains("extractive fallback checkpoint")));
    assert_eq!(body["content"][1]["text"], "main request completed");
    assert!(body["usage"].get("iterations").is_none());
    let applied = &body["context_management"]["applied_edits"][0];
    assert_eq!(applied["reason"], "model_mapping_overflow");
    assert!(
        applied["original_input_tokens"].as_u64().expect("original")
            > applied["compacted_input_tokens"]
                .as_u64()
                .expect("compacted")
    );

    let requests = mock.received_requests().await.expect("received requests");
    let payloads = requests
        .iter()
        .filter_map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).ok())
        .filter(|payload| payload.get("conversationState").is_some())
        .collect::<Vec<_>>();
    assert_eq!(
        payloads.len(),
        1,
        "multiwave automatic compaction must skip doomed summary requests"
    );
    let summaries = payloads
        .iter()
        .filter(|payload| {
            payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
                .as_str()
                .is_some_and(|content| content.contains("durable conversation checkpoint"))
        })
        .collect::<Vec<_>>();
    assert!(summaries.is_empty());
    let main = payloads
        .iter()
        .find(|payload| {
            payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
                .as_str()
                .is_some_and(|content| {
                    content.contains("continue the implementation")
                        && !content.contains("durable conversation checkpoint")
                })
        })
        .expect("main request");
    assert_eq!(
        main["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
        "mapped-small"
    );
    assert_eq!(
        main["conversationState"]["currentMessage"]["userInputMessage"]["content"],
        "continue the implementation"
    );
    assert!(
        main["conversationState"]["history"][0]["userInputMessage"]["content"]
            .as_str()
            .is_some_and(|content| content.contains("Never lose this governing instruction."))
    );
    assert_eq!(
        main["conversationState"]["history"][1]["assistantResponseMessage"]["content"],
        "I will follow these instructions."
    );
    assert!(main.to_string().contains("extractive fallback checkpoint"));

    daemon.stop().await;
}

#[tokio::test]
async fn compacted_stream_retries_before_flushing_the_compaction_block() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    let responder = RetryingCompactionStreamResponder::default();
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(responder.clone())
        .mount(&mock)
        .await;

    let port = unused_tcp_port();
    let upstream_url = format!("{}/generateAssistantResponse", mock.uri());
    let daemon = Daemon::start_http(port, &upstream_url).await;
    import_context_alignment_account(&daemon, 0.0).await;
    assert_eq!(
        expect_ok(
            daemon
                .call(
                    "account.import",
                    serde_json::json!({
                        "accounts": [{
                            "id": "acc_77777777",
                            "email": "stream-retry@example.com",
                            "machine_id": "7".repeat(64),
                            "credentials": {
                                "access_token": "stream-retry-token",
                                "region": "us-east-1",
                                "expires_at": 4_000_000_000i64,
                                "auth_method": "idc"
                            },
                            "usage": {
                                "current": 0.0,
                                "limit": 100.0,
                                "percent_used": 0.0,
                                "updated_at": 1
                            },
                            "subscription": {"kind": "pro"},
                            "created_at": 1
                        }]
                    }),
                )
                .await,
        )["imported"],
        1
    );
    configure_context_alignment(
        &daemon,
        "summary-large",
        r#"
[[model_mapping]]
name = "large-to-small-stream"
type = "replace"
source_models = ["source-large"]
target_models = ["mapped-small"]
priority = 10
"#,
    )
    .await;

    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header(
            "x-api-key",
            daemon.api_key.as_deref().expect("service API key"),
        )
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", "claude-cli/2.1.235 (external, e2e)")
        .json(&serde_json::json!({
            "model":"source-large",
            "max_tokens":64,
            "stream":true,
            "messages":[
                {"role":"user","content":"old context ".repeat(140000)},
                {"role":"assistant","content":"an earlier conclusion"},
                {"role":"user","content":"continue the streamed implementation"}
            ]
        }))
        .send()
        .await
        .expect("call compacted stream retry path");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body = response.text().await.expect("stream body");
    assert_eq!(responder.main_calls.load(Ordering::SeqCst), 2);
    assert!(!body.contains("broken-write"), "{body}");
    assert!(!body.contains("produced complete JSON"), "{body}");
    assert!(body.contains("\"type\":\"compaction\""), "{body}");
    assert!(!body.contains("\"type\":\"compaction_delta\""), "{body}");
    let checkpoints = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| {
            event["type"] == "content_block_start" && event["content_block"]["type"] == "compaction"
        })
        .count();
    assert_eq!(checkpoints, 1, "retry must emit only one checkpoint block");
    assert_eq!(
        body.matches("Task Overview: preserve the compacted stream across a retry.")
            .count(),
        2,
        "the single checkpoint must contain both chronological part summaries"
    );
    assert!(
        body.contains("main request completed after retry"),
        "{body}"
    );
    assert!(
        body.find("\"type\":\"compaction\"").expect("compaction")
            < body
                .find("main request completed after retry")
                .expect("retried content")
    );

    daemon.stop().await;
}

#[tokio::test]
async fn timed_out_compaction_summary_is_still_accounted() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(SlowCompactionResponder)
        .mount(&mock)
        .await;

    let port = unused_tcp_port();
    let upstream_url = format!("{}/generateAssistantResponse", mock.uri());
    let daemon = Daemon::start_http(port, &upstream_url).await;
    import_context_alignment_account(&daemon, 0.0).await;
    configure_context_alignment(
        &daemon,
        "summary-large",
        r#"
[[model_mapping]]
name = "large-to-small-timeout"
type = "replace"
source_models = ["source-large"]
target_models = ["mapped-small"]
priority = 10
"#,
    )
    .await;
    let config_path = daemon.home().join("config.toml");
    let raw = tokio::fs::read_to_string(&config_path)
        .await
        .expect("read config");
    tokio::fs::write(
        &config_path,
        raw.replace(
            "compaction_summary_timeout_ms = 120000",
            "compaction_summary_timeout_ms = 1",
        ),
    )
    .await
    .expect("write timeout config");
    let reload = expect_ok(daemon.call("config.reload", serde_json::json!({})).await);
    assert_eq!(reload["applied"], true, "config reload failed: {reload}");

    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header(
            "x-api-key",
            daemon.api_key.as_deref().expect("service API key"),
        )
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", "claude-cli/2.1.235 (external, e2e)")
        .json(&serde_json::json!({
            "model":"source-large",
            "max_tokens":64,
            "stream":false,
            "messages":[
                {"role":"user","content":"old context ".repeat(140000)},
                {"role":"assistant","content":"an earlier conclusion"},
                {"role":"user","content":"continue after the summary timeout"}
            ]
        }))
        .send()
        .await
        .expect("call summary timeout path");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("Claude response JSON");
    assert_eq!(body["content"][0]["type"], "compaction");
    assert_eq!(
        body["content"][1]["text"],
        "main request completed while summary accounting continued"
    );

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let stats = expect_ok(
            daemon
                .call("stats", serde_json::json!({"detail":true,"recent":20}))
                .await,
        );
        let accounted = stats["stats"]["recent_requests"]
            .as_array()
            .is_some_and(|requests| {
                requests.iter().any(|request| {
                    request["path"] == "/internal/compact"
                        && request["input_tokens"]
                            .as_u64()
                            .is_some_and(|tokens| tokens > 0)
                        && request["credits"]
                            .as_f64()
                            .is_some_and(|credits| credits > 0.0)
                })
            });
        if accounted {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed-out summary never reached stats accounting: {stats}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    daemon.stop().await;
}

#[tokio::test]
async fn failed_compaction_stream_preserves_partial_usage_accounting() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(PartialFailureCompactionResponder)
        .mount(&mock)
        .await;

    let port = unused_tcp_port();
    let upstream_url = format!("{}/generateAssistantResponse", mock.uri());
    let daemon = Daemon::start_http(port, &upstream_url).await;
    import_context_alignment_account(&daemon, 0.0).await;
    configure_context_alignment(
        &daemon,
        "summary-large",
        r#"
[[model_mapping]]
name = "large-to-small-partial-summary"
type = "replace"
source_models = ["source-large"]
target_models = ["mapped-small"]
priority = 10
"#,
    )
    .await;

    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header(
            "x-api-key",
            daemon.api_key.as_deref().expect("service API key"),
        )
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", "claude-cli/2.1.235 (external, e2e)")
        .json(&serde_json::json!({
            "model":"source-large",
            "max_tokens":64,
            "stream":false,
            "messages":[
                {"role":"user","content":"old context ".repeat(140000)},
                {"role":"assistant","content":"an earlier conclusion"},
                {"role":"user","content":"continue after the malformed summary stream"}
            ]
        }))
        .send()
        .await
        .expect("call malformed summary stream path");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("Claude response JSON");
    assert_eq!(body["content"][0]["type"], "compaction");
    assert_eq!(
        body["content"][1]["text"],
        "main request completed after the failed summary was accounted"
    );
    let stats = expect_ok(
        daemon
            .call("stats", serde_json::json!({"detail":true,"recent":20}))
            .await,
    );
    let compacts = stats["stats"]["recent_requests"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|request| request["path"] == "/internal/compact")
        .collect::<Vec<_>>();
    assert!(!compacts.is_empty() && compacts.len() <= 2);
    assert_eq!(
        body["usage"]["iterations"][0]["input_tokens"],
        compacts.len() as u64 * 100
    );
    assert_eq!(
        body["usage"]["iterations"][0]["output_tokens"],
        compacts.len() as u64 * 20
    );
    let compact = stats["stats"]["recent_requests"]
        .as_array()
        .and_then(|requests| {
            requests
                .iter()
                .find(|request| request["path"] == "/internal/compact")
        })
        .expect("failed compact request was recorded");
    assert_eq!(compact["status"], 502);
    assert_eq!(compact["input_tokens"], 100);
    assert_eq!(compact["output_tokens"], 20);
    assert_eq!(compact["credits"], 0.25);

    daemon.stop().await;
}

#[tokio::test]
async fn resolved_account_window_replans_once_and_reuses_the_summary() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(CompactionResponder)
        .mount(&mock)
        .await;

    let port = unused_tcp_port();
    let upstream_url = format!("{}/generateAssistantResponse", mock.uri());
    let daemon = Daemon::start_http(port, &upstream_url).await;
    import_context_alignment_account(&daemon, 50.0).await;
    configure_context_alignment(
        &daemon,
        "summary-large",
        r#"
[[model_mapping]]
name = "low-credit-small-window"
type = "replace"
source_models = ["source-large"]
target_models = ["resolved-small"]
max_remaining_credit_percent = 90.0
priority = 10
"#,
    )
    .await;

    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header(
            "x-api-key",
            daemon.api_key.as_deref().expect("service API key"),
        )
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", "claude-cli/2.1.235 (external, e2e)")
        .json(&serde_json::json!({
            "model":"source-large",
            "max_tokens":64,
            "stream":false,
            "messages":[
                {"role":"user","content":"resolved history ".repeat(35000)},
                {"role":"assistant","content":"keep the decision"},
                {"role":"user","content":"finish after replanning"}
            ]
        }))
        .send()
        .await
        .expect("call resolved-window compact path");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("Claude response JSON");
    assert_eq!(body["content"][0]["type"], "compaction");

    let requests = mock.received_requests().await.expect("received requests");
    let payloads = requests
        .iter()
        .filter_map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).ok())
        .filter(|payload| payload.get("conversationState").is_some())
        .collect::<Vec<_>>();
    assert_eq!(
        payloads.len(),
        2,
        "the failed context preflight must not reach Kiro and replanning must not resummarize"
    );
    assert_eq!(
        payloads
            .iter()
            .filter(
                |payload| payload["conversationState"]["currentMessage"]["userInputMessage"]
                    ["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("durable conversation checkpoint"))
            )
            .count(),
        1
    );
    let main = payloads
        .iter()
        .find(|payload| {
            payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
                .as_str()
                .is_some_and(|content| {
                    content.contains("finish after replanning")
                        && !content.contains("durable conversation checkpoint")
                })
        })
        .expect("main request");
    assert_eq!(
        main["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
        "resolved-small"
    );

    daemon.stop().await;
}

#[tokio::test]
async fn mapped_compaction_is_reapplied_for_a_smaller_resolved_window_without_resummarizing() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(LongCompactionResponder)
        .mount(&mock)
        .await;

    let port = unused_tcp_port();
    let upstream_url = format!("{}/generateAssistantResponse", mock.uri());
    let daemon = Daemon::start_http(port, &upstream_url).await;
    import_context_alignment_account(&daemon, 50.0).await;
    configure_context_alignment(
        &daemon,
        "summary-large",
        r#"
[[model_mapping]]
name = "low-credit-tiny-window"
type = "replace"
source_models = ["source-large"]
target_models = ["resolved-tiny"]
max_remaining_credit_percent = 90.0
priority = 5

[[model_mapping]]
name = "large-to-small"
type = "replace"
source_models = ["source-large"]
target_models = ["mapped-small"]
priority = 10
"#,
    )
    .await;

    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header(
            "x-api-key",
            daemon.api_key.as_deref().expect("service API key"),
        )
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", "claude-cli/2.1.235 (external, e2e)")
        .json(&serde_json::json!({
            "model":"source-large",
            "max_tokens":64,
            "stream":false,
            "messages":[
                {"role":"user","content":"mapped history ".repeat(140000)},
                {"role":"assistant","content":"keep the decision"},
                {"role":"user","content":"finish after semantic reuse"}
            ]
        }))
        .send()
        .await
        .expect("call two-window compact path");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("Claude response JSON");
    assert_eq!(body["content"][0]["type"], "compaction");
    assert!(body["content"][0]["content"]
        .as_str()
        .is_some_and(|content| content.contains("checkpoint shortened to fit context")));

    let requests = mock.received_requests().await.expect("received requests");
    let payloads = requests
        .iter()
        .filter_map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).ok())
        .filter(|payload| payload.get("conversationState").is_some())
        .collect::<Vec<_>>();
    assert_eq!(payloads.len(), 3, "two summary parts plus one main request");
    assert_eq!(
        payloads
            .iter()
            .filter(
                |payload| payload["conversationState"]["currentMessage"]["userInputMessage"]
                    ["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("durable conversation checkpoint"))
            )
            .count(),
        2,
        "resolved-window replanning must reuse both original summary parts"
    );
    let main = payloads
        .iter()
        .find(|payload| {
            payload["conversationState"]["currentMessage"]["userInputMessage"]["content"]
                .as_str()
                .is_some_and(|content| {
                    content.contains("finish after semantic reuse")
                        && !content.contains("durable conversation checkpoint")
                })
        })
        .expect("main request");
    assert_eq!(
        main["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
        "resolved-tiny"
    );

    daemon.stop().await;
}

#[tokio::test]
async fn a_second_smaller_resolved_window_fails_without_a_compaction_loop() {
    let _http_guard = HTTP_TEST_LOCK.lock().await;
    let mock = MockServer::start().await;
    mount_context_alignment_models(&mock).await;
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(CompactionResponder)
        .mount(&mock)
        .await;

    let port = unused_tcp_port();
    let upstream_url = format!("{}/generateAssistantResponse", mock.uri());
    let daemon = Daemon::start_http(port, &upstream_url).await;
    import_context_alignment_account(&daemon, 20.0).await;
    assert_eq!(
        expect_ok(
            daemon
                .call(
                    "account.import",
                    serde_json::json!({
                        "accounts": [{
                            "id": "acc_88888888",
                            "email": "smaller-window@example.com",
                            "machine_id": "8".repeat(64),
                            "credentials": {
                                "access_token": "smaller-window-token",
                                "region": "us-east-1",
                                "expires_at": 4_000_000_000i64,
                                "auth_method": "idc"
                            },
                            "usage": {
                                "current": 30.0,
                                "limit": 100.0,
                                "percent_used": 30.0,
                                "updated_at": 1
                            },
                            "subscription": {"kind": "pro"},
                            "created_at": 1
                        }]
                    }),
                )
                .await,
        )["imported"],
        1
    );
    configure_context_alignment(
        &daemon,
        // Force capacity preflight to choose extractive fallback so the
        // summary operation cannot affect which account the second main
        // preflight selects.
        "resolved-tiny",
        r#"
[[model_mapping]]
name = "lowest-credit-tiny-window"
type = "replace"
source_models = ["source-large"]
target_models = ["resolved-tiny"]
max_remaining_credit_percent = 75.0
priority = 5

[[model_mapping]]
name = "low-credit-small-window"
type = "replace"
source_models = ["source-large"]
target_models = ["resolved-small"]
max_remaining_credit_percent = 85.0
priority = 10
"#,
    )
    .await;

    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header(
            "x-api-key",
            daemon.api_key.as_deref().expect("service API key"),
        )
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", "claude-cli/2.1.235 (external, e2e)")
        .json(&serde_json::json!({
            "model":"source-large",
            "max_tokens":64,
            "stream":false,
            "messages":[
                {"role":"user","content":"resolved drift history ".repeat(35000)},
                {"role":"assistant","content":"keep the decision"},
                {"role":"user","content":"this must stop after one replan ".repeat(2000)}
            ]
        }))
        .send()
        .await
        .expect("call bounded resolved-window path");
    let status = response.status();
    let body: serde_json::Value = response.json().await.expect("Claude error JSON");
    let requests = mock.received_requests().await.expect("received requests");
    let generated_models = requests
        .iter()
        .filter(|request| request.url.path() == "/generateAssistantResponse")
        .filter_map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).ok())
        .filter_map(|payload| {
            payload["conversationState"]["currentMessage"]["userInputMessage"]["modelId"]
                .as_str()
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        status,
        reqwest::StatusCode::BAD_REQUEST,
        "unexpected response {body}; generated models: {generated_models:?}"
    );
    let message = body["error"]["message"].as_str().expect("error message");
    assert!(
        message.contains("resolved-tiny"),
        "unexpected error: {message}; generated models: {generated_models:?}"
    );
    assert!(message.contains("prompt is too long"));
    assert!(
        message.contains(" > 495"),
        "unexpected safe window: {message}"
    );

    assert_eq!(
        generated_models.len(),
        0,
        "both account checks must fail before Kiro and no third attempt is allowed"
    );

    daemon.stop().await;
}

#[tokio::test]
async fn bare_start_creates_files_secures_socket_and_serves_status() {
    use std::os::unix::fs::PermissionsExt;

    let daemon = Daemon::start().await;
    for name in ["config.toml", "accounts.json", "daily.json", "stats.json"] {
        assert!(daemon.home().join(name).exists(), "{name} missing");
    }
    let socket_mode = std::fs::metadata(&daemon.socket)
        .expect("socket metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(socket_mode, 0o600);
    let status = expect_ok(daemon.call("status", serde_json::json!({})).await);
    assert_eq!(status["account_total"], 0);
    assert_eq!(status["listen"], "-");
    assert_eq!(status["proxy_service_total"], 0);
    assert!(status["hint"].is_string());
    daemon.stop().await;
}

#[tokio::test]
async fn account_lifecycle_persists_without_token_leakage() {
    let daemon = Daemon::start().await;
    let account = serde_json::json!({
        "accounts": [{
            "id": "acc_11111111",
            "email": "e2e@example.com",
            "machine_id": "2".repeat(64),
            "credentials": {
                "access_token": "at-e2e-secret",
                "region": "us-east-1",
                "expires_at": 4_000_000_000i64,
                "auth_method": "idc"
            },
            "created_at": 1
        }]
    });
    assert_eq!(
        expect_ok(daemon.call("account.import", account).await)["imported"],
        1
    );
    let list = expect_ok(daemon.call("account.list", serde_json::json!({})).await);
    assert_eq!(list["accounts"].as_array().expect("array").len(), 1);
    assert!(!serde_json::to_string(&list)
        .expect("serialize")
        .contains("at-e2e-secret"));

    expect_ok(
        daemon
            .call(
                "account.tag",
                serde_json::json!({"id": "e2e@example.com", "add": ["prod"]}),
            )
            .await,
    );
    let filtered = expect_ok(
        daemon
            .call("account.list", serde_json::json!({"tag": "prod"}))
            .await,
    );
    assert_eq!(filtered["accounts"].as_array().expect("array").len(), 1);

    expect_ok(
        daemon
            .call(
                "account.setEnabled",
                serde_json::json!({"id": "acc_11111111", "enabled": false}),
            )
            .await,
    );
    let after = expect_ok(daemon.call("account.list", serde_json::json!({})).await);
    assert_eq!(after["accounts"][0]["enabled"], false);
    let disk = tokio::fs::read_to_string(daemon.home().join("accounts.json"))
        .await
        .expect("read disk");
    assert!(disk.contains("prod"));

    expect_ok(
        daemon
            .call("account.remove", serde_json::json!({"id": "acc_11111111"}))
            .await,
    );
    let empty = expect_ok(daemon.call("account.list", serde_json::json!({})).await);
    assert!(empty["accounts"].as_array().expect("array").is_empty());
    daemon.stop().await;
}

#[tokio::test]
async fn config_edit_hot_reloads_and_broken_edit_keeps_service_alive() {
    let daemon = Daemon::start().await;
    let config_path = daemon.home().join("config.toml");
    let raw = tokio::fs::read_to_string(&config_path)
        .await
        .expect("read config");
    let edited = raw.replace("enable_prompt_cache = false", "enable_prompt_cache = true");
    tokio::fs::write(&config_path, edited)
        .await
        .expect("edit config");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let show = expect_ok(daemon.call("config.show", serde_json::json!({})).await);
        if show["effective_json"]["features"]["enable_prompt_cache"] == true {
            break;
        }
        assert!(Instant::now() < deadline, "hot reload timeout");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    tokio::fs::write(&config_path, "[server\nport = ")
        .await
        .expect("break config");
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    let status = expect_ok(daemon.call("status", serde_json::json!({})).await);
    assert_eq!(status["listen"], "-");
    daemon.stop().await;
}

#[tokio::test]
async fn unknown_method_returns_structured_error() {
    let daemon = Daemon::start().await;
    match daemon.call("does.not.exist", serde_json::json!({})).await {
        Response::Err { error, .. } => assert_eq!(error.code, 404),
        Response::Ok { .. } => panic!("unknown method should fail"),
    }
    daemon.stop().await;
}

#[tokio::test]
async fn restart_reloads_persisted_accounts() {
    let home = tempfile::tempdir().expect("tempdir");
    let socket = home.path().join("admin.sock");
    let mut first = Command::new(env!("CARGO_BIN_EXE_crosslaned"))
        .env("CROSSLANE_HOME", home.path())
        .env("CROSSLANE_HTTP_PORT", "0")
        .env("CROSSLANE_DISABLE_HTTP", "1")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn first");
    wait_for_socket(&socket).await;
    expect_ok(
        rpc_call(
            &socket,
            "account.import",
            serde_json::json!({
                "accounts": [{
                    "id": "acc_33333333",
                    "email": "persist@example.com",
                    "machine_id": "4".repeat(64),
                    "credentials": {
                        "access_token": "at",
                        "region": "us-east-1",
                        "expires_at": 4_000_000_000i64,
                        "auth_method": "idc"
                    },
                    "created_at": 1
                }]
            }),
        )
        .await,
    );
    first.kill().await.expect("kill first");
    let _first_status = first.wait().await;

    let mut second = Command::new(env!("CARGO_BIN_EXE_crosslaned"))
        .env("CROSSLANE_HOME", home.path())
        .env("CROSSLANE_HTTP_PORT", "0")
        .env("CROSSLANE_DISABLE_HTTP", "1")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn second");
    wait_for_socket(&socket).await;
    let list = expect_ok(rpc_call(&socket, "account.list", serde_json::json!({})).await);
    assert_eq!(list["accounts"].as_array().expect("array").len(), 1);
    assert_eq!(list["accounts"][0]["email"], "persist@example.com");
    second.kill().await.expect("kill second");
}

#[tokio::test]
async fn external_account_file_edits_reload_without_accepting_corruption() {
    let daemon = Daemon::start().await;
    let path = daemon.home().join("accounts.json");
    tokio::fs::write(
        &path,
        serde_json::to_vec_pretty(&serde_json::json!([{
            "id": "acc_55555555",
            "email": "external@example.com",
            "machine_id": "5".repeat(64),
            "credentials": {
                "access_token": "external-token",
                "region": "us-east-1",
                "expires_at": 4_000_000_000i64,
                "auth_method": "idc"
            },
            "created_at": 1
        }]))
        .expect("serialize accounts"),
    )
    .await
    .expect("write accounts");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let list = expect_ok(daemon.call("account.list", serde_json::json!({})).await);
        if list["accounts"]
            .as_array()
            .is_some_and(|items| items.len() == 1)
        {
            assert_eq!(list["accounts"][0]["email"], "external@example.com");
            break;
        }
        assert!(Instant::now() < deadline, "account file reload timeout");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    tokio::fs::write(&path, b"{broken")
        .await
        .expect("break accounts");
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    let retained = expect_ok(daemon.call("account.list", serde_json::json!({})).await);
    assert_eq!(retained["accounts"][0]["email"], "external@example.com");
    daemon.stop().await;
}

#[tokio::test]
async fn stale_socket_file_does_not_block_startup() {
    let home = tempfile::tempdir().expect("tempdir");
    let socket = home.path().join("admin.sock");
    tokio::fs::write(&socket, b"stale")
        .await
        .expect("write stale");
    let mut child = Command::new(env!("CARGO_BIN_EXE_crosslaned"))
        .env("CROSSLANE_HOME", home.path())
        .env("CROSSLANE_HTTP_PORT", "0")
        .env("CROSSLANE_DISABLE_HTTP", "1")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn");
    wait_for_socket(&socket).await;
    child.kill().await.expect("kill");
}

#[tokio::test]
async fn second_daemon_cannot_delete_a_live_socket() {
    let daemon = Daemon::start().await;
    let mut second = Command::new(env!("CARGO_BIN_EXE_crosslaned"))
        .env("CROSSLANE_HOME", daemon.home())
        .env("CROSSLANE_HTTP_PORT", "0")
        .env("CROSSLANE_DISABLE_HTTP", "1")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn second");
    let status = tokio::time::timeout(Duration::from_secs(5), second.wait())
        .await
        .expect("second daemon should exit")
        .expect("wait second");
    assert!(!status.success());

    let still_alive = expect_ok(daemon.call("status", serde_json::json!({})).await);
    assert_eq!(still_alive["pid"], daemon.child.id().expect("pid"));
    daemon.stop().await;
}
