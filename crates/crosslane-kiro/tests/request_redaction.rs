//! Verify redaction at the actual HTTP boundary, using synthetic credentials only.

use base64::Engine;
use crosslane_core::{
    account::{Account, AuthMethod, Credentials},
    config::UpstreamConfig,
};
use crosslane_kiro::{endpoint::EndpointOverrides, EndpointKey, KiroClient, KiroEvent};
use crosslane_translate::{
    claude_to_kiro, compaction_summary_payload, sanitize_kiro_tool_history, ClaudeRequest,
    KiroDocument, KiroDocumentSource, KiroPayload, RedactionSession, TokenCountCache,
    TranslationOptions,
};
use serde_json::{json, Value};
use wiremock::{
    matchers::{header, method, path},
    Mock, MockServer, ResponseTemplate,
};

const PLACEHOLDER_PREFIX: &str = "__CROSSLANE_SECRET_";

const ACCESS: &str = "opaque-upstream-access-credential-7821";
const REFRESH: &str = "opaque-upstream-refresh-credential-3492";
const CLIENT_SECRET: &str = "opaque-upstream-client-secret-4928";
const PROFILE: &str = "arn:aws:codewhisperer:us-east-1:123456789012:profile/audit-profile";
const SECRETS: &[&str] = &[
    "ksk_fixture_system_123",
    "ksk_fixture_history_123",
    "ksk_fixture_current_123",
    "ksk_fixture_description_123",
    "ksk_fixture_schema_default_123",
    "ksk_fixture_command_123",
    "ksk_fixture_result_123",
    "opaque-tool-api-key-value",
    "opaque tool password with spaces",
    "admin:admin",
    ACCESS,
    REFRESH,
    CLIENT_SECRET,
];

fn account(auth_method: AuthMethod) -> Account {
    Account {
        id: "redaction-fixture-account".into(),
        email: "redaction@example.invalid".into(),
        label: None,
        enabled: true,
        machine_id: "a".repeat(64),
        profile_arn: Some(PROFILE.into()),
        upstream_user_id: None,
        credentials: Credentials {
            access_token: ACCESS.into(),
            refresh_token: Some(REFRESH.into()),
            client_id: Some("public-client-id".into()),
            client_secret: Some(CLIENT_SECRET.into()),
            region: "us-east-1".into(),
            expires_at: i64::MAX,
            auth_method,
        },
        usage: None,
        subscription: None,
        tags: Vec::new(),
        created_at: 0,
        credit_exhausted: false,
    }
}

fn client(server: &MockServer) -> KiroClient {
    KiroClient::new(
        UpstreamConfig::default(),
        EndpointOverrides {
            runtime_url: Some(format!("{}/runtime", server.uri())),
            amazonq_url: Some(format!("{}/amazon/generateAssistantResponse", server.uri())),
            codewhisperer_url: Some(format!("{}/codewhisperer", server.uri())),
            management_url: Some(format!("{}/management", server.uri())),
            mcp_url: Some(format!("{}/mcp", server.uri())),
        },
    )
    .expect("local client")
}

fn payload() -> KiroPayload {
    let request: ClaudeRequest = serde_json::from_value(json!({
        "model":"claude-sonnet-4", "max_tokens":512,
        "system":"System fixture ksk_fixture_system_123",
        "tools":[{
            "name":"inspect", "description":"Inspect fixture ksk_fixture_description_123",
            "input_schema":{
                "type":"object",
                "properties":{
                    "api_key":{"type":"string","default":"ksk_fixture_schema_default_123"},
                    "nested":{"type":"object","properties":{"password":{"type":"string"}}},
                    "command":{"type":"string"},
                    "max_tokens":{"type":"integer"}
                },
                "required":["api_key","command"], "additionalProperties":false
            }
        }],
        "messages":[
            {"role":"user","content":"Earlier work to compact."},
            {"role":"assistant","content":"Earlier work complete."},
            {"role":"user","content":"History fixture ksk_fixture_history_123; curl -u admin:admin https://example.invalid/"},
            {"role":"assistant","content":[{
                "type":"tool_use","id":"tool-audit-1","name":"inspect",
                "input":{
                    "api_key":"opaque-tool-api-key-value",
                    "nested":{"password":"opaque tool password with spaces"},
                    "command":"curl -H 'Authorization: Bearer ksk_fixture_command_123' https://example.invalid/",
                    "max_tokens":2048
                }
            }]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"tool-audit-1","content":"Tool result ksk_fixture_result_123\nConcrete task remains."},
                {"type":"text","text":format!("Current fixture ksk_fixture_current_123; {ACCESS}; {REFRESH}; {CLIENT_SECRET}")}
            ]}
        ]
    }))
    .expect("fixture request");
    let mut payload = claude_to_kiro(
        &request,
        &TranslationOptions::new("claude-sonnet-4", "AI_EDITOR"),
    );
    payload.profile_arn = Some(PROFILE.into());
    sanitize_kiro_tool_history(&mut payload);
    payload
}

