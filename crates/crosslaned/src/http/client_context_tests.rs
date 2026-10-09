//! Exercise attribution through HTTP routing, providers, and continuation state.

use super::tests::{body_json, test_state};
use super::*;
use axum::body::{to_bytes, Body, Bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use crosslane_core::config::{Config, ProviderConfig};
use crosslane_core::provider::{
    ProviderCapabilities, ProviderDescriptor, ProviderId, ProviderModel, ProviderProtocol,
};
use crosslane_provider::{
    ProviderAdapter, ProviderError, ProviderRequest, ProviderResponse, ProviderResponseBody,
};
use futures::StreamExt;
use serde_json::{json, Value};
use std::sync::Mutex;
use tower::ServiceExt;

#[derive(Default)]
struct RecordingProvider {
    requests: Mutex<Vec<ProviderRequest>>,
}

#[async_trait::async_trait]
impl ProviderAdapter for RecordingProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: ProviderId::parse("copilot").unwrap(),
            kind: "copilot".into(),
            enabled: true,
            status: "ready".into(),
            error: None,
            capabilities: ProviderCapabilities {
                protocols: vec![
                    ProviderProtocol::ClaudeMessages,
                    ProviderProtocol::OpenAiResponses,
                ],
                ..Default::default()
            },
        }
    }

    async fn models(&self, _: bool) -> Result<Vec<ProviderModel>, ProviderError> {
        Ok(Vec::new())
    }

    async fn execute(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let protocol = request.protocol;
        let index = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request);
            requests.len()
        };
        if body["fail"] == true {
            return Err(ProviderError::unavailable("fixture unavailable"));
        }
        let response = if protocol == ProviderProtocol::OpenAiResponses {
            json!({"id":format!("resp_fixture_{index}"),"object":"response","status":"completed",
                "output":[],"usage":{"input_tokens":1,"output_tokens":1}})
        } else {
            json!({"id":"msg_fixture","type":"message","role":"assistant","content":[],
                "usage":{"input_tokens":1,"output_tokens":1}})
        };
        let response_body = if body["stream"] == true {
            let bytes = Bytes::from(format!(
                "event: response.completed\ndata: {}\n\n",
                json!({"type":"response.completed","response":response})
            ));
            ProviderResponseBody::Stream(Box::pin(futures::stream::iter([Ok(bytes)])))
        } else {
            ProviderResponseBody::Full(Bytes::from(response.to_string()))
        };
        Ok(ProviderResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: response_body,
            account_id: Some("shared-account".into()),
            upstream_request_id: None,
        })
    }

    async fn count_tokens(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        self.requests.lock().unwrap().push(request);
        Ok(ProviderResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: ProviderResponseBody::Full(Bytes::from_static(br#"{"input_tokens":1}"#)),
            account_id: Some("shared-account".into()),
            upstream_request_id: None,
        })
    }
}

