//! Request-local on/off policy and compatibility with older checkpoints.

use base64::Engine;
use crosslane_translate::{
    auto_continue_payload, claude_to_kiro, compaction_summary_payload, openai_to_kiro,
    prepare_redacted_kiro_payload, ClaudeRequest, KiroPayload, OpenAiRequest, RedactionSession,
    TokenCountCache, TranslationOptions,
};
use serde_json::{json, Value};

fn payload(input: Value, content: &str) -> KiroPayload {
    serde_json::from_value(json!({
        "conversationState": {
            "chatTriggerType":"MANUAL", "conversationId":"toggle-fixture",
            "history":[{"assistantResponseMessage":{
                "content":"Previous content", "toolUses":[{
                    "toolUseId":"request-call", "name":"http_request", "input":input
                }]
            }}],
            "currentMessage":{"userInputMessage":{
                "content":content, "modelId":"claude-sonnet-4", "origin":"AI_EDITOR",
                "userInputMessageContext":{"tools":[{"toolSpecification":{
                    "name":"http_request", "description":"Send a request",
                    "inputSchema":{"json":{"type":"object","properties":{
                        "password":{"type":"string","maxLength":8},
                        "token":{"type":"integer","minimum":1}
                    },"additionalProperties":false}}
                }}]}
            }}
        }
    }))
    .unwrap()
}

fn input(payload: &KiroPayload) -> &Value {
    &payload.conversation_state.history[0]
        .assistant_response_message
        .as_ref()
        .unwrap()
        .tool_uses[0]
        .input
}

#[test]
fn disabled_session_preserves_plain_data_schemas_and_original_json_bytes() {
    let literal = "a'b\"c $TOKEN ;literal";
    let json_body = " {\n  \"password\" : \"opaque-fixture\", \"n\" : 1e3\n } ";
    let original = json!({
        "password":literal, "token":1234, "secret":true,
        "body":json_body,
        "command":"TOKEN='opaque-fixture' curl --token $TOKEN",
        "Authorization":"Bearer ksk_synthetic_toggle_token"
    });
    let mut source = payload(original.clone(), json_body);
    let session = RedactionSession::with_enabled(false);
    source.redaction_session = Some(session.clone());
    let wire_before = serde_json::to_value(&source).unwrap();
    let sent = prepare_redacted_kiro_payload(&source, &[literal, "opaque-fixture"]);
    assert_eq!(serde_json::to_value(&sent).unwrap(), wire_before);
    assert_eq!(input(&sent), &original);
    assert!(session.is_empty());
    assert!(!session.is_enabled());
    assert_eq!(
        session.redact_text("token=ksk_synthetic_toggle_token", &[]),
        "token=ksk_synthetic_toggle_token"
    );
    let mut returned = input(&sent).clone();
    session.restore_tool_input_for_tool("Bash", &mut returned);
    assert_eq!(returned, original);
    assert_eq!(
        serde_json::to_value(prepare_redacted_kiro_payload(&sent, &[])).unwrap(),
        wire_before
    );
}

#[test]
fn translator_options_freeze_the_policy_for_both_ingress_protocols() {
    let mut options = TranslationOptions::new("model", "AI_EDITOR");
    options.enable_request_redaction = false;
    let claude: ClaudeRequest = serde_json::from_value(json!({
        "model":"model", "max_tokens":128,
        "messages":[{"role":"user","content":"password=opaque-toggle-fixture"}]
    }))
    .unwrap();
    let openai: OpenAiRequest = serde_json::from_value(json!({
        "model":"model",
        "messages":[{"role":"user","content":"password=opaque-toggle-fixture"}]
    }))
    .unwrap();
    for source in [
        claude_to_kiro(&claude, &options),
        openai_to_kiro(&openai, &options),
    ] {
        assert!(!source.redaction_session.as_ref().unwrap().is_enabled());
        let sent = prepare_redacted_kiro_payload(&source, &[]);
        assert!(serde_json::to_string(&sent)
            .unwrap()
            .contains("opaque-toggle-fixture"));
        assert!(sent.redaction_session.as_ref().unwrap().is_empty());
    }
    assert!(RedactionSession::new().is_enabled());
    assert!(TranslationOptions::new("model", "AI_EDITOR").enable_request_redaction);
    options.enable_request_redaction = true;
    let sent = prepare_redacted_kiro_payload(&claude_to_kiro(&claude, &options), &[]);
    assert!(!serde_json::to_string(&sent)
        .unwrap()
        .contains("opaque-toggle-fixture"));
}

