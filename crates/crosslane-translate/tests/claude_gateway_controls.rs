use crosslane_translate::{
    auto_continue_payload, claude_to_kiro, sanitize_kiro_tool_history, validate_claude,
    ClaudeRequest, KiroToolUse, TranslationOptions,
};
use serde_json::{json, Value};

fn request() -> Value {
    json!({"model":"claude-opus-5", "max_tokens":4096,
        "messages":[{"role":"user", "content":"hello"}]})
}

fn options() -> TranslationOptions {
    let mut options = TranslationOptions::new("claude-opus-5", "AI_EDITOR");
    options.enhance_system_prompt = false;
    options.additional_model_request_fields_schema = Some(json!({"properties":{
        "output_config":{"properties":{"effort":{"enum":["low","medium","high","xhigh","max"]}}}
    }}));
    options
}

#[test]
fn per_message_effort_applies_from_next_user_turn_and_survives_model_fallback() {
    let mut value = request();
    value["output_config"] = json!({"effort":"high",
        "format":{"type":"json_schema","schema":{"type":"object","properties":{"format_sentinel":{"type":"string"}}}},
        "task_budget":{"type":"tokens","total":64000}});
    value["thinking"] = json!({"type":"enabled","budget_tokens":1024});
    value["messages"] = json!([
        {"role":"user", "content":"first"},
        {"role":"assistant", "content":"done"},
        {"role":"system", "content":[], "output_config":{"effort":"medium",
            "format":{"type":"text"}, "task_budget":{"type":"tokens","total":64000}}},
        {"role":"user", "content":"follow-up"},
        {"role":"system", "content":[], "output_config":{"effort":"max"}}
    ]);
    let request: ClaudeRequest = serde_json::from_value(value).unwrap();
    validate_claude(&request).unwrap();
    let mut payload = claude_to_kiro(&request, &options());
    assert_eq!(
        payload.additional_model_request_fields,
        Some(json!({
            "thinking":{"type":"adaptive","display":"summarized"},
            "output_config":{"effort":"medium"}
        }))
    );
    let wire = serde_json::to_string(&payload).unwrap();
    assert!(!wire.contains("format_sentinel"));
    assert!(!wire.contains("task_budget"));
    let schema = json!({"properties":{"reasoning":{"properties":{"effort":{"enum":["low","medium","high"]}}}}});
    crosslane_translate::model::apply_adaptive_thinking(&mut payload, Some(&schema), true);
    assert_eq!(
        payload.additional_model_request_fields,
        Some(json!({"reasoning":{"effort":"medium"}}))
    );
    assert!(!serde_json::to_string(&payload)
        .unwrap()
        .contains("modelRequestIntent"));
}

#[test]
fn invalid_effort_is_rejected_with_field_paths_even_with_ignored_format_hints() {
    for per_message in [false, true] {
        let mut value = request();
        let config = json!({"effort":"adaptive", "format":{"type":"text"}});
        let path = if per_message {
            value["messages"].as_array_mut().unwrap().insert(
                0,
                json!({"role":"system","content":[],"output_config":config}),
            );
            "messages.0.output_config.effort"
        } else {
            value["output_config"] = config;
            "output_config.effort"
        };
        let request: ClaudeRequest = serde_json::from_value(value).unwrap();
        assert!(validate_claude(&request)
            .unwrap_err()
            .to_string()
            .contains(path));
    }
    let mut value = request();
    value["messages"][0]["output_config"] = json!({"effort":"low"});
    let request: ClaudeRequest = serde_json::from_value(value).unwrap();
    assert!(validate_claude(&request)
        .unwrap_err()
        .to_string()
        .contains("system messages"));
}

#[test]
fn env_state_uses_only_client_environment_blocks_and_survives_continuation() {
    let mut value = request();
    value["system"] = json!("Working directory: /wrong\nPlatform: linux\n<env>\nWorking directory: /client/repo\nPlatform: darwin\n</env>");
    value["tools"] = json!([{"name":"lookup","input_schema":{"type":"object"}}]);
    let request: ClaudeRequest = serde_json::from_value(value).unwrap();
    let mut payload = claude_to_kiro(&request, &options());
    sanitize_kiro_tool_history(&mut payload);
    let expected = json!({"currentWorkingDirectory":"/client/repo","operatingSystem":"macos"});
    let wire = serde_json::to_value(&payload).unwrap();
    assert_eq!(
        wire.pointer(
            "/conversationState/currentMessage/userInputMessage/userInputMessageContext/envState"
        ),
        Some(&expected)
    );
    let mut next = auto_continue_payload(
        &payload,
        "lookup",
        vec![KiroToolUse {
            tool_use_id: "tool_1".into(),
            name: "lookup".into(),
            input: json!({}),
        }],
    );
    sanitize_kiro_tool_history(&mut next);
    let wire = serde_json::to_value(&next).unwrap();
    assert_eq!(
        wire.pointer(
            "/conversationState/currentMessage/userInputMessage/userInputMessageContext/envState"
        ),
        Some(&expected)
    );
    assert!(!wire["conversationState"]["history"]
        .to_string()
        .contains("envState"));
}

#[test]
fn env_only_context_is_preserved_without_tools_and_omitted_without_client_data() {
    for (system, expected) in [
        ("Platform: darwin", Value::Null),
        (
            "<env>\nPlatform: win32\n</env>",
            json!({"operatingSystem":"windows"}),
        ),
        (
            "<env>\nWorking directory: /remote\n</env>",
            json!({"currentWorkingDirectory":"/remote"}),
        ),
    ] {
        let mut value = request();
        value["system"] = json!(system);
        let request: ClaudeRequest = serde_json::from_value(value).unwrap();
        let mut payload = claude_to_kiro(&request, &options());
        sanitize_kiro_tool_history(&mut payload);
        let wire = serde_json::to_value(payload).unwrap();
        assert_eq!(
            wire["conversationState"]["currentMessage"]["userInputMessage"]
                ["userInputMessageContext"]["envState"],
            expected
        );
    }
}

#[test]
fn claude_discovery_aliases_round_trip_to_the_same_kiro_model() {
    use crosslane_translate::model::{claude_discovery_model_id, map_model, resolve_dynamic_model};
    for model in [
        "claude-opus-5",
        "deepseek-3.2",
        "gpt-5.6-sol",
        "auto",
        "minimax-m2.5",
    ] {
        let alias = claude_discovery_model_id(model);
        assert!(alias.contains("claude") || alias.contains("anthropic"));
        assert_eq!(
            resolve_dynamic_model(&alias, &[model.into()]),
            Some(model.into())
        );
        let route = map_model(&alias, &[], None, None, "");
        assert_eq!(route.original, alias);
        assert_eq!(route.mapped, model);
    }
}

#[test]
fn per_message_effort_survives_a_client_compaction_boundary() {
    let mut value = request();
    value["output_config"] = json!({"effort":"high"});
    value["messages"] = json!([
        {"role":"system", "content":[], "output_config":{"effort":"low"}},
        {"role":"user","content":"earlier"},
        {"role":"assistant","content":[{"type":"compaction","content":"summary"}]},
        {"role":"user","content":"continue"}
    ]);
    let mut request: ClaudeRequest = serde_json::from_value(value).unwrap();
    crosslane_translate::normalize_compaction_boundary(&mut request);
    validate_claude(&request).unwrap();
    let payload = claude_to_kiro(&request, &options());
    assert_eq!(
        payload.additional_model_request_fields.unwrap()["output_config"]["effort"],
        "low"
    );
}