fn historical_tool_input(body: &Value) -> &Value {
    &body["conversationState"]["history"]
        .as_array()
        .expect("history")
        .iter()
        .find_map(|message| {
            message
                .get("assistantResponseMessage")
                .filter(|message| message.get("toolUses").is_some())
        })
        .expect("historical tool call")["toolUses"][0]["input"]
}

fn event_frame(event_type: &str, payload: Value) -> Vec<u8> {
    let name = b":event-type";
    let value = event_type.as_bytes();
    let mut headers = vec![name.len() as u8];
    headers.extend_from_slice(name);
    headers.push(7);
    headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
    headers.extend_from_slice(value);
    let payload = serde_json::to_vec(&payload).unwrap();
    let total = 16 + headers.len() + payload.len();
    let mut frame = Vec::with_capacity(total);
    frame.extend_from_slice(&(total as u32).to_be_bytes());
    frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    frame.extend_from_slice(&crc32fast::hash(&frame).to_be_bytes());
    frame.extend_from_slice(&headers);
    frame.extend_from_slice(&payload);
    frame.extend_from_slice(&crc32fast::hash(&frame).to_be_bytes());
    frame
}

fn returned_tool_input(events: &[KiroEvent], tool_id: &str) -> Value {
    returned_tool_input_with_name(events, tool_id, "inspect")
}

fn returned_tool_input_with_name(
    events: &[KiroEvent],
    tool_id: &str,
    expected_name: &str,
) -> Value {
    let mut input = String::new();
    let mut stopped = false;
    for event in events {
        if let KiroEvent::ToolUse {
            id,
            name,
            input_delta,
            stop,
        } = event
        {
            if id == tool_id {
                assert!(name.is_empty() || name == expected_name);
                input.push_str(input_delta);
                stopped |= stop;
            }
        }
    }
    assert!(stopped, "tool completion retained");
    serde_json::from_str(&input).expect("restored arguments remain valid JSON")
}

async fn mount_generation(server: &MockServer, endpoint_path: &str) {
    Mock::given(method("POST"))
        .and(path(endpoint_path))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(server)
        .await;
}

fn assert_redacted(body: &Value) {
    let encoded = body.to_string();
    for secret in SECRETS {
        assert!(
            !encoded.contains(secret),
            "synthetic secret reached upstream"
        );
    }
    assert!(encoded.contains(PLACEHOLDER_PREFIX));
    assert!(encoded.contains("Concrete task remains."));
    let state = &body["conversationState"];
    assert_eq!(
        state["currentMessage"]["userInputMessage"]["modelId"],
        "claude-sonnet-4"
    );
    assert_eq!(body["inferenceConfig"]["maxTokens"], 512);
    let assistant = state["history"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|message| {
            message
                .get("assistantResponseMessage")
                .filter(|message| message.get("toolUses").is_some())
        })
        .expect("structured tool call retained");
    assert_eq!(assistant["toolUses"][0]["toolUseId"], "tool-audit-1");
    assert_eq!(assistant["toolUses"][0]["name"], "inspect");
    assert_eq!(assistant["toolUses"][0]["input"]["max_tokens"], 2048);
    let context = &state["currentMessage"]["userInputMessage"]["userInputMessageContext"];
    assert_eq!(context["toolResults"][0]["toolUseId"], "tool-audit-1");
    let specification = &context["tools"][0]["toolSpecification"];
    assert_eq!(specification["name"], "inspect");
    let schema = &specification["inputSchema"]["json"];
    let key_schema = &schema["properties"]["api_key"];
    assert_eq!(key_schema["type"], "string");
    assert!(key_schema["default"]
        .as_str()
        .unwrap()
        .starts_with(PLACEHOLDER_PREFIX));
    // This fixture only maps strings, so unrelated numeric properties retain
    // their original schema instead of accepting unusable string references.
    assert_eq!(schema["properties"]["max_tokens"]["type"], "integer");
    assert_eq!(schema["required"], json!(["api_key", "command"]));
    assert_eq!(schema["additionalProperties"], false);
}