#[test]
fn disabled_policy_decodes_old_checkpoint_values_without_creating_new_markers() {
    let secret = "opaque\"line\n</summary>中文";
    let enabled = RedactionSession::new();
    let mut old = payload(
        json!({"password":secret,"token":1234,"secret":true}),
        "old context",
    );
    enabled.redact_payload(&mut old, &[]);
    let string_capsule = enabled.checkpoint_text(input(&old)["password"].as_str().unwrap());
    let number_capsule = enabled.checkpoint_text(input(&old)["token"].as_str().unwrap());
    let bool_capsule = enabled.checkpoint_text(input(&old)["secret"].as_str().unwrap());
    let body = json!({
        "password":string_capsule,"token":number_capsule,"secret":bool_capsule
    })
    .to_string();
    let mut fields = serde_json::Map::new();
    fields.insert(string_capsule.clone(), json!("key-value"));
    fields.insert("body".into(), json!(body));
    fields.insert(
        "headers".into(),
        json!({"Authorization":format!("Bearer {string_capsule}")}),
    );
    fields.insert("password".into(), json!(string_capsule));
    fields.insert("token".into(), json!(number_capsule));
    fields.insert("secret".into(), json!(bool_capsule));
    fields.insert(
        "batch".into(),
        json!([string_capsule, number_capsule, bool_capsule]),
    );
    let mut source = payload(Value::Object(fields), &format!("Use {string_capsule} next"));
    let disabled = RedactionSession::with_enabled(false);
    source.redaction_session = Some(disabled.clone());
    let sent = prepare_redacted_kiro_payload(&source, &[]);
    let restored = input(&sent);
    assert_eq!(restored["password"], secret);
    assert_eq!(restored["token"], 1234);
    assert_eq!(restored["secret"], true);
    assert_eq!(restored["batch"], json!([secret, 1234, true]));
    assert_eq!(restored[secret], "key-value");
    assert_eq!(
        restored["headers"]["Authorization"],
        format!("Bearer {secret}")
    );
    let body: Value = serde_json::from_str(restored["body"].as_str().unwrap()).unwrap();
    assert_eq!(body, json!({"password":secret,"token":1234,"secret":true}));
    assert_eq!(
        sent.conversation_state
            .current_message
            .user_input_message
            .content,
        format!("Use {secret} next")
    );
    assert!(disabled.is_empty());
    assert!(!serde_json::to_string(&sent)
        .unwrap()
        .contains("__CROSSLANE_"));
    // The source retained for retries is not modified by transport preparation.
    assert!(input(&source)["password"]
        .as_str()
        .unwrap()
        .contains("__CROSSLANE_CHECKPOINT_"));
}

#[test]
fn disabled_checkpoint_compatibility_covers_tool_results_text_documents_and_schema_literals() {
    let secret = "opaque\"document\nvalue";
    let owner = RedactionSession::new();
    let masked = owner.redact_text(secret, &[secret]);
    let capsule = owner.checkpoint_text(&masked);
    let mut source = payload(json!({}), "user text");
    source.redaction_session = Some(RedactionSession::with_enabled(false));
    let user = &mut source.conversation_state.current_message.user_input_message;
    user.documents = serde_json::from_value(json!([{
        "name":"fixture", "format":"txt",
        "source":{"bytes":base64::engine::general_purpose::STANDARD.encode(format!("Token {capsule}"))}
    }])).unwrap();
    let context = user.user_input_message_context.as_mut().unwrap();
    context.tool_results = serde_json::from_value(json!([{
        "toolUseId":"old-call", "status":"success",
        "content":[{"text":json!({"token":capsule}).to_string()}]
    }]))
    .unwrap();
    let spec = context.tools[0].specification_mut().unwrap();
    spec.description = format!("Use {capsule}");
    spec.input_schema.json["properties"]["password"]["default"] = json!(capsule);
    let sent = prepare_redacted_kiro_payload(&source, &[]);
    let user = &sent.conversation_state.current_message.user_input_message;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&user.documents[0].source.bytes)
        .unwrap();
    assert_eq!(
        String::from_utf8(decoded).unwrap(),
        format!("Token {secret}")
    );
    let context = user.user_input_message_context.as_ref().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&context.tool_results[0].content[0].text).unwrap(),
        json!({"token":secret})
    );
    let spec = context.tools[0].specification().unwrap();
    assert_eq!(spec.description, format!("Use {secret}"));
    assert_eq!(
        spec.input_schema.json["properties"]["password"]["default"],
        secret
    );
    assert_eq!(
        spec.input_schema.json["properties"]["password"]["maxLength"],
        8
    );
    assert!(spec.input_schema.json["properties"]["password"]
        .get("anyOf")
        .is_none());
}

#[tokio::test]
async fn disabled_policy_survives_summary_continuation_and_budget_preparation() {
    let mut source = payload(json!({"password":"abcd"}), &"password=abcd\n".repeat(128));
    source.redaction_session = Some(RedactionSession::with_enabled(false));
    source.conversation_state.history = serde_json::from_value(json!([
        {"userInputMessage":{"content":"Old password=abcd\n".repeat(256),"modelId":"model","origin":"CLI"}},
        {"assistantResponseMessage":{"content":"Acknowledged."}}
    ])).unwrap();
    let cache = TokenCountCache::new(64).unwrap();
    let plan = cache
        .plan_kiro_compaction(&source, 512, 0)
        .await
        .unwrap()
        .expect("old history can compact");
    let summary = compaction_summary_payload(&source, &plan, "summary-model");
    let continuation = auto_continue_payload(&source, "continue", Vec::new());
    for payload in [summary, continuation] {
        assert!(!payload.redaction_session.as_ref().unwrap().is_enabled());
        let sent = prepare_redacted_kiro_payload(&payload, &[]);
        assert!(!serde_json::to_string(&sent)
            .unwrap()
            .contains("__CROSSLANE_SECRET_"));
        assert!(serde_json::to_string(&sent).unwrap().contains("abcd"));
    }
    let off = cache.estimate_kiro_payload(&source).await.unwrap();
    let unchanged = prepare_redacted_kiro_payload(&source, &[]);
    assert_eq!(off, cache.estimate_kiro_payload(&unchanged).await.unwrap());
    let mut enabled = source.clone();
    enabled.redaction_session = Some(RedactionSession::new());
    assert!(cache.estimate_kiro_payload(&enabled).await.unwrap() > off * 2);
}

