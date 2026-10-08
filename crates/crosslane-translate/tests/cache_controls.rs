use crosslane_translate::{
    claude_to_kiro, openai_to_kiro, validate_claude, validate_claude_count, validate_openai,
    ClaudeRequest, OpenAiRequest, TranslationOptions,
};
use serde_json::{json, Value};

fn claude_request(control: &Value) -> ClaudeRequest {
    serde_json::from_value(json!({
        "model":"claude-opus-5", "max_tokens":128,
        "cache_control":control,
        "system":[{"type":"text","text":"System context","cache_control":control}],
        "tools":[{"name":"lookup","input_schema":{"type":"object"},"cache_control":control}],
        "messages":[
            {"role":"user","content":"Hello","cache_control":control},
            {"role":"assistant","content":[{"type":"text","text":"Ready","cache_control":control}]},
            {"role":"user","content":[{"type":"text","text":"Continue","cache_control":control}]}
        ]
    }))
    .unwrap()
}

fn openai_request(control: &Value) -> OpenAiRequest {
    serde_json::from_value(json!({
        "model":"claude-opus-5", "max_tokens":128,
        "tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object"}},"cache_control":control}],
        "messages":[
            {"role":"system","content":[{"type":"text","text":"System context","cache_control":control}]},
            {"role":"user","content":"Hello","cache_control":control},
            {"role":"assistant","content":[{"type":"text","text":"Ready","cache_control":control}]},
            {"role":"user","content":[{"type":"text","text":"Continue","cache_control":control}],"cache_control":control}
        ]
    })).unwrap()
}

fn options() -> TranslationOptions {
    let mut options = TranslationOptions::new("claude-opus-5", "AI_EDITOR");
    options.enhance_system_prompt = false;
    options.enable_prompt_cache = true;
    options
}

#[test]
fn assistant_history_ephemeral_marker_does_not_block_generation() {
    let request: ClaudeRequest = serde_json::from_value(json!({
        "model":"claude-opus-5", "max_tokens":128,
        "messages":[
            {"role":"user","content":"Hello"},
            {"role":"assistant","content":[{"type":"text","text":"Ready",
                "cache_control":{"type":"ephemeral"}}]},
            {"role":"user","content":"Continue"}
        ]
    }))
    .unwrap();
    validate_claude(&request).unwrap();
}

#[test]
fn absent_and_null_cache_controls_do_not_create_kiro_breakpoints() {
    for absent in [false, true] {
        let control = Value::Null;
        let claude = claude_request(&control);
        let openai = openai_request(&control);
        fn remove_controls(value: &mut Value) {
            match value {
                Value::Object(object) => {
                    object.remove("cache_control");
                    object.values_mut().for_each(remove_controls);
                }
                Value::Array(array) => array.iter_mut().for_each(remove_controls),
                _ => {}
            }
        }
        let mut claude = serde_json::to_value(claude).unwrap();
        let mut openai = serde_json::to_value(openai).unwrap();
        if absent {
            remove_controls(&mut claude);
            remove_controls(&mut openai);
        }
        let claude = serde_json::from_value(claude).unwrap();
        let openai = serde_json::from_value(openai).unwrap();
        validate_claude(&claude).unwrap();
        validate_claude_count(&claude).unwrap();
        validate_openai(&openai).unwrap();
        for payload in [
            claude_to_kiro(&claude, &options()),
            openai_to_kiro(&openai, &options()),
        ] {
            let wire = serde_json::to_string(&payload).unwrap();
            assert!(wire.contains("Continue"), "{wire}");
            assert!(!wire.contains("cachePoint"), "{wire}");
            assert!(!wire.contains("cache_control"), "{wire}");
        }
    }
}