#[tokio::test]
async fn runtime_and_legacy_generation_redact_without_changing_auth_or_source() {
    for (auth_method, endpoint, endpoint_path) in [
        (AuthMethod::ApiKey, EndpointKey::Runtime, "/runtime"),
        (
            AuthMethod::Idc,
            EndpointKey::Amazonq,
            "/amazon/generateAssistantResponse",
        ),
        (
            AuthMethod::Social,
            EndpointKey::Codewhisperer,
            "/codewhisperer",
        ),
    ] {
        let server = MockServer::start().await;
        mount_generation(&server, endpoint_path).await;
        let source = payload();
        let original = serde_json::to_value(&source).unwrap();
        let response = client(&server)
            .generate(&account(auth_method), &source, Some(endpoint))
            .await
            .expect("local generation succeeds");
        assert_eq!(response.endpoint.key, endpoint);
        assert_eq!(serde_json::to_value(&source).unwrap(), original);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "no discovery or other network request");
        assert_eq!(
            requests[0].headers["authorization"],
            format!("Bearer {ACCESS}")
        );
        assert_eq!(requests[0].headers["x-amzn-codewhisperer-optout"], "true");
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_redacted(&body);
        assert_eq!(
            body["conversationState"]["conversationId"],
            original["conversationState"]["conversationId"]
        );
        if endpoint == EndpointKey::Runtime {
            assert!(body.get("profileArn").is_none());
        }
    }
}

#[tokio::test]
async fn common_auth_forms_are_filtered_before_send_and_recover_working_curl_arguments() {
    for enabled in [true, false] {
        let server = MockServer::start().await;
        let command = format!(
            "curl -q --noproxy '*' --fail --silent --show-error --max-time 5 -u 'wire-user:wire-password-123' {}/protected",
            server.uri()
        );
        let input = json!({
            "command":command, "timeout":10000,
            "headers":{"Access-Control-Allow-Credentials":"true"},
            "has_password":true,
            "env_command":"curl -u \"$BASIC_USER:$BASIC_PASSWORD\" https://example.invalid/",
            "fetch_options":{"credentials":"include","method":"GET"},
            "authorization_code_request":{"grant_type":"authorization_code","code":"opaque-code-fixture"},
            "docker":{"auth":"dXNlcjpvcGFxdWUtZG9ja2VyLWZpeHR1cmU="},
            "certificate":"curl --cert client.p12:opaque-p12-fixture --key /run/secrets/client.pem https://example.invalid/"
        });
        let request: ClaudeRequest = serde_json::from_value(json!({
            "model":"claude-sonnet-4", "max_tokens":512,
            "tools":[{"name":"Bash","description":"Execute a command","input_schema":{"type":"object"}}],
            "messages":[
                {"role":"user","content":"Run curl -u admin:admin https://example.invalid/. auth=('user','opaque-sdk-fixture')"},
                {"role":"assistant","content":[{"type":"tool_use","id":"auth-history","name":"Bash","input":input}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"auth-history","content":"access-control-allow-credentials: true\nwire-password-123 was accepted\nServer=localhost;Password=opaque-db-fixture;Database=appdb"}]}
            ]
        })).unwrap();
        let mut source = claude_to_kiro(
            &request,
            &TranslationOptions::new("claude-sonnet-4", "AI_EDITOR"),
        );
        source.redaction_session = Some(RedactionSession::with_enabled(enabled));
        let original = serde_json::to_value(&source).unwrap();
        let expected = input.clone();
        Mock::given(method("POST")).and(path("/runtime"))
            .respond_with(move |request: &wiremock::Request| {
                let wire: Value = serde_json::from_slice(&request.body).unwrap();
                let encoded = wire.to_string();
                for secret in ["admin:admin", "wire-password-123", "opaque-code-fixture", "opaque-sdk-fixture", "opaque-db-fixture", "opaque-p12-fixture", "dXNlcjpvcGFxdWUtZG9ja2VyLWZpeHR1cmU="] {
                    assert_eq!(encoded.contains(secret), !enabled, "wire policy for synthetic {secret}");
                }
                let tool = historical_tool_input(&wire);
                for field in ["headers", "has_password", "env_command", "fetch_options", "timeout"] {
                    assert_eq!(tool[field], expected[field], "public control changed");
                }
                assert!(encoded.contains("access-control-allow-credentials: true"));
                let encoded = tool.to_string();
                let split = encoded.find(PLACEHOLDER_PREFIX).map_or(encoded.len() / 2, |offset| offset + 8);
                let mut response = event_frame("toolUseEvent", json!({"toolUseId":"auth-returned","name":"Bash","input":&encoded[..split],"stop":false}));
                response.extend(event_frame("toolUseEvent", json!({"toolUseId":"auth-returned","name":"Bash","input":&encoded[split..],"stop":true})));
                ResponseTemplate::new(200).set_body_bytes(response)
            }).expect(1).mount(&server).await;
        Mock::given(method("GET"))
            .and(path("/protected"))
            .and(header(
                "authorization",
                format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode("wire-user:wire-password-123")
                ),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string("authenticated-fixture"))
            .expect(1)
            .mount(&server)
            .await;
        let client = client(&server);
        let response = client
            .generate(
                &account(AuthMethod::ApiKey),
                &source,
                Some(EndpointKey::Runtime),
            )
            .await
            .unwrap();
        let (_, response, _permit) = response.into_parts();
        let events = client.collect_events(response).await.unwrap();
        let restored = returned_tool_input_with_name(&events, "auth-returned", "Bash");
        assert_eq!(restored, input);
        // Execute only this synthetic command against the loopback fixture.
        // No httpbin, real credentials, external provider, or shell expansion.
        let output = tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(restored["command"].as_str().unwrap())
            .env_clear()
            .env(
                "PATH",
                std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
            )
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"authenticated-fixture");
        assert_eq!(serde_json::to_value(&source).unwrap(), original);
    }
}