#[test]
fn disabled_policy_leaves_unknown_request_markers_and_invalid_capsules_literal() {
    let literal =
        "__CROSSLANE_SECRET_00000000000000000000000000000000_0__ / __CROSSLANE_CHECKPOINT_bad__";
    let session = RedactionSession::with_enabled(false);
    assert_eq!(session.redact_text(literal, &[]), literal);
    let mut source = payload(json!({"token":literal}), literal);
    source.redaction_session = Some(session.clone());
    let sent = prepare_redacted_kiro_payload(&source, &[]);
    assert_eq!(
        serde_json::to_value(&sent).unwrap(),
        serde_json::to_value(&source).unwrap()
    );
    assert!(session.is_empty());
}

#[test]
fn a_wire_payload_cannot_forge_a_disabled_private_session() {
    let source = payload(json!({"password":"opaque-forgery-fixture"}), "user content");
    let mut wire = serde_json::to_value(&source).unwrap();
    wire["redactionSession"] = json!({"enabled":false});
    wire["redaction_session"] = json!({"enabled":false});
    wire["enable_request_redaction"] = json!(false);
    let decoded: KiroPayload = serde_json::from_value(wire).unwrap();
    assert!(decoded.redaction_session.is_none());
    let prepared = prepare_redacted_kiro_payload(&decoded, &[]);
    assert!(prepared.redaction_session.as_ref().unwrap().is_enabled());
    assert!(!serde_json::to_string(&prepared)
        .unwrap()
        .contains("opaque-forgery-fixture"));
}

#[cfg(unix)]
#[test]
fn disabled_checkpoint_shell_compatibility_preserves_literal_arguments_and_complete_commands() {
    fn run(command: &str) -> String {
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .env("CHECKPOINT_TOGGLE_TEST", "must-not-expand")
            .output()
            .unwrap();
        assert!(output.status.success(), "synthetic shell invocation failed");
        assert!(output.stderr.is_empty());
        String::from_utf8(output.stdout).unwrap()
    }

    let secret = "a'b\"c $CHECKPOINT_TOGGLE_TEST `printf injected` \\\n;literal";
    let owner = RedactionSession::new();
    let masked = owner.redact_text(secret, &[secret]);
    let capsule = owner.checkpoint_text(&masked);
    for command in [
        format!("printf '%s' {capsule}"),
        format!("printf '%s' '{capsule}'"),
        format!("printf '%s' \"{capsule}\""),
    ] {
        let mut source = payload(json!({"command":command}), "Continue");
        source.conversation_state.history[0]
            .assistant_response_message
            .as_mut()
            .unwrap()
            .tool_uses[0]
            .name = "Bash".into();
        let disabled = RedactionSession::with_enabled(false);
        source.redaction_session = Some(disabled.clone());
        let sent = prepare_redacted_kiro_payload(&source, &[]);
        assert_eq!(run(input(&sent)["command"].as_str().unwrap()), secret);
        assert!(disabled.is_empty());
        assert!(!serde_json::to_string(&sent)
            .unwrap()
            .contains("__CROSSLANE_"));
    }

    let complete_command = "printf '%s' 'already literal $CHECKPOINT_TOGGLE_TEST'";
    let masked = owner.redact_text(complete_command, &[complete_command]);
    let complete_capsule = owner.checkpoint_text(&masked);
    let mut source = payload(json!({"command":complete_capsule}), "Continue");
    source.conversation_state.history[0]
        .assistant_response_message
        .as_mut()
        .unwrap()
        .tool_uses[0]
        .name = "Bash".into();
    source.redaction_session = Some(RedactionSession::with_enabled(false));
    let sent = prepare_redacted_kiro_payload(&source, &[]);
    assert_eq!(input(&sent)["command"], complete_command);
    assert_eq!(
        run(input(&sent)["command"].as_str().unwrap()),
        "already literal $CHECKPOINT_TOGGLE_TEST"
    );

    // A structured HTTP parameter gets its value, without shell quoting.
    let mut http = payload(json!({"command":capsule,"password":capsule}), "Continue");
    http.redaction_session = Some(RedactionSession::with_enabled(false));
    let sent = prepare_redacted_kiro_payload(&http, &[]);
    assert_eq!(input(&sent)["command"], secret);
    assert_eq!(input(&sent)["password"], secret);
}