#[test]
fn ephemeral_extensions_map_only_to_native_default_markers() {
    let control = json!({"type":"ephemeral","ttl":"1h","scope":"global","evict_on_complete":true});
    let mut claude = claude_request(&Value::Null);
    claude.messages[1].content[0]["cache_control"] = control.clone();
    validate_claude(&claude).unwrap();
    let mut openai = openai_request(&Value::Null);
    openai.messages[1].cache_control = Some(control);
    validate_openai(&openai).unwrap();
    for payload in [
        claude_to_kiro(&claude, &options()),
        openai_to_kiro(&openai, &options()),
    ] {
        let wire = serde_json::to_string(&payload).unwrap();
        assert!(
            wire.contains(r#""cachePoint":{"type":"default"}"#),
            "{wire}"
        );
        for excluded in [
            "cache_control",
            "ephemeral",
            "ttl",
            "scope",
            "evict_on_complete",
        ] {
            assert!(!wire.contains(excluded), "{excluded} leaked into {wire}");
        }
    }
}

#[test]
fn explicit_markers_use_the_four_breakpoint_budget() {
    let marker = json!({"type":"ephemeral","ttl":"5m","scope":"global"});
    let mut claude = claude_request(&Value::Null);
    claude.system.as_mut().unwrap()[0]["cache_control"] = marker.clone();
    claude.tools[0].cache_control = Some(marker.clone());
    claude.messages[1].content[0]["cache_control"] = marker.clone();
    claude.messages[2].content[0]["cache_control"] = marker.clone();
    validate_claude(&claude).unwrap();
    claude.messages[0].cache_control = Some(marker.clone());
    assert!(validate_claude(&claude)
        .unwrap_err()
        .to_string()
        .contains("at most 4"));

    let mut openai = openai_request(&Value::Null);
    openai.tools[0].body["cache_control"] = marker.clone();
    openai.messages[0].content.as_mut().unwrap()[0]["cache_control"] = marker.clone();
    openai.messages[1].cache_control = Some(marker.clone());
    openai.messages[3].content.as_mut().unwrap()[0]["cache_control"] = marker.clone();
    validate_openai(&openai).unwrap();
    openai.messages[2].cache_control = Some(marker);
    assert!(validate_openai(&openai)
        .unwrap_err()
        .to_string()
        .contains("at most 4"));
}

#[test]
fn null_controls_do_not_erase_ephemeral_markers_when_merging_adjacent_messages() {
    let request: ClaudeRequest = serde_json::from_value(json!({
        "model":"claude-opus-5","max_tokens":128,
        "messages":[
            {"role":"user","content":"Cache this","cache_control":{"type":"ephemeral"}},
            {"role":"user","content":"Continue","cache_control":null}
        ]
    }))
    .unwrap();
    validate_claude(&request).unwrap();
    let payload = claude_to_kiro(&request, &options());
    let current = payload
        .conversation_state
        .current_message
        .user_input_message;
    assert_eq!(current.content, "Cache this\nContinue");
    assert!(current.cache_point.is_some());
}

#[test]
fn malformed_cache_controls_still_report_the_exact_field() {
    for (control, suffix) in [
        (json!(false), ""),
        (json!([]), ""),
        (json!("ephemeral"), ""),
        (json!({}), ".type"),
        (json!({"type":null}), ".type"),
        (json!({"type":42}), ".type"),
        (json!({"type":""}), ".type"),
        (json!({"type":"  "}), ".type"),
        (json!({"type":"default"}), ".type"),
        (json!({"type":"disabled"}), ".type"),
        (json!({"type":"Ephemeral"}), ".type"),
        (
            json!({"type":"future_cache_hint","ttl":"future_ttl"}),
            ".type",
        ),
        (json!({"type":"ephemeral","ttl":"30m"}), ".ttl"),
        (json!({"type":"ephemeral","ttl":null}), ".ttl"),
    ] {
        let mut claude = claude_request(&Value::Null);
        claude.messages[1].content[0]["cache_control"] = control.clone();
        let mut openai = openai_request(&Value::Null);
        openai.messages[1].content =
            Some(json!([{"type":"text","text":"Hello","cache_control":control}]));
        for result in [validate_claude(&claude), validate_openai(&openai)] {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains(&format!("messages.1.content.0.cache_control{suffix}:")),
                "{control}"
            );
        }
    }
}

#[test]
fn nested_null_cache_controls_preserve_tool_results_and_documents() {
    let mut request = claude_request(&Value::Null);
    request.messages[1].content =
        json!([{"type":"tool_use","id":"call_1","name":"lookup","input":{}}]);
    request.messages[2].content = json!([{
        "type":"tool_result","tool_use_id":"call_1","cache_control":null,
        "content":[
            {"type":"text","text":"Tool result","cache_control":null},
            {"type":"document","cache_control":null,"source":{"type":"content","content":[
                {"type":"text","text":"Document text","cache_control":null}
            ]}}
        ]
    }]);
    validate_claude(&request).unwrap();
    let payload = claude_to_kiro(&request, &options());
    let wire = serde_json::to_string(&payload).unwrap();
    assert!(wire.contains("Tool result"), "{wire}");
    use base64::Engine as _;
    assert_eq!(
        payload
            .conversation_state
            .current_message
            .user_input_message
            .documents[0]
            .source
            .bytes,
        base64::engine::general_purpose::STANDARD.encode("Document text")
    );
    assert!(!wire.contains("cachePoint"), "{wire}");
    request.messages[2].content[0]["content"][1]["source"]["content"][0]["cache_control"] =
        json!({"type":"ephemeral","ttl":"30m"});
    assert!(validate_claude(&request)
        .unwrap_err()
        .to_string()
        .contains("messages.2.content.0.content.1.source.content.0.cache_control.ttl"));
}