#[tokio::test]
async fn returned_tool_inputs_restore_secrets_types_and_fragmented_placeholders() {
    const PASSWORD: &str = "quoted \"password\" with \\ slash\n第二行";
    const FOREIGN: &str = "__CROSSLANE_SECRET_00000000000000000000000000000000_999__";
    for fragmented in [false, true] {
        let server = MockServer::start().await;
        let mut source = payload();
        let input = &mut source
            .conversation_state
            .history
            .iter_mut()
            .filter_map(|message| message.assistant_response_message.as_mut())
            .find(|message| !message.tool_uses.is_empty())
            .unwrap()
            .tool_uses[0]
            .input;
        input["password"] = json!(PASSWORD);
        input["password_repeat"] = json!(PASSWORD);
        input["embedded"] = json!(format!("prefix {PASSWORD} suffix"));
        input["numeric_password"] = json!(43218765);
        input["enabled_secret"] = json!(true);
        input["env_token"] = json!("$API_TOKEN");
        input["brace_token"] = json!("${API_TOKEN}");
        input["powershell_token"] = json!("$env:API_TOKEN");
        input["windows_token"] = json!("%API_TOKEN%");
        input["headers"] = json!({"Authorization":"Bearer ${API_TOKEN}"});
        input["command_env"] =
            json!("curl -H 'Authorization: Bearer $API_TOKEN' https://example.invalid/");
        let before = serde_json::to_value(&source).unwrap();
        let expected = historical_tool_input(&before).clone();
        let expected_wire = expected.clone();
        Mock::given(method("POST"))
            .and(path("/runtime"))
            .respond_with(move |request: &wiremock::Request| {
                let wire: Value = serde_json::from_slice(&request.body).unwrap();
                let mut tool_input = historical_tool_input(&wire).clone();
                for name in ["api_key", "password", "numeric_password", "enabled_secret"] {
                    let marker = tool_input[name]
                        .as_str()
                        .expect("typed secret uses a marker");
                    assert!(marker.starts_with(PLACEHOLDER_PREFIX));
                    assert_ne!(tool_input[name], expected_wire[name]);
                }
                assert_eq!(tool_input["password"], tool_input["password_repeat"]);
                assert_ne!(tool_input["api_key"], tool_input["password"]);
                for name in [
                    "env_token",
                    "brace_token",
                    "powershell_token",
                    "windows_token",
                    "headers",
                    "command_env",
                ] {
                    assert_eq!(
                        tool_input[name], expected_wire[name],
                        "symbolic reference changed"
                    );
                }
                assert!(!wire.to_string().contains("opaque-tool-api-key-value"));
                assert!(!wire.to_string().contains("ksk_fixture_command_123"));
                tool_input["foreign_reference"] = json!(FOREIGN);
                let encoded = tool_input.to_string();
                let mut response = event_frame(
                    "assistantResponseEvent",
                    json!({"content":format!("Calling inspect with {}.", tool_input["api_key"].as_str().unwrap())}),
                );
                if fragmented {
                    let marker_start = encoded.find(PLACEHOLDER_PREFIX).unwrap();
                    let first_split = marker_start + PLACEHOLDER_PREFIX.len() / 2;
                    let second_split = marker_start + PLACEHOLDER_PREFIX.len() + 5;
                    for (index, part) in [
                        &encoded[..first_split],
                        &encoded[first_split..second_split],
                        &encoded[second_split..],
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        response.extend(event_frame(
                            "toolUseEvent",
                            json!({
                                "toolUseId":"returned-tool", "name":"inspect",
                                "input":part, "stop":index == 2,
                            }),
                        ));
                    }
                } else {
                    response.extend(event_frame(
                        "toolUseEvent",
                        json!({
                            "toolUseId":"returned-tool", "name":"inspect",
                            "input":encoded, "stop":true,
                        }),
                    ));
                }
                ResponseTemplate::new(200).set_body_bytes(response)
            })
            .expect(1)
            .mount(&server)
            .await;
        let client = client(&server);
        let response = client
            .generate(
                &account(AuthMethod::ApiKey),
                &source,
                Some(EndpointKey::Runtime),
            )
            .await
            .expect("local model response");
        let (_, response, _permit) = response.into_parts();
        let events = client
            .collect_events(response)
            .await
            .expect("restored events");
        let mut expected = expected;
        expected["foreign_reference"] = json!(FOREIGN);
        let restored = returned_tool_input(&events, "returned-tool");
        assert_eq!(restored, expected);
        // Simulate the caller's HTTP tool: the restored token must actually
        // authenticate, rather than forwarding an opaque model placeholder.
        Mock::given(method("GET"))
            .and(path("/protected"))
            .and(header("authorization", "Bearer opaque-tool-api-key-value"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let authenticated = reqwest::Client::new()
            .get(format!("{}/protected", server.uri()))
            .bearer_auth(restored["api_key"].as_str().unwrap())
            .send()
            .await
            .expect("local authenticated tool request");
        assert_eq!(authenticated.status(), 200);
        assert_eq!(serde_json::to_value(&source).unwrap(), before);
        assert!(events.iter().any(|event| matches!(event,
            KiroEvent::AssistantResponse { content }
                if content.starts_with("Calling inspect with ")
                    && content.contains(PLACEHOLDER_PREFIX)
                    && !content.contains("opaque-tool-api-key-value")
        )));
    }
}

#[tokio::test]
async fn placeholder_mappings_are_isolated_between_requests() {
    let server = MockServer::start().await;
    let first_marker = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let captured = first_marker.clone();
    Mock::given(method("POST"))
        .and(path("/runtime"))
        .respond_with(move |request: &wiremock::Request| {
            let wire: Value = serde_json::from_slice(&request.body).unwrap();
            let marker = historical_tool_input(&wire)["api_key"]
                .as_str()
                .unwrap()
                .to_owned();
            assert!(marker.starts_with(PLACEHOLDER_PREFIX));
            let mut first = captured.lock().unwrap();
            let previous = if let Some(first) = first.as_ref() {
                assert_ne!(first, &marker, "requests must not share placeholder identities");
                first.clone()
            } else {
                *first = Some(marker.clone());
                "ordinary-first-request".into()
            };
            let input = json!({"current_secret":marker,"previous_secret":previous});
            ResponseTemplate::new(200).set_body_bytes(event_frame("toolUseEvent", json!({
                "toolUseId":"isolated-tool", "name":"inspect", "input":input.to_string(), "stop":true,
            })))
        })
        .expect(2)
        .mount(&server)
        .await;
    let client = client(&server);
    for index in 0..2 {
        let response = client
            .generate(
                &account(AuthMethod::ApiKey),
                &payload(),
                Some(EndpointKey::Runtime),
            )
            .await
            .unwrap();
        let (_, response, _permit) = response.into_parts();
        let events = client.collect_events(response).await.unwrap();
        let input = returned_tool_input(&events, "isolated-tool");
        assert_eq!(input["current_secret"], "opaque-tool-api-key-value");
        if index == 1 {
            assert_eq!(
                input["previous_secret"],
                first_marker.lock().unwrap().as_ref().unwrap().as_str()
            );
        } else {
            assert_eq!(input["previous_secret"], "ordinary-first-request");
        }
    }
}

#[tokio::test]
async fn interleaved_tool_calls_restore_each_argument_buffer_independently() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/runtime"))
        .respond_with(|request: &wiremock::Request| {
            let wire: Value = serde_json::from_slice(&request.body).unwrap();
            let input = historical_tool_input(&wire);
            let first = json!({"authorization":format!("Bearer {}", input["api_key"].as_str().unwrap()),"count":2}).to_string();
            let second = json!({"nested":{"password":input["nested"]["password"]},"count":3}).to_string();
            let first_split = first.len() / 2;
            let second_split = second.len() / 2;
            let mut frames = Vec::new();
            for (id, part, stop) in [
                ("first-tool", &first[..first_split], false),
                ("second-tool", &second[..second_split], false),
                ("first-tool", &first[first_split..], true),
                ("second-tool", &second[second_split..], true),
            ] {
                frames.extend(event_frame("toolUseEvent", json!({
                    "toolUseId":id, "name":"inspect", "input":part, "stop":stop,
                })));
            }
            ResponseTemplate::new(200).set_body_bytes(frames)
        })
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server);
    let response = client
        .generate(
            &account(AuthMethod::ApiKey),
            &payload(),
            Some(EndpointKey::Runtime),
        )
        .await
        .unwrap();
    let (_, response, _permit) = response.into_parts();
    let events = client.collect_events(response).await.unwrap();
    assert_eq!(
        returned_tool_input(&events, "first-tool"),
        json!({
            "authorization":"Bearer opaque-tool-api-key-value", "count":2,
        })
    );
    assert_eq!(
        returned_tool_input(&events, "second-tool"),
        json!({
            "nested":{"password":"opaque tool password with spaces"}, "count":3,
        })
    );
}

