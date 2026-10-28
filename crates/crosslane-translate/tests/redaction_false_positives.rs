//! Public examples must not seed request-wide credential replacement.

use crosslane_translate::{
    claude_to_kiro, prepare_redacted_kiro_payload, ClaudeRequest, KiroPayload, TranslationOptions,
};
use serde_json::json;

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a04cAAAAASUVORK5CYII=";
const DIMENSIONS: &str =
    "const DIMENSIONS = [{key: 'bugs'}, {key: 'perf'}]; perform a performance review";

fn payload(text: &str) -> KiroPayload {
    let request: ClaudeRequest = serde_json::from_value(json!({
        "model":"fixture", "max_tokens":256,
        "system":format!("{DIMENSIONS}\n{text}"),
        "tools":[{
            "name":"inspect", "description":format!("{DIMENSIONS}\n{text}"),
            "input_schema":{
                "type":"object", "properties":{
                    "key":{"type":"string","enum":["bugs","perf","Enter"]},
                    "api_key":{"type":"string","default":text}
                }
            }
        }],
        "messages":[
            {"role":"user","content":text},
            {"role":"assistant","content":[{"type":"tool_use","id":"fixture","name":"inspect",
                "input":{"key":"perf","text":text}}]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"fixture","content":text},
                {"type":"text","text":format!("performance.md: {text}")},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG}}
            ]}
        ]
    }))
    .unwrap();
    claude_to_kiro(&request, &TranslationOptions::new("fixture", "AI_EDITOR"))
}

fn assert_unmodified(source: &KiroPayload) {
    let before = serde_json::to_value(source).unwrap();
    let sent = prepare_redacted_kiro_payload(source, &[]);
    let wire = serde_json::to_value(&sent).unwrap();
    assert!(
        wire == before,
        "public examples must not change the wire contract: {:?}",
        source.redaction_session.as_ref().unwrap()
    );
    let session = source.redaction_session.as_ref().unwrap();
    assert!(session.is_empty());
    #[cfg(feature = "crs")]
    {
        session.record_transmitted_payload(&wire);
        assert!(!session.has_transmitted_redactions());
    }
}

fn documentation(text: &str) -> KiroPayload {
    let mut source = payload(text);
    let schema = &mut source
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context
        .as_mut()
        .unwrap()
        .tools[0]
        .specification_mut()
        .unwrap()
        .input_schema
        .json;
    schema["properties"]["api_key"]["default"] = json!("sk-proj-xxxxx");
    schema["properties"]["key"]["description"] = json!(text);
    source
}

#[test]
fn documentation_placeholder_variants_do_not_seed_request_wide_replacement() {
    for text in [
        "api_key=YOUR_OPENAI_API_KEY",
        "password=<YOUR_PASSWORD>",
        "secret={{API_SECRET}}",
        "Authorization: Bearer [YOUR_ACCESS_TOKEN]",
        "token=YOUR_ACCESS_TOKEN_HERE",
        "password=<REDACTED>",
        "gho_xxxxxxxxxxxx glpat-xxxxxxxxxxxx xoxb-xxxxxxxxxxxx AIzaXXXXXXXXXXXXXXXXXXXX",
        "api_key=<YOUR-API-KEY>",
        "curl -u '<USERNAME>:<PASSWORD>' https://example.invalid/",
    ] {
        assert_unmodified(&documentation(text));
    }
}

#[test]
fn authentication_prose_with_punctuation_is_not_a_bearer_credential() {
    for text in [
        "Use Bearer authentication.",
        "Bearer authentication, optional; Bearer credentials are supplied by the client.",
        "Bearer token-based authentication is supported.",
        "Bearer authorization: see the authentication guide.",
    ] {
        assert_unmodified(&documentation(text));
    }
}

#[test]
fn public_identifiers_pagination_and_function_signatures_are_not_credentials() {
    for text in [
        r#"{"hotKey":"Enter","shortcut_key":"Ctrl+K","lookupKey":"name"}"#,
        r#"{"nextPageToken":"cursor-page-2","page_token":"cursor-page-3"}"#,
        "next_page_token=cursor-page-2\nshortcut_key=Ctrl+K",
        r#"{"public_key":"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPublic public@example.invalid"}"#,
        r#"{"signature":"fn handler(request: Request) -> Response"}"#,
        "signature: (name: string) => string",
        "AsianDevelopmentBank and AsiaPacificDevelopmentCenter are names.",
    ] {
        assert_unmodified(&documentation(text));
    }
}