#[test]
fn cache_control_schema_is_checked_at_every_supported_claude_location() {
    let template = serde_json::to_value(claude_request(&Value::Null)).unwrap();
    for pointer in [
        "/cache_control",
        "/system/0/cache_control",
        "/tools/0/cache_control",
        "/messages/0/cache_control",
        "/messages/1/content/0/cache_control",
    ] {
        for control in [
            Value::Null,
            json!({"type":"ephemeral"}),
            json!({"type":"ephemeral","ttl":"5m"}),
            json!({"type":"ephemeral","ttl":"1h"}),
        ] {
            let mut request = template.clone();
            *request.pointer_mut(pointer).unwrap() = control;
            let request = serde_json::from_value(request).unwrap();
            validate_claude(&request).unwrap_or_else(|error| panic!("{pointer}: {error}"));
            validate_claude_count(&request).unwrap();
        }
        for (control, suffix) in [
            (json!({}), "type"),
            (json!({"type":null}), "type"),
            (json!({"type":"default"}), "type"),
            (json!({"type":"ephemeral","ttl":null}), "ttl"),
            (json!({"type":"ephemeral","ttl":"30m"}), "ttl"),
        ] {
            let mut request = template.clone();
            *request.pointer_mut(pointer).unwrap() = control;
            let request = serde_json::from_value(request).unwrap();
            let field = format!("{}.{}", pointer[1..].replace('/', "."), suffix);
            for result in [validate_claude(&request), validate_claude_count(&request)] {
                assert!(result.unwrap_err().to_string().contains(&field), "{field}");
            }
        }
    }
}

#[test]
fn longer_cache_ttls_must_precede_shorter_ttls_in_prompt_order() {
    let mut request = claude_request(&Value::Null);
    request.tools[0].cache_control = Some(json!({"type":"ephemeral","ttl":"1h"}));
    request.system.as_mut().unwrap()[0]["cache_control"] = json!({"type":"ephemeral","ttl":"5m"});
    request.messages[1].content[0]["cache_control"] = json!({"type":"ephemeral"});
    validate_claude(&request).unwrap();
    validate_claude_count(&request).unwrap();

    request.messages[1].content[0]["cache_control"]["ttl"] = json!("1h");
    for result in [validate_claude(&request), validate_claude_count(&request)] {
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("messages.1.content.0.cache_control.ttl"));
    }
    request.messages[1].content[0]["cache_control"] = Value::Null;
    request.tools[0].cache_control = Some(json!({"type":"ephemeral"}));
    request.system.as_mut().unwrap()[0]["cache_control"]["ttl"] = json!("1h");
    assert!(validate_claude(&request)
        .unwrap_err()
        .to_string()
        .contains("system.0.cache_control.ttl"));
}

#[test]
fn automatic_cache_control_checks_effective_ttl_and_available_slots() {
    let marker = json!({"type":"ephemeral"});
    let mut request = claude_request(&Value::Null);
    request.cache_control = Some(marker.clone());
    request.tools[0].cache_control = Some(marker.clone());
    request.system.as_mut().unwrap()[0]["cache_control"] = marker.clone();
    request.messages[2].content[0]["cache_control"] = json!({"type":"ephemeral","ttl":"5m"});
    validate_claude(&request).unwrap();
    validate_claude_count(&request).unwrap();
    request.cache_control = Some(json!({"type":"ephemeral","ttl":"1h"}));
    assert!(validate_claude(&request)
        .unwrap_err()
        .to_string()
        .contains("cache_control"));

    request.cache_control = Some(marker.clone());
    request.messages[2].content[0]["cache_control"] = Value::Null;
    request.messages[1].content[0]["cache_control"] = marker.clone();
    validate_claude(&request).unwrap(); // Three explicit breakpoints plus automatic caching.
    request.messages[0].cache_control = Some(marker);
    assert!(validate_claude(&request)
        .unwrap_err()
        .to_string()
        .contains("at most 4"));
}