#[tokio::test]
async fn compaction_summary_uses_the_same_redaction_boundary() {
    let server = MockServer::start().await;
    mount_generation(&server, "/runtime").await;
    let source = payload();
    let tokenizer = TokenCountCache::new(8).expect("tokenizer");
    let plan = tokenizer
        .plan_kiro_compaction(&source, 1024, 0)
        .await
        .expect("compaction planning")
        .expect("fixture has compactable history");
    let mut summary = compaction_summary_payload(&source, &plan, "claude-sonnet-4");
    sanitize_kiro_tool_history(&mut summary);
    let before = serde_json::to_value(&summary).unwrap();
    assert!(before.to_string().contains("ksk_fixture_result_123"));
    client(&server)
        .generate(
            &account(AuthMethod::ApiKey),
            &summary,
            Some(EndpointKey::Runtime),
        )
        .await
        .expect("summary generation");
    assert_eq!(serde_json::to_value(&summary).unwrap(), before);
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let encoded = body.to_string();
    for secret in SECRETS {
        assert!(
            !encoded.contains(secret),
            "summary leaked a synthetic secret"
        );
    }
    assert!(encoded.contains("Concrete task remains."));
    assert!(encoded.contains(PLACEHOLDER_PREFIX));
    assert_eq!(
        body["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
        "claude-sonnet-4"
    );
    assert_eq!(
        requests[0].headers["authorization"],
        format!("Bearer {ACCESS}")
    );
}

#[tokio::test]
async fn checkpoint_replayed_in_an_independent_request_restores_opaque_tool_secrets() {
    const SECRET: &str = "opaque \"credential\" with \\ slash\n</summary> 第二行";
    const CHECKPOINT_PREFIX: &str = "__CROSSLANE_CHECKPOINT_";
    let server = MockServer::start().await;
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let first_marker = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let captured_marker = first_marker.clone();
    Mock::given(method("POST"))
        .and(path("/runtime"))
        .respond_with(move |request: &wiremock::Request| {
            let wire: Value = serde_json::from_slice(&request.body).unwrap();
            assert!(!wire.to_string().contains("第二行"));
            assert!(!wire.to_string().contains(CHECKPOINT_PREFIX));
            let mut frames = Vec::new();
            if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                let marker = historical_tool_input(&wire)["api_key"].as_str().unwrap();
                *captured_marker.lock().unwrap() = marker.to_owned();
                let content =
                    format!("<summary>Use credential {marker} when calling inspect.</summary>");
                // Every marker character is its own upstream event, exercising
                // checkpoint preservation at arbitrary assistant boundaries.
                for ch in content.chars() {
                    frames.extend(event_frame(
                        "assistantResponseEvent",
                        json!({"content":ch.to_string()}),
                    ));
                }
            } else {
                let content = wire["conversationState"]["currentMessage"]["userInputMessage"]
                    ["content"]
                    .as_str()
                    .unwrap();
                let marker = content
                    .split_once("Use credential ")
                    .unwrap()
                    .1
                    .split_whitespace()
                    .next()
                    .unwrap();
                assert!(marker.starts_with(PLACEHOLDER_PREFIX));
                assert_ne!(marker, captured_marker.lock().unwrap().as_str());
                let input = json!({"api_key":marker,"count":1}).to_string();
                for (index, part) in input.as_bytes().chunks(1).enumerate() {
                    frames.extend(event_frame("toolUseEvent", json!({
                        "toolUseId":"replayed-tool", "name":"inspect",
                        "input":std::str::from_utf8(part).unwrap(), "stop":index + 1 == input.len(),
                    })));
                }
            }
            ResponseTemplate::new(200).set_body_bytes(frames)
        })
        .expect(2)
        .mount(&server)
        .await;
    let client = client(&server);
    let mut source = payload();
    source.restore_summary_secrets = true;
    source
        .conversation_state
        .history
        .iter_mut()
        .filter_map(|item| item.assistant_response_message.as_mut())
        .flat_map(|message| message.tool_uses.iter_mut())
        .next()
        .unwrap()
        .input["api_key"] = json!(SECRET);
    let response = client
        .generate(
            &account(AuthMethod::ApiKey),
            &source,
            Some(EndpointKey::Runtime),
        )
        .await
        .unwrap();
    let (_, response, _permit) = response.into_parts();
    let summary: String = client
        .collect_events(response)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|event| match event {
            KiroEvent::AssistantResponse { content } => Some(content),
            _ => None,
        })
        .collect();
    assert!(summary.contains(CHECKPOINT_PREFIX));
    assert!(!summary.contains("opaque"));
    assert_eq!(summary.matches("</summary>").count(), 1);
    let replay: ClaudeRequest = serde_json::from_value(json!({
        "model":"claude-sonnet-4", "max_tokens":128,
        "tools":[{"name":"inspect","input_schema":{"type":"object"}}],
        "messages":[{"role":"user","content":summary}]
    }))
    .unwrap();
    let replay = claude_to_kiro(
        &replay,
        &TranslationOptions::new("claude-sonnet-4", "AI_EDITOR"),
    );
    let response = client
        .generate(
            &account(AuthMethod::ApiKey),
            &replay,
            Some(EndpointKey::Runtime),
        )
        .await
        .unwrap();
    let (_, response, _permit) = response.into_parts();
    let events = client.collect_events(response).await.unwrap();
    assert_eq!(
        returned_tool_input(&events, "replayed-tool"),
        json!({"api_key":SECRET,"count":1})
    );
}

