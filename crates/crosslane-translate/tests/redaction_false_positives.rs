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
fn business_resource_keys_stay_public_across_text_data_and_schemas() {
    for name in [
        "pdf_key",
        "pdfKey",
        "file_key",
        "document_key",
        "imageKey",
        "attachment_key",
        "blob_key",
        "resource_key",
        "artifact_key",
        "record_key",
        "column_key",
        "output_pdf_key",
        "source_document_key",
        "renderedPdfKey",
        "prompt_cache_key",
        "toml_key",
        "json_key",
        "yaml_key",
        "xml_key",
        "config_key",
        "setting_key",
        "property_key",
        "schema_key",
        "template_key",
        "translation_key",
        "i18n_key",
        "locale_key",
        "message_key",
        "label_key",
        "component_key",
        "element_key",
        "widget_key",
        "feature_flag_key",
        "routing_key",
        "route_key",
        "operation_key",
        "command_key",
        "action_key",
        "method_key",
        "task_key",
        "workflow_key",
        "job_key",
        "queue_key",
        "topic_key",
        "event_key",
        "pipeline_key",
        "stage_key",
        "step_key",
        "business_key",
        "reference_key",
        "ref_key",
        "relation_key",
        "relationship_key",
        "clustering_key",
        "shard_key",
        "composite_key",
        "dedup_key",
        "dedupe_key",
        "idempotency_key",
        "correlation_key",
        "trace_key",
        "cursor_key",
        "pagination_key",
        "redis_key",
        "source_key",
        "target_key",
        "child_key",
        "root_key",
        "pdf_storage_key",
        "file_storage_key",
        "s3_object_key",
        "cos_object_key",
        "source_pdf_file_key",
        "source_output_pdf_key",
        "theme_key",
        "style_key",
        "icon_key",
        "layout_key",
        "panel_key",
        "section_key",
        "view_key",
        "form_key",
        "table_key",
        "cell_key",
        "filter_key",
        "user_reference_key",
        "project_record_key",
        "next_cursor_token",
        "cursor_token",
        "pagination_token",
    ] {
        for value in [
            "PDF存储Key",
            "documents/report.pdf",
            "object-ref-2026",
            "perf",
        ] {
            let text = json!({name:value}).to_string();
            let mut source = documentation(&text);
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
            schema["properties"][name] =
                json!({"type":"string","default":value,"examples":[value]});
            let assistant = source
                .conversation_state
                .history
                .iter_mut()
                .find_map(|h| {
                    h.assistant_response_message
                        .as_mut()
                        .filter(|a| !a.tool_uses.is_empty())
                })
                .unwrap();
            assistant.tool_uses[0].input = json!({name:value});
            assert_unmodified(&source);
        }
        for text in [
            format!("{name}=object-ref-2026"),
            format!("client --{name} object-ref-2026"),
            format!("https://example.invalid/?{name}=object-ref-2026"),
        ] {
            assert_unmodified(&documentation(&text));
        }
    }
}