fn post(path: &str, body: &Value) -> Request<Body> {
    Request::post(path)
        .header(
            "user-agent",
            if path.contains("responses") {
                "codex_cli_rs/0.153.0"
            } else {
                "claude-cli/2.1.260 (external, test)"
            },
        )
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn provider_state() -> (tempfile::TempDir, Arc<AppState>, Arc<RecordingProvider>) {
    let mut config = Config::default();
    config.provider.push(ProviderConfig {
        id: "copilot".into(),
        kind: "copilot".into(),
        enabled: true,
        ..Default::default()
    });
    let (directory, state) = test_state(config).await;
    let provider = Arc::new(RecordingProvider::default());
    state.providers.registry().register(provider.clone()).await;
    (directory, state, provider)
}

fn client_metadata() -> Value {
    json!({"x-codex-installation-id":"installation-a","session_id":"session-a","thread_id":"thread-a"})
}

fn test_router(state: Arc<AppState>) -> Router {
    router_for_service(
        state,
        ProxyServiceConfig {
            id: "test".into(),
            allowed_providers: vec!["kiro".into(), "copilot".into()],
            ..Default::default()
        },
        false,
    )
}

#[tokio::test]
async fn count_generation_tool_results_and_errors_share_context_before_provider_routing() {
    // The same anonymous device is represented identically in Kiro and a
    // provider adapter, including local count and request rejection paths.
    let metadata =
        json!({"user_id":json!({"device_id":"device-a","session_id":"session-a"}).to_string()});
    let (_directory, kiro) = test_state(Config::default()).await;
    let mut body = json!({"model":"model","max_tokens":1,"metadata":metadata,"messages":[{"role":"user","content":"hello"}]});
    for path in ["/v1/messages/count_tokens", "/v1/messages"] {
        let response = router(kiro.clone())
            .oneshot(post(path, &body))
            .await
            .unwrap();
        let status = response.status();
        let response_body = body_json(response).await;
        assert_eq!(
            status,
            if path.ends_with("count_tokens") {
                StatusCode::OK
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            },
            "{path}: {response_body}"
        );
    }
    let logs = kiro.stats.snapshot(None).recent_requests;
    let expected = logs[0].diagnostics.client_context.clone().unwrap();
    assert!(expected.user_ref.is_some());
    assert_eq!(logs[1].diagnostics.client_context.as_ref(), Some(&expected));

    let (_directory, state, provider) = provider_state().await;
    body["model"] = json!("copilot/model");
    for path in ["/v1/messages/count_tokens", "/v1/messages"] {
        let response = test_router(state.clone())
            .oneshot(post(path, &body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let _ = body_json(response).await;
    }
    body["messages"] = json!([
        {"role":"user","content":"hello"},
        {"role":"assistant","content":[{"type":"tool_use","id":"call_a","name":"read_file","input":{}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_a","content":"contents"}]}
    ]);
    let response = test_router(state.clone())
        .oneshot(post("/v1/messages", &body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = body_json(response).await;
    body["fail"] = json!(true);
    let response = test_router(state.clone())
        .oneshot(post("/v1/messages", &body))
        .await
        .unwrap();
    assert!(response.status().is_server_error());
    let _ = body_json(response).await;
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    for request in requests.iter() {
        assert_eq!(request.client_context, expected);
        assert_eq!(
            serde_json::from_slice::<Value>(&request.body).unwrap()["metadata"],
            metadata
        );
    }
    for log in state.stats.snapshot(None).recent_requests {
        assert_eq!(log.diagnostics.client_context.as_ref(), Some(&expected));
    }
}

#[tokio::test]
async fn provider_responses_inherit_identity_for_json_and_streaming_tool_continuations() {
    for streaming in [false, true] {
        let (_directory, state, provider) = provider_state().await;
        let body = json!({"model":"copilot/model","input":"hello","stream":streaming,"client_metadata":client_metadata()});
        let response = test_router(state.clone())
            .oneshot(post("/v1/responses", &body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut pending_stream = if streaming {
            let mut stream = response.into_body().into_data_stream();
            let completed = stream.next().await.unwrap().unwrap();
            assert!(String::from_utf8_lossy(&completed).contains("response.completed"));
            Some(stream)
        } else {
            let _ = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            None
        };
        let context = provider.requests.lock().unwrap()[0].client_context.clone();
        let continuation = json!({"model":"copilot/model","previous_response_id":"resp_fixture_1",
            "input":[{"type":"function_call_output","call_id":"call_a","output":"result"}]});
        let response = test_router(state.clone())
            .oneshot(post("/v1/responses", &continuation))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let _ = body_json(response).await;
        assert_eq!(provider.requests.lock().unwrap()[1].client_context, context);
        // Root transport EOF is polled only after the tool continuation has
        // already arrived, exercising the completed-event/next-request race.
        if let Some(stream) = pending_stream.as_mut() {
            while let Some(chunk) = stream.next().await {
                chunk.unwrap();
            }
        }
        let mut other = continuation;
        other["client_metadata"] = client_metadata();
        other["client_metadata"]["x-codex-installation-id"] = json!("installation-b");
        let response = test_router(state.clone())
            .oneshot(post("/v1/responses", &other))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(provider.requests.lock().unwrap().len(), 2);
        for log in state
            .stats
            .snapshot(None)
            .recent_requests
            .into_iter()
            .take(2)
        {
            assert_eq!(log.diagnostics.client_context.as_ref(), Some(&context));
        }
    }
}

#[tokio::test]
async fn responses_store_false_does_not_retain_provider_client_context() {
    let (_directory, state, provider) = provider_state().await;
    let body = json!({"model":"copilot/model","input":"hello","store":false,"client_metadata":client_metadata()});
    let response = test_router(state.clone())
        .oneshot(post("/v1/responses", &body))
        .await
        .unwrap();
    let _ = body_json(response).await;
    let continuation =
        json!({"model":"copilot/model","previous_response_id":"resp_fixture_1","input":"continue"});
    let response = test_router(state)
        .oneshot(post("/v1/responses", &continuation))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let _ = body_json(response).await;
    assert!(provider.requests.lock().unwrap()[1]
        .client_context
        .is_unknown());
}

#[tokio::test]
async fn kiro_responses_recover_omitted_context_from_owned_response_history() {
    let (_directory, state) = test_state(Config::default()).await;
    let body = json!({"model":"model","input":"hello","client_metadata":client_metadata()});
    let context = client_context::extract(&HeaderMap::new(), &serde_json::to_vec(&body).unwrap());
    let request = serde_json::from_value(body).unwrap();
    let translated = crosslane_translate::responses_to_openai(&request).unwrap();
    let owner = responses::ResponsesSessionOwner::new("test".into(), None);
    let mut options = responses::ResponsesOptions::new(
        &request,
        translated.tools,
        translated.tool_names,
        Some((state.responses_sessions.clone(), owner)),
        Some("resolved-conversation".into()),
    );
    options.set_client_context(&context);
    let response_id = options.id().to_owned();
    responses::json_response(
        json!({"choices":[{"message":{"content":"ready"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":1,"completion_tokens":1}}),
        options,
    )
    .unwrap();
    let continuation =
        json!({"model":"model","previous_response_id":response_id,"input":"continue"});
    let response = router(state.clone())
        .oneshot(post("/v1/responses", &continuation))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let _ = body_json(response).await;
    assert_eq!(
        state.stats.snapshot(None).recent_requests[0]
            .diagnostics
            .client_context,
        Some(context)
    );
}