#[test]
fn programming_type_declarations_do_not_become_passwords() {
    for text in [
        "```typescript\ninterface Options {\n apiKey: string;\n password: string;\n token: string;\n}\n```",
        "type Options = { apiKey: string; password: string | null; }",
        "```rust\nstruct Options {\n api_key: String,\n password: String,\n}\n```",
        "```python\ndef connect(password: str, api_key: str):\n    pass\n```",
        "fn connect(password: &str, api_key: &str) {}",
    ] {
        assert_unmodified(&documentation(text));
    }
}

#[test]
fn private_key_file_references_are_not_inline_credentials() {
    for text in [
        "client --private-key /home/user/.ssh/id_ed25519 --token-file /run/secrets/token",
        "curl --key ./certs/client.key https://example.invalid/",
        r#"{"private_key":"/home/user/.ssh/id_ed25519","tls_key":"./certs/client.pem"}"#,
    ] {
        assert_unmodified(&documentation(text));
    }
}

#[test]
fn documentation_public_syntax_never_exempts_actual_credentials() {
    for (text, secret) in [
        (
            "interface Options { apiKey: string; }\nconst apiKey = 'actual-private-value-2026';",
            "actual-private-value-2026",
        ),
        (
            "class Options { password: string = 'actual-private-value-2026'; }",
            "actual-private-value-2026",
        ),
        (
            "def connect(password: str = 'actual-private-value-2026'): pass",
            "actual-private-value-2026",
        ),
        (
            r#"{"nextPageToken":"ksk_actual_private_value_2026"}"#,
            "ksk_actual_private_value_2026",
        ),
        (
            r#"{"hotKey":"ksk_actual_private_value_2026"}"#,
            "ksk_actual_private_value_2026",
        ),
        (
            r#"{"public_key":"-----BEGIN PRIVATE KEY-----actual-private-value-2026-----END PRIVATE KEY-----"}"#,
            "actual-private-value-2026",
        ),
        (
            r#"{"api_key":"/path-looking-real-secret-2026"}"#,
            "/path-looking-real-secret-2026",
        ),
        (
            r#"{"auth":{"key":"/opaque-private-value-2026"}}"#,
            "/opaque-private-value-2026",
        ),
        (
            r#"{"private_key":"/opaque/private/value/2026"}"#,
            "/opaque/private/value/2026",
        ),
        (
            r#"{"signature":"opaque-private-value-2026"}"#,
            "opaque-private-value-2026",
        ),
        ("Authorization: Bearer authentication.", "authentication."),
        ("Authorization: Bearer credentials", "credentials"),
        ("password=string", "string"),
        ("password=xxxxx", "xxxxx"),
        ("password=***", "***"),
        (
            "token=YOUR_ACCESS_TOKEN_HERE-real-suffix",
            "YOUR_ACCESS_TOKEN_HERE-real-suffix",
        ),
    ] {
        let source = documentation(text);
        let session = source.redaction_session.as_ref().unwrap();
        let sent = prepare_redacted_kiro_payload(&source, &[]);
        let wire = serde_json::to_value(&sent).unwrap();
        // Protocol schema types such as `string` are public even when an actual
        // password happens to have that value. Check model text and tool data.
        let private = |value: &str| assert!(!value.contains(secret), "leak: {text}");
        let current = &sent.conversation_state.current_message.user_input_message;
        private(&current.content);
        for tool in &current.user_input_message_context.as_ref().unwrap().tools {
            if let Some(spec) = tool.specification() {
                private(&spec.description);
            }
        }
        for history in &sent.conversation_state.history {
            if let Some(user) = &history.user_input_message {
                private(&user.content);
            }
            if let Some(assistant) = &history.assistant_response_message {
                private(&assistant.content);
                for tool in &assistant.tool_uses {
                    private(&tool.input.to_string());
                }
            }
        }
        assert!(wire.to_string().contains("performance"));
        assert_eq!(session.restore_text(&session.redact_text(text, &[])), text);
        #[cfg(feature = "crs")]
        {
            session.record_transmitted_payload(&wire);
            assert!(session.has_transmitted_redactions());
        }
    }
}