#[test]
fn automatic_cache_control_skips_ineligible_trailing_blocks() {
    let mut request = claude_request(&Value::Null);
    request.cache_control = Some(json!({"type":"ephemeral"}));
    request.messages[2].content = json!([
        {"type":"text","text":"Cacheable","cache_control":{"type":"ephemeral","ttl":"1h"}},
        {"type":"text","text":""}
    ]);
    assert!(validate_claude(&request)
        .unwrap_err()
        .to_string()
        .contains("messages.2.content.0.cache_control.ttl"));
    request.cache_control = Some(json!({"type":"ephemeral","ttl":"1h"}));
    validate_claude(&request).unwrap();

    request.tools.clear();
    request.system = None;
    request.messages.truncate(1);
    request.messages[0].content = json!([{"type":"text","text":""}]);
    validate_claude(&request).unwrap();
    validate_claude_count(&request).unwrap();
}

#[test]
fn automatic_cache_control_can_target_a_compaction_summary() {
    let mut request = claude_request(&Value::Null);
    request.cache_control = Some(json!({"type":"ephemeral","ttl":"1h"}));
    request.messages[1].content = json!([{
        "type":"compaction","content":"Conversation summary",
        "cache_control":{"type":"ephemeral","ttl":"1h"}
    }]);
    request.messages[2].content = json!([{"type":"text","text":""}]);
    validate_claude(&request).unwrap();
    request.cache_control = Some(json!({"type":"ephemeral"}));
    assert!(validate_claude(&request)
        .unwrap_err()
        .to_string()
        .contains("messages.1.content.0.cache_control.ttl"));
    request.messages[1].content[0]["content"] = Value::Null;
    request.messages[1].content[0]["cache_control"] = Value::Null;
    validate_claude(&request).unwrap();
}

#[test]
fn nested_breakpoints_precede_their_containing_block_in_ttl_order() {
    let mut request = claude_request(&Value::Null);
    request.messages[2].content = json!([{
        "type":"search_result","source":"https://example.com","title":"Reference",
        "cache_control":{"type":"ephemeral"},
        "content":[{"type":"text","text":"Search result", "cache_control":{"type":"ephemeral","ttl":"1h"}}]
    }]);
    validate_claude(&request).unwrap();
    request.messages[2].content[0]["cache_control"]["ttl"] = json!("1h");
    request.messages[2].content[0]["content"][0]["cache_control"]["ttl"] = json!("5m");
    assert!(validate_claude(&request)
        .unwrap_err()
        .to_string()
        .contains("messages.2.content.0.cache_control.ttl"));
}

#[test]
fn nested_search_result_cache_controls_are_validated() {
    let mut request = claude_request(&Value::Null);
    request.messages[2].content = json!([{
        "type":"search_result","source":"https://example.com","title":"Reference",
        "content":[{"type":"text","text":"Search result", "cache_control":{"type":"ephemeral"}}]
    }]);
    validate_claude(&request).unwrap();
    let payload = claude_to_kiro(&request, &options());
    assert!(payload
        .conversation_state
        .current_message
        .user_input_message
        .cache_point
        .is_some());
    for (control, suffix) in [
        (json!({"type":"default"}), "type"),
        (json!({"type":"ephemeral","ttl":null}), "ttl"),
    ] {
        request.messages[2].content[0]["content"][0]["cache_control"] = control;
        for result in [validate_claude(&request), validate_claude_count(&request)] {
            assert!(result.unwrap_err().to_string().contains(&format!(
                "messages.2.content.0.content.0.cache_control.{suffix}"
            )));
        }
    }
}

#[test]
fn explicit_cache_controls_reject_uncacheable_blocks() {
    for block in [
        json!({"type":"text","text":""}),
        json!({"type":"thinking","thinking":"Reasoning","signature":"signature"}),
        json!({"type":"redacted_thinking","data":"opaque"}),
    ] {
        let mut request = claude_request(&Value::Null);
        request.messages[1].content = json!([block]);
        validate_claude(&request).unwrap();
        request.messages[1].content[0]["cache_control"] = json!({"type":"ephemeral"});
        for result in [validate_claude(&request), validate_claude_count(&request)] {
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("messages.1.content.0.cache_control"));
        }
    }
}