#[test]
fn business_key_exemptions_never_override_real_credentials_or_private_contexts() {
    const PRIVATE: &str = "opaque-private-credential-2026";
    for name in [
        "pdf_key",
        "file_key",
        "documentKey",
        "output_pdf_key",
        "config_key",
        "idempotency_key",
        "templateKey",
        "s3_object_key",
        "cursor_token",
        "next_cursor_token",
    ] {
        for container in [
            "auth",
            "authentication",
            "credentials",
            "encryption",
            "decryption",
            "signing",
            "crypto",
        ] {
            let text = json!({container:{name:PRIVATE}}).to_string();
            let session = crosslane_translate::RedactionSession::new();
            let safe = session.redact_text(&text, &[]);
            assert!(!safe.contains(PRIVATE), "credential leaked: {text}");
            assert_eq!(session.restore_text(&safe), text);
            let text = format!("const options = {{{container}: {{ {name}: '{PRIVATE}' }} }};");
            let session = crosslane_translate::RedactionSession::new();
            assert!(
                !session.redact_text(&text, &[]).contains(PRIVATE),
                "credential leaked: {text}"
            );
            let mut source = documentation("A normal business response.");
            source
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
                .json["properties"][container] =
                json!({"type":"object","properties":{name:{"type":"string","default":PRIVATE}}});
            let sent = prepare_redacted_kiro_payload(&source, &[]);
            assert!(
                !serde_json::to_string(&sent).unwrap().contains(PRIVATE),
                "schema credential leaked: {container}.{name}"
            );
        }
        for secret in [
            "ksk_private_credential_2026",
            "sk-proj-Ab12Cd34Ef56Gh78Ij90",
            "AKIA1234567890ABCDEF",
        ] {
            let text = json!({name:secret}).to_string();
            let session = crosslane_translate::RedactionSession::new();
            assert!(!session.redact_text(&text, &[]).contains(secret));
        }
        let text = json!({name:PRIVATE}).to_string();
        let session = crosslane_translate::RedactionSession::new();
        assert!(!session.redact_text(&text, &[PRIVATE]).contains(PRIVATE));
        let text = json!({name:"PDF存储Key"}).to_string();
        assert!(!crosslane_translate::RedactionSession::new()
            .redact_text(&text, &["PDF存储Key"])
            .contains("PDF存储Key"));
    }
    for text in [
        json!({"auth":{"providers":[{"pdf_key":PRIVATE}]},"pdf_key":"public-object-ref"}).to_string(),
        json!({"encryption":[{"settings":{"document_key":PRIVATE}}],"file_key":"public-object-ref"}).to_string(),
        format!("const options = {{auth: {{ providers: [{{ pdf_key: '{PRIVATE}' }}] }}, pdf_key: 'public-object-ref' }};"),
        format!("const options = {{encryption: [{{ file_key: '{PRIVATE}' }}, {{pdf_key: '{PRIVATE}'}}], file_key: 'public-object-ref' }};"),
    ] {
        let session = crosslane_translate::RedactionSession::new();
        let safe = session.redact_text(&text, &[]);
        assert!(!safe.contains(PRIVATE), "nested credential leaked: {text}");
        assert!(safe.contains("public-object-ref"));
        assert_eq!(session.restore_text(&safe), text);
    }
    for text in [
        format!("PDF_KEY={PRIVATE}"),
        format!("${{PDF_KEY:-{PRIVATE}}}"),
        format!("${{FILE_KEY:-{PRIVATE}}}"),
        format!("pdf_encryption_key={PRIVATE}"),
        format!("file_access_key={PRIVATE}"),
        format!("document_secret_key={PRIVATE}"),
        format!("app_key={PRIVATE}"),
        format!("application_key={PRIVATE}"),
        format!("client_key={PRIVATE}"),
        format!("account_key={PRIVATE}"),
        format!("user_key={PRIVATE}"),
        format!("project_key={PRIVATE}"),
        format!("service_key={PRIVATE}"),
        format!("tenant_key={PRIVATE}"),
        format!("storage_key={PRIVATE}"),
        format!("external_key={PRIVATE}"),
        format!("api_config_key={PRIVATE}"),
        format!("config_api_key={PRIVATE}"),
        format!("source_pdf_encryption_key={PRIVATE}"),
        format!("source_client_key={PRIVATE}"),
        format!("source_project_key={PRIVATE}"),
        format!("config_storage_key={PRIVATE}"),
        format!("file_aws_key={PRIVATE}"),
        format!("config_secret_key={PRIVATE}"),
        format!("download_token={PRIVATE}"),
        format!("share_token={PRIVATE}"),
        format!("resume_token={PRIVATE}"),
        format!("csrf_token={PRIVATE}"),
        format!("CONFIG_KEY={PRIVATE}"),
        format!("CURSOR_TOKEN={PRIVATE}"),
        format!("${{CURSOR_TOKEN:-{PRIVATE}}}"),
    ] {
        let session = crosslane_translate::RedactionSession::new();
        let safe = session.redact_text(&text, &[]);
        assert!(!safe.contains(PRIVATE), "credential leaked: {text}");
        assert_eq!(session.restore_text(&safe), text);
    }
    // A descriptive-looking value is still private in an actual password field.
    let text = r#"{"password":"PDF存储Key"}"#;
    assert!(!crosslane_translate::RedactionSession::new()
        .redact_text(text, &[])
        .contains("PDF存储Key"));
}