#[test]
fn known_credentials_override_public_metadata_and_documentation_placeholders() {
    for (text, secret) in [
        ("api_key=YOUR_OPENAI_API_KEY", "YOUR_OPENAI_API_KEY"),
        (
            r#"{"page_token":"private-cursor-2026"}"#,
            "private-cursor-2026",
        ),
        (
            r#"{"public_key":"private-value-2026"}"#,
            "private-value-2026",
        ),
        (
            "client --private-key ./certs/private.key",
            "./certs/private.key",
        ),
        ("interface Options { password: string; }", "string"),
        ("Use Bearer authentication.", "authentication."),
    ] {
        let source = documentation(text);
        let sent = prepare_redacted_kiro_payload(&source, &[secret]);
        let session = source.redaction_session.as_ref().unwrap();
        let safe = session.redact_text(text, &[secret]);
        assert!(!safe.contains(secret), "known credential leaked: {text}");
        assert_eq!(session.restore_text(&safe), text);
        let wire = serde_json::to_value(&sent).unwrap();
        assert!(wire.to_string().contains("__CROSSLANE_SECRET_"));
        #[cfg(feature = "crs")]
        {
            session.record_transmitted_payload(&wire);
            assert!(session.has_transmitted_redactions());
        }
    }
}

#[test]
fn ordinary_keys_preserve_words_and_schema_enums() {
    // Remove the deliberately credential-labelled schema default for this case.
    let mut source = payload("perform a performance review");
    let context = source
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context
        .as_mut()
        .unwrap();
    context.tools[0]
        .specification_mut()
        .unwrap()
        .input_schema
        .json["properties"]
        .as_object_mut()
        .unwrap()
        .remove("api_key");
    assert_unmodified(&source);
}

#[test]
fn ordinary_dictionary_identifiers_do_not_become_credentials() {
    for name in [
        "key",
        "Key",
        "cache_key",
        "sortKey",
        "primary_key",
        "object_key",
    ] {
        let original = json!({name:"perf", "text":"perform a performance review"}).to_string();
        let session = crosslane_translate::RedactionSession::new();
        assert_eq!(session.redact_text(&original, &[]), original);
        assert!(session.is_empty());
    }
    let original = json!({"hasKey":true,"enableKey":false,"hasSortKey":"true"}).to_string();
    let session = crosslane_translate::RedactionSession::new();
    assert_eq!(session.redact_text(&original, &[]), original);
    assert!(session.is_empty());
}

#[test]
fn explicit_key_placeholders_do_not_seed_redaction_on_any_surface() {
    for placeholder in [
        "sk-proj-xxxxx",
        "sk-ant-xxxxx",
        "sk-xxxxxxxxxxxxxxxx",
        "sk-proj-...",
        "YOUR_API_KEY",
        "<API_KEY>",
    ] {
        assert_unmodified(&payload(placeholder));
    }
}

#[test]
fn explicit_placeholder_assignments_and_authentication_examples_stay_literal() {
    for text in [
        "const apiKey = \"sk-proj-xxxxx\";",
        "Authorization: Bearer sk-proj-xxxxx",
        "curl -H 'Authorization: Bearer sk-proj-xxxxx' https://example.invalid/",
        "API_KEY=YOUR_API_KEY",
        "${API_KEY:-sk-proj-xxxxx}",
        r#"{"api_key":"${API_KEY:-sk-proj-xxxxx}"}"#,
        r#"{"api_key":"os.getenv(\"API_KEY\", \"sk-proj-xxxxx\")"}"#,
        r#"{"auth":{"key":"YOUR_API_KEY"}}"#,
        r#"{"password":"<PASSWORD>"}"#,
    ] {
        let session = crosslane_translate::RedactionSession::new();
        assert_eq!(session.redact_text(text, &[]), text);
        assert!(session.is_empty());
    }
}

