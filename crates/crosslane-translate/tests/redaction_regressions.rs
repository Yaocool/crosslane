//! Privacy and tool compatibility at the final serialized Kiro boundary.

use crosslane_translate::{
    claude_to_kiro, compaction_summary_payload, prepare_redacted_kiro_payload,
    validate_kiro_tool_history, ClaudeRequest, KiroPayload, TokenCountCache, TranslationOptions,
};
use serde_json::{json, Value};

#[derive(Clone, Copy, Debug)]
enum Surface {
    Body,
    History,
    ToolInput,
    ToolResult,
}

fn source(text: &str, surface: Surface) -> KiroPayload {
    let messages = match surface {
        Surface::Body => json!([{"role":"user","content":text}]),
        Surface::History => json!([
            {"role":"user","content":text},
            {"role":"assistant","content":"Acknowledged."},
            {"role":"user","content":"Continue."}
        ]),
        Surface::ToolInput | Surface::ToolResult => json!([
            {"role":"user","content":"Inspect the synthetic fixture."},
            {"role":"assistant","content":[{"type":"tool_use","id":"fixture","name":"Bash",
                "input":{"command":if matches!(surface, Surface::ToolInput) { text } else { "printf fixture" }}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"fixture",
                "content":if matches!(surface, Surface::ToolResult) { text } else { "Fixture complete." }}]}
        ]),
    };
    let request: ClaudeRequest = serde_json::from_value(json!({
        "model":"claude-sonnet-4", "max_tokens":128,
        "tools":[{"name":"Bash","description":"Run a command","input_schema":{"type":"object","properties":{"command":{"type":"string"}}}}],
        "messages":messages
    })).unwrap();
    claude_to_kiro(
        &request,
        &TranslationOptions::new("claude-sonnet-4", "AI_EDITOR"),
    )
}

fn assert_protected(text: &str, secret: &str) {
    for surface in [
        Surface::Body,
        Surface::History,
        Surface::ToolInput,
        Surface::ToolResult,
    ] {
        let source = source(text, surface);
        let before = serde_json::to_value(&source).unwrap();
        let session = source.redaction_session.as_ref().unwrap();
        assert!(session.is_enabled());
        let sent = prepare_redacted_kiro_payload(&source, &[]);
        assert!(validate_kiro_tool_history(&sent).is_ok());
        let wire = serde_json::to_value(&sent).unwrap();
        assert!(
            !wire.to_string().contains(secret),
            "leak at {surface:?}: {text}"
        );
        assert!(!session.is_empty(), "fixture must actually trigger masking");
        #[cfg(feature = "crs")]
        {
            session.record_transmitted_payload(&wire);
            assert!(session.has_transmitted_redactions());
        }
        assert_eq!(serde_json::to_value(&source).unwrap(), before);
        for message in &sent.conversation_state.history {
            if let Some(assistant) = &message.assistant_response_message {
                for tool in &assistant.tool_uses {
                    let mut returned = tool.input.clone();
                    session.restore_tool_input_for_tool("Bash", &mut returned);
                    if matches!(surface, Surface::ToolInput) {
                        assert_eq!(returned, json!({"command":text}), "exact source replay");
                    }
                }
            }
        }
        assert_eq!(
            serde_json::to_value(prepare_redacted_kiro_payload(&sent, &[])).unwrap(),
            wire
        );
    }
}

#[test]
fn auth_literals_that_resemble_environment_references_are_private() {
    for (text, secret) in [
        ("sshpass -p '$AuditLiteralPassword2026' ssh example.invalid", "$AuditLiteralPassword2026"),
        ("mysql --password='$AuditLiteralPassword2026' -h example.invalid", "$AuditLiteralPassword2026"),
        ("curl -H 'X-Api-Key: $AuditLiteralPassword2026' https://example.invalid/", "$AuditLiteralPassword2026"),
        ("curl -u 'user:$AuditLiteralPassword2026' https://example.invalid/", "$AuditLiteralPassword2026"),
        ("curl -u user:%AuditPercentPassword2026% https://example.invalid/", "%AuditPercentPassword2026%"),
        ("python -c 'from requests.auth import HTTPBasicAuth; HTTPBasicAuth(\"user\", \"$AuditLiteralPassword2026\")'", "$AuditLiteralPassword2026"),
        ("const config = {password: '$AuditLiteralPassword2026'}", "$AuditLiteralPassword2026"),
        ("password=$'$AuditLiteralPassword2026'", "$AuditLiteralPassword2026"),
        ("custom-client --api-key '$AuditLiteralFlag2026' --verbose", "$AuditLiteralFlag2026"),
    ] { assert_protected(text, secret); }
}

