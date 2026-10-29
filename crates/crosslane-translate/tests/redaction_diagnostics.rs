use base64::Engine;
use crosslane_translate::{KiroPayload, RedactionSession};
use serde_json::{json, Value};

#[test]
fn reports_only_owned_references_on_the_outgoing_wire() {
    let session = RedactionSession::new();
    let masked = session.redact_text("token=synthetic-budget-only-credential", &[]);
    assert!(
        !session
            .outgoing_redaction_report(&json!({"content":"clean"}), 0)
            .triggered
    );
    let other = RedactionSession::new().redact_text("token=synthetic-unrelated-credential", &[]);
    assert!(
        !session
            .outgoing_redaction_report(&json!({"content":other}), 0)
            .triggered
    );
    let wire = json!({"content":masked});
    let report = session.outgoing_redaction_report(&wire, 0);
    assert!(report.enabled && report.triggered);
    assert_eq!(report.redacted_values, 1);
    assert_eq!(
        report.sources[0].rules.iter().copied().collect::<Vec<_>>(),
        ["text_match"]
    );
    #[cfg(feature = "crs")]
    assert!(
        !session.has_transmitted_redactions(),
        "inspection must not change notice state"
    );
    let disabled = RedactionSession::with_enabled(false).outgoing_redaction_report(&wire, 0);
    assert!(!disabled.enabled && !disabled.triggered && disabled.sources.is_empty());
}

#[test]
fn outgoing_sources_cover_system_history_tools_schemas_and_text_documents() {
    const SECRET: &str = "ksk_synthetic_diagnostics_credential_2026";
    let doc = base64::engine::general_purpose::STANDARD.encode(format!("api_key={SECRET}"));
    let mut payload: KiroPayload = serde_json::from_value(json!({
        "conversationState": {
            "conversationId":"fixture", "chatTriggerType":"MANUAL",
            "currentMessage":{"userInputMessage":{
                "content":format!("api_key={SECRET}"), "modelId":"fixture", "origin":"AI_EDITOR",
                "documents":[{"format":"txt","name":"fixture","source":{"bytes":doc}}],
                "userInputMessageContext":{
                    "toolResults":[{"toolUseId":"fixture","status":"success","content":[{"text":format!("api_key={SECRET}")}]}],
                    "tools":[{"toolSpecification":{
                        "name":"private-tool-name-do-not-log", "description":format!("api_key={SECRET}"),
                        "inputSchema":{"json":{"type":"object","properties":{"private-property-name-do-not-log":{"type":"string","description":format!("api_key={SECRET}")},"api_key":{"type":"string","default":SECRET}}}}
                    }}]
                }
            }},
            "history":[
                {"userInputMessage":{"content":format!("api_key={SECRET}"),"modelId":"fixture","origin":"AI_EDITOR"}},
                {"assistantResponseMessage":{"content":"Acknowledged"}},
                {"userInputMessage":{"content":format!("api_key={SECRET}"),"modelId":"fixture","origin":"AI_EDITOR"}},
                {"assistantResponseMessage":{"content":format!("api_key={SECRET}"),"toolUses":[{"toolUseId":"fixture","name":"private-tool-name-do-not-log","input":{"password":SECRET,"private-property-name-do-not-log":SECRET,"documents":{"nested":SECRET}}}]}}
            ]
        }
    })).unwrap();
    payload.protected_history_messages = 2;
    let session = RedactionSession::new();
    session.redact_payload(&mut payload, &[]);
    let wire = serde_json::to_value(&payload).unwrap();
    let report = session.outgoing_redaction_report(&wire, payload.protected_history_len());
    assert!(report.triggered && !report.sources_truncated);
    for category in [
        "current_user_text",
        "history_user_text",
        "history_assistant_text",
        "system_prompt",
        "tool_input",
        "tool_result",
        "tool_description",
        "tool_schema",
        "document_text",
    ] {
        assert!(
            report.sources.iter().any(|entry| entry.source == category),
            "missing {category}: {report:?}"
        );
    }
    assert!(report
        .sources
        .iter()
        .any(|entry| entry.rules.contains("credential_field")));
    let serialized = serde_json::to_string(&report).unwrap();
    for private in [
        SECRET,
        "__CROSSLANE_SECRET_",
        "private-tool-name-do-not-log",
        "private-property-name-do-not-log",
        "fixture",
    ] {
        assert!(!serialized.contains(private), "diagnostic leaked {private}");
    }
    assert!(serialized.contains("/history/3/assistantResponseMessage/toolUses/0/input/"));
    assert!(
        report
            .sources
            .iter()
            .any(|entry| entry.source == "tool_input" && entry.path.contains("documents")),
        "data named documents must not be mistaken for a native attachment"
    );
}

#[test]
fn report_size_is_bounded_without_losing_trigger_counts() {
    let session = RedactionSession::new();
    let masked = session.redact_text("token=synthetic-report-size-credential", &[]);
    let mut data = json!({});
    for index in 0..100 {
        data[format!("private-field-{index}")] = json!(masked);
    }
    let report = session.outgoing_redaction_report(&data, 0);
    assert!(report.triggered && report.sources_truncated);
    assert_eq!(report.redacted_values, 100);
    assert_eq!(report.sources.len(), 32);
    let mut deep = json!(masked);
    for _ in 0..100 {
        deep = json!({"private-field":deep});
    }
    let report = session.outgoing_redaction_report(&deep, 0);
    assert!(report.triggered && report.sources_truncated);
    assert!(report.sources.iter().all(|source| source.path.len() <= 512));
    assert!(!serde_json::to_string(&report)
        .unwrap()
        .contains("private-field"));
}

#[test]
fn search_reports_are_content_free_and_do_not_count_encoded_images() {
    let session = RedactionSession::new();
    let masked = session.redact_text("api_key=synthetic-web-search-credential", &[]);
    let wire = json!({"params":{"arguments":{"query":masked}}});
    let report = session.outgoing_redaction_report(&wire, 0);
    assert_eq!(report.sources[0].source, "web_search");
    let image = base64::engine::general_purpose::STANDARD.encode(masked.as_bytes());
    let wire: Value = json!({"conversationState":{"currentMessage":{"userInputMessage":{"images":[{"format":"png","source":{"bytes":image}}]}}}});
    assert!(!session.outgoing_redaction_report(&wire, 0).triggered);
}