#[tokio::test]
async fn text_attachments_are_decoded_redacted_and_encoded_without_mutating_source() {
    let server = MockServer::start().await;
    mount_generation(&server, "/runtime").await;
    let mut source = payload();
    source
        .conversation_state
        .current_message
        .user_input_message
        .documents
        .push(KiroDocument {
            format: "txt".into(),
            name: "configuration".into(),
            source: KiroDocumentSource {
                bytes: base64::engine::general_purpose::STANDARD
                    .encode("password=opaque-document-password\nkeep this task detail"),
            },
            citations: None,
        });
    let before = serde_json::to_value(&source).unwrap();
    client(&server)
        .generate(
            &account(AuthMethod::ApiKey),
            &source,
            Some(EndpointKey::Runtime),
        )
        .await
        .expect("generation with a text attachment");
    assert_eq!(serde_json::to_value(&source).unwrap(), before);
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let bytes = body["conversationState"]["currentMessage"]["userInputMessage"]["documents"][0]
        ["source"]["bytes"]
        .as_str()
        .unwrap();
    let text = String::from_utf8(
        base64::engine::general_purpose::STANDARD
            .decode(bytes)
            .expect("valid Base64 after redaction"),
    )
    .unwrap();
    assert!(!text.contains("opaque-document-password"));
    assert!(text.contains(PLACEHOLDER_PREFIX));
    assert!(text.contains("keep this task detail"));
}