#[test]
fn runtime_defaults_are_private_including_inside_quoted_shell_programs() {
    for expression in [
        "Deno.env.get(\"API_TOKEN\") ?? \"AuditRuntimeDefault2026\"",
        "Bun.env.API_TOKEN ?? \"AuditRuntimeDefault2026\"",
        "Bun.env['API_TOKEN'] || 'AuditRuntimeDefault2026'",
        "std::env::var(\"API_TOKEN\").unwrap_or(\"AuditRuntimeDefault2026\".to_owned())",
        "std::env::var(\"API_TOKEN\").unwrap_or_else(|_| \"AuditRuntimeDefault2026\".to_string())",
        "process.env.API_TOKEN ?? \"AuditRuntimeDefault2026\"",
        "os.getenv(\"API_TOKEN\", \"AuditRuntimeDefault2026\")",
        "Environment.GetEnvironmentVariable(\"API_TOKEN\") ?? \"AuditRuntimeDefault2026\"",
    ] {
        assert_protected(
            &format!("let value = {expression};"),
            "AuditRuntimeDefault2026",
        );
    }
    assert_protected(
        "deno eval 'const token = Deno.env.get(\"API_TOKEN\") ?? \"AuditDenoDefault2026\";'",
        "AuditDenoDefault2026",
    );
    assert_protected(
        "bun -e 'const value = Bun.env.API_TOKEN ?? \"$AuditLiteralDefault2026\";'",
        "$AuditLiteralDefault2026",
    );
}

#[test]
fn escaped_json_field_names_are_decoded_inside_prose_fences_and_logs() {
    for document in [
        r#"{"\u5bc6\u7801":"AuditEscapedPassword2026"}"#,
        r#"{"api\u005fkey":"AuditEscapedPassword2026","credentials":"include","method":"GET"}"#,
        r#"[{"\u5bc6\u7801":"AuditEscapedPassword2026"}]"#,
    ] {
        for text in [
            document.to_owned(),
            format!("```json\n{document}\n```"),
            format!("configuration output:\n{document}"),
            format!("{document}\nend of output"),
            format!("[INFO] {document}\n[INFO] {document}"),
        ] {
            assert_protected(&text, "AuditEscapedPassword2026");
        }
    }
    for text in [
        r#"log: {"\u5bc6\u7801":"AuditBrokenJson2026""#,
        r#"log: {"\u5bc6\u7801":"AuditBrokenJson2026",}"#,
        r#"log: {"\u5bc6\u7801":"AuditBrokenJson2026","token":"other-incomplete-value"#,
    ] {
        assert_protected(text, "AuditBrokenJson2026");
    }
    assert_protected(
        "password=prefix{\"data\":1}AuditEnclosingSuffix2026",
        "AuditEnclosingSuffix2026",
    );
}

#[test]
fn embedded_json_restores_decoded_values_and_original_source_bytes() {
    let original = r#"configuration output:
 { "\u5bc6\u7801" : "quote\"slash\\line\n中文", "n" : 1e3 }
end"#;
    let payload = source(original, Surface::Body);
    let session = payload.redaction_session.as_ref().unwrap();
    let sent = prepare_redacted_kiro_payload(&payload, &[]);
    let text = &sent
        .conversation_state
        .current_message
        .user_input_message
        .content;
    let start = text.find("configuration output:\n").unwrap() + "configuration output:\n".len();
    let end = text[start..].find("\nend").unwrap() + start;
    let document: Value = serde_json::from_str(&text[start..end]).unwrap();
    assert_eq!(document["n"], 1000.0);
    let mut returned = json!({"password":document["密码"],"body":&text[text.find("configuration output:").unwrap()..]});
    session.restore_tool_input(&mut returned);
    assert_eq!(returned["password"], "quote\"slash\\line\n中文");
    assert_eq!(returned["body"], original);
}

#[test]
fn real_references_and_public_controls_remain_unchanged() {
    for text in [
        "curl -H \"Authorization: Bearer $API_TOKEN\" https://example.invalid/",
        "curl -u \"${BASIC_USER}:${BASIC_PASSWORD}\" https://example.invalid/",
        "curl.exe -H \"Authorization: Bearer %API_TOKEN%\" https://example.invalid/",
        "curl.exe -H \"Authorization: Bearer $Env:API_TOKEN\" https://example.invalid/",
        "const value = Deno.env.get(\"API_TOKEN\");",
        "const value = Bun.env.API_TOKEN;",
        "let value = std::env::var(\"API_TOKEN\");",
        "log: {\"credentials\":\"include\", \"method\":\"GET\", \"max_tokens\":1024}\nAccess-Control-Allow-Credentials: true",
        "const value = Bun.env.REGION ?? \"us-east-1\";",
    ] {
        for surface in [Surface::Body, Surface::ToolInput] {
            let original = source(text, surface);
            let before = serde_json::to_value(&original).unwrap();
            let sent = prepare_redacted_kiro_payload(&original, &[]);
            assert_eq!(serde_json::to_value(&sent).unwrap(), before, "public/reference input: {text}");
            assert!(original.redaction_session.as_ref().unwrap().is_empty());
        }
    }
}