#[test]
fn explicit_cloud_and_design_references_stay_public_on_every_payload_surface() {
    for (name, reference) in [
        (
            "secret_id",
            "arn:aws:secretsmanager:us-east-1:123456789012:secret:example-AbCdEf",
        ),
        (
            "kms_key",
            "arn:aws:kms:us-east-1:123456789012:key/12345678-1234-1234-1234-123456789012",
        ),
        (
            "secret_id",
            "projects/example/secrets/database/versions/latest",
        ),
        ("color_token", "var(--color-primary)"),
        ("design_token", "{color.brand.primary}"),
        ("font_token", "var(--font-body)"),
    ] {
        let text = json!({name:reference}).to_string();
        let mut source = documentation(&text);
        source
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
            .json["properties"][name] = json!({"type":"string","default":reference});
        assert_unmodified(&source);
        for text in [
            reference.to_owned(),
            format!("{name}={reference}"),
            format!("client --{name} '{reference}'"),
        ] {
            assert_unmodified(&documentation(&text));
        }
        let session = crosslane_translate::RedactionSession::new();
        assert!(
            !session.redact_text(&text, &[reference]).contains(reference),
            "known reference-shaped credential leaked: {name}"
        );
        for container in [
            "auth",
            "encryption",
            "signing",
            "secret",
            "credentials",
            "password",
        ] {
            for text in [
                json!({container:{name:reference}}).to_string(),
                format!("const options = {{{container}: {{{name}: '{reference}'}}}};"),
            ] {
                let session = crosslane_translate::RedactionSession::new();
                let safe = session.redact_text(&text, &[]);
                assert!(
                    !safe.contains(reference),
                    "credential context bypass: {text}"
                );
                assert_eq!(session.restore_text(&safe), text);
            }
            let mut source = documentation("Referenced configuration metadata.");
            source
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
                .json["properties"][container] =
                json!({"type":"object","properties":{name:{"type":"string","default":reference}}});
            let sent = prepare_redacted_kiro_payload(&source, &[]);
            assert!(
                !serde_json::to_string(&sent).unwrap().contains(reference),
                "credential schema bypass: {container}.{name}"
            );
        }
    }
    for name in ["secret_id", "kms_key", "color_token", "design_token"] {
        let text = json!({name:"opaque-private-reference-2026"}).to_string();
        assert!(!crosslane_translate::RedactionSession::new()
            .redact_text(&text, &[])
            .contains("opaque-private-reference-2026"));
    }
    let private = "ksk_private_reference_material_2026";
    let reference = format!("arn:aws:secretsmanager:us-east-1:123456789012:secret:{private}");
    assert!(!crosslane_translate::RedactionSession::new()
        .redact_text(&json!({"secret_id":reference}).to_string(), &[])
        .contains(private));
    for reference in [
        "var(--color-primary)",
        "arn:aws:secretsmanager:us-east-1:123456789012:secret:example-AbCdEf",
    ] {
        let text = json!({"password":reference}).to_string();
        assert!(!crosslane_translate::RedactionSession::new()
            .redact_text(&text, &[])
            .contains(reference));
    }
}

#[test]
fn known_numeric_credentials_override_business_ids_without_corrupting_protocol_budgets() {
    let mut source = documentation("Business metadata.");
    source.inference_config = Some(serde_json::from_value(json!({"maxTokens":4096})).unwrap());
    let assistant = source
        .conversation_state
        .history
        .iter_mut()
        .find_map(|history| {
            history
                .assistant_response_message
                .as_mut()
                .filter(|assistant| !assistant.tool_uses.is_empty())
        })
        .unwrap();
    assistant.tool_uses[0].input = json!({"config_key":4096,"idempotency_key":4096});
    let sent = prepare_redacted_kiro_payload(&source, &["4096"]);
    let mut input = sent
        .conversation_state
        .history
        .iter()
        .find_map(|history| {
            history
                .assistant_response_message
                .as_ref()
                .filter(|assistant| !assistant.tool_uses.is_empty())
        })
        .unwrap()
        .tool_uses[0]
        .input
        .clone();
    assert!(!input.to_string().contains("4096"));
    let wire = serde_json::to_value(&sent).unwrap();
    assert_eq!(wire["inferenceConfig"]["maxTokens"], 4096);
    let session = source.redaction_session.as_ref().unwrap();
    session.restore_tool_input(&mut input);
    assert_eq!(input, json!({"config_key":4096,"idempotency_key":4096}));
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