#[tokio::test]
async fn web_search_redacts_queries_on_native_and_compatibility_mcp() {
    for native in [false, true] {
        let server = MockServer::start().await;
        let endpoint_path = if native { "/runtime" } else { "/mcp" };
        Mock::given(method("POST"))
            .and(path(endpoint_path))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc":"2.0", "id":"fixture-response",
                "result":{"isError":false,"content":[{"type":"text","text":json!({
                    "query":"", "totalResults":0, "results":[]
                }).to_string()}]}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = KiroClient::new(
            UpstreamConfig::default(),
            EndpointOverrides {
                runtime_url: Some(format!("{}/runtime", server.uri())),
                mcp_url: (!native).then(|| format!("{}/mcp", server.uri())),
                ..Default::default()
            },
        )
        .unwrap();
        let results = client
            .web_search(
                &account(AuthMethod::Idc),
                &format!("documentation ksk_fixture_search_123 plus {ACCESS}"),
            )
            .await
            .expect("local web search");
        assert!(results.query.contains("documentation"));
        assert!(results.query.contains("ksk_fixture_search_123"));
        assert!(results.query.contains(ACCESS));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].headers["authorization"],
            format!("Bearer {ACCESS}")
        );
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["method"], "tools/call");
        assert_eq!(body["params"]["name"], "web_search");
        assert!(!body.to_string().contains("ksk_fixture_search_123"));
        assert!(!body.to_string().contains(ACCESS));
        assert!(body["params"]["arguments"]["query"]
            .as_str()
            .unwrap()
            .contains(PLACEHOLDER_PREFIX));
        if native {
            assert_eq!(body["profileArn"], PROFILE);
            assert_eq!(
                requests[0].headers["x-amz-target"],
                "KiroRuntimeService.InvokeMCP"
            );
        }
    }
}