#[test]
fn explicit_windows_shell_metadata_preserves_its_variable_references() {
    for (shell, command) in [
        (
            "cmd",
            "curl -H \"X-Api-Key: %API_TOKEN%\" https://example.invalid/",
        ),
        (
            "pwsh",
            "curl -H \"X-Api-Key: $Env:API_TOKEN\" https://example.invalid/",
        ),
        (
            "powershell",
            "curl -u \"${Env:USER}:${Env:PASSWORD}\" https://example.invalid/",
        ),
    ] {
        let mut original = source(command, Surface::ToolInput);
        let tool = original
            .conversation_state
            .history
            .iter_mut()
            .filter_map(|entry| entry.assistant_response_message.as_mut())
            .flat_map(|assistant| &mut assistant.tool_uses)
            .next()
            .unwrap();
        tool.input["shell"] = json!(shell);
        let before = serde_json::to_value(&original).unwrap();
        let sent = prepare_redacted_kiro_payload(&original, &[]);
        assert_eq!(serde_json::to_value(&sent).unwrap(), before, "{shell}");
        assert!(original.redaction_session.as_ref().unwrap().is_empty());
    }
}

#[tokio::test]
async fn compaction_summary_uses_the_same_filter_for_embedded_credentials() {
    let text = "log: {\"\\u5bc6\\u7801\":\"AuditSummarySecret2026\"}\n".repeat(128);
    let original = source(&text, Surface::History);
    let cache = TokenCountCache::new(64).unwrap();
    let plan = cache
        .plan_kiro_compaction(&original, 512, 0)
        .await
        .unwrap()
        .unwrap();
    let summary = compaction_summary_payload(&original, &plan, "summary-model");
    let sent = prepare_redacted_kiro_payload(&summary, &[]);
    assert!(!serde_json::to_string(&sent)
        .unwrap()
        .contains("AuditSummarySecret2026"));
    assert!(!sent.redaction_session.as_ref().unwrap().is_empty());
}

#[test]
fn embedded_json_does_not_change_surrounding_shell_assignment_boundaries() {
    for command in [
        "API_TOKEN=AuditShellPrefix2026 curl --data '{ \"credentials\": \"include\" }' https://example.invalid/",
        "echo '{}' ; API_TOKEN=AuditShellPrefix2026 curl https://example.invalid/",
    ] {
        let original = source(command, Surface::ToolInput);
        let sent = prepare_redacted_kiro_payload(&original, &[]);
        let tool = sent.conversation_state.history.iter()
            .filter_map(|entry| entry.assistant_response_message.as_ref())
            .flat_map(|assistant| &assistant.tool_uses).next().unwrap();
        let safe = tool.input["command"].as_str().unwrap();
        assert!(!safe.contains("AuditShellPrefix2026"));
        assert!(safe.contains(" curl "), "following command must survive: {safe}");
        assert!(safe.ends_with("https://example.invalid/"));
        let mut returned = tool.input.clone();
        original.redaction_session.as_ref().unwrap().restore_tool_input_for_tool("Bash", &mut returned);
        assert_eq!(returned, json!({"command":command}));
    }
}

#[test]
fn embedded_json_preserves_language_context_for_following_environment_references() {
    let text = "```ruby\nputs({\"\\u5bc6\\u7801\":\"AuditFenceSecret2026\"})\napi_token=ENV.fetch('API_TOKEN')\n```";
    assert_protected(text, "AuditFenceSecret2026");
    let original = source(text, Surface::Body);
    let sent = prepare_redacted_kiro_payload(&original, &[]);
    let safe = &sent
        .conversation_state
        .current_message
        .user_input_message
        .content;
    assert!(safe.contains("api_token=ENV.fetch('API_TOKEN')"));
}

#[test]
fn json_lines_keep_public_controls_and_filter_credentials_at_the_end() {
    let log = "[INFO] {\"credentials\":\"include\",\"method\":\"GET\"}\n".repeat(4200);
    let request = source(&log, Surface::ToolInput);
    assert_eq!(
        serde_json::to_value(prepare_redacted_kiro_payload(&request, &[])).unwrap(),
        serde_json::to_value(&request).unwrap()
    );
    let text = format!("{log}{{\"\\u5bc6\\u7801\":\"AuditJsonLinesSecret2026\"}}");
    let original = source(&text, Surface::Body);
    let sent = prepare_redacted_kiro_payload(&original, &[]);
    let safe = &sent
        .conversation_state
        .current_message
        .user_input_message
        .content;
    assert!(!safe.contains("AuditJsonLinesSecret2026"));
    assert!(safe.contains(&log));
}