#[test]
fn credential_examples_with_real_values_are_still_protected() {
    for secret in [
        "sk-proj-Ab12Cd34Ef56Gh78Ij90",
        "ksk_fixture_real_key_2026",
        "AKIA1234567890ABCDEF",
        "example-but-actually-private-2026",
        "sk-proj-xxxxx-real-suffix",
    ] {
        let source = payload(secret);
        let sent = prepare_redacted_kiro_payload(&source, &[]);
        let wire = serde_json::to_value(&sent).unwrap();
        assert!(
            !wire.to_string().contains(secret),
            "credential leaked: {secret}"
        );
        assert!(wire.to_string().contains("performance"));
        assert!(wire.to_string().contains("perform a performance review"));
        assert_eq!(
            wire["conversationState"]["currentMessage"]["userInputMessage"]["images"][0]["source"]
                ["bytes"],
            PNG
        );
        let session = source.redaction_session.as_ref().unwrap();
        let input = &sent
            .conversation_state
            .history
            .iter()
            .find_map(|h| {
                h.assistant_response_message
                    .as_ref()
                    .filter(|a| !a.tool_uses.is_empty())
            })
            .unwrap()
            .tool_uses[0]
            .input;
        let mut restored = input.clone();
        session.restore_tool_input(&mut restored);
        assert_eq!(restored, json!({"key":"perf","text":secret}));
        #[cfg(feature = "crs")]
        {
            session.record_transmitted_payload(&wire);
            assert!(session.has_transmitted_redactions());
        }
    }
}

#[test]
fn known_credentials_override_placeholder_appearance() {
    for secret in ["sk-proj-xxxxx", "YOUR_API_KEY", "<API_KEY>"] {
        let source = payload(secret);
        let sent = prepare_redacted_kiro_payload(&source, &[secret]);
        let wire = serde_json::to_value(&sent).unwrap();
        assert!(!wire.to_string().contains(secret));
        assert!(wire.to_string().contains("performance"));
        #[cfg(feature = "crs")]
        {
            let session = source.redaction_session.as_ref().unwrap();
            session.record_transmitted_payload(&wire);
            assert!(session.has_transmitted_redactions());
        }
    }
    // A real short credential embedded in an otherwise public placeholder
    // must still be removed; heuristic exemptions cannot override known values.
    let source = payload("sk-proj-xxxxx");
    let sent = prepare_redacted_kiro_payload(&source, &["xxxxx"]);
    assert!(!serde_json::to_string(&sent).unwrap().contains("xxxxx"));
    for text in [
        "${API_KEY:-sk-proj-xxxxx}",
        r#"{"api_key":"${API_KEY:-sk-proj-xxxxx}"}"#,
        r#"{"api_key":"os.getenv(\"API_KEY\", \"sk-proj-xxxxx\")"}"#,
    ] {
        let session = crosslane_translate::RedactionSession::new();
        let safe = session.redact_text(text, &["xxxxx"]);
        assert!(!safe.contains("xxxxx"));
        assert_eq!(session.restore_text(&safe), text);
    }
}

#[test]
fn generic_keys_in_authentication_contexts_remain_private() {
    for text in [
        r#"{"auth":{"key":"opaque-real-key"}}"#,
        r#"{"authentication":{"key":"opaque-real-key"}}"#,
        r#"{"credentials":{"key":"opaque-real-key"}}"#,
        "const options = {auth: {key: 'opaque-real-key'}};",
        "const options = {auth: {\n  type: 'api_key',\n  key: 'opaque-real-key'\n}};",
        "https://example.invalid/?key=opaque-real-key&limit=10",
        "client --key opaque-real-key --verbose",
        "KEY=opaque-real-key",
        "${KEY:-opaque-real-key}",
        "curl -H 'Key: opaque-real-key' https://example.invalid/",
        r#"{"key":"ksk_opaque-real-key"}"#,
    ] {
        let session = crosslane_translate::RedactionSession::new();
        let safe = session.redact_text(text, &[]);
        assert!(!safe.contains("opaque-real-key"), "leak: {safe}");
        assert_eq!(session.restore_text(&safe), text);
    }
}

#[test]
fn labels_examples_and_short_passwords_do_not_disable_actual_protection() {
    for (text, secret) in [
        (
            "// Example only\npassword=real-password-2026",
            "real-password-2026",
        ),
        ("password=xxxxx", "xxxxx"),
        ("token=test", "test"),
        (
            "api_key=sk-proj-xxxxx-real-suffix",
            "sk-proj-xxxxx-real-suffix",
        ),
        (
            r#"{"api_key":"sk-proj-xxxxx","password":"real-password-2026"}"#,
            "real-password-2026",
        ),
    ] {
        let session = crosslane_translate::RedactionSession::new();
        let safe = session.redact_text(text, &[]);
        assert!(!safe.contains(secret), "leak: {safe}");
        assert_eq!(session.restore_text(&safe), text);
    }
    let session = crosslane_translate::RedactionSession::new();
    session.redact_text("password=test", &[]);
    let safe = session.redact_text("test and prefix-test-suffix", &[]);
    assert!(!safe.contains("test"));
}