#[tokio::test]
async fn web_search_reuses_generation_secrets_on_native_and_compatibility_mcp() {
    const SECRET: &str = "opaque-labeled-cloud-secret-7821";
    for native in [false, true] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(if native { "/runtime" } else { "/mcp" }))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc":"2.0", "id":"fixture-response",
                "result":{"isError":false,"content":[{"type":"text","text":json!({
                    "query":"", "totalResults":0, "results":[]
                }).to_string()}]}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let client = KiroClient::new(
            UpstreamConfig::default(),
            EndpointOverrides {
                runtime_url: Some(format!("{}/runtime", server.uri())),
                mcp_url: (!native).then(|| format!("{}/mcp", server.uri())),
                ..Default::default()
            },
        )
        .unwrap();
        let session = RedactionSession::new();
        let learned = session.redact_text(&format!("SecretKey={SECRET}"), &[]);
        assert!(!learned.contains(SECRET));
        let query = format!("documentation for {SECRET}");
        let results = client
            .web_search_with_redaction(&account(AuthMethod::Idc), &query, &session)
            .await
            .unwrap();
        assert_eq!(results.query, query);
        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(!body.to_string().contains(SECRET));
        assert!(body["params"]["arguments"]["query"]
            .as_str()
            .unwrap()
            .contains(PLACEHOLDER_PREFIX));
        assert_eq!(
            requests[0].headers["authorization"],
            format!("Bearer {ACCESS}")
        );
        if native {
            assert_eq!(
                requests[0].headers["x-amz-target"],
                "KiroRuntimeService.InvokeMCP"
            );
        }
    }
}

#[cfg(feature = "crs")]
#[tokio::test]
async fn isolated_runtime_retry_also_redacts_and_preserves_prepared_profile() {
    let server = MockServer::start().await;
    mount_generation(&server, "/runtime").await;
    let source = payload();
    let original = serde_json::to_value(&source).unwrap();
    client(&server)
        .generate_at_endpoint_isolated(&account(AuthMethod::Idc), &source, EndpointKey::Runtime)
        .await
        .expect("isolated generation");
    assert_eq!(serde_json::to_value(&source).unwrap(), original);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_redacted(&body);
    assert_eq!(body["profileArn"], PROFILE);
    assert_eq!(
        requests[0].headers["authorization"],
        format!("Bearer {ACCESS}")
    );
}
