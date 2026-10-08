use crosslane_translate::{
    claude_to_kiro, openai_to_kiro, parse_openai_request, responses_to_openai, validate_claude,
    validate_claude_count, validate_openai, ClaudeRequest, OpenAiRequest, ResponsesRequest,
    TranslationOptions,
};
use serde_json::{json, Value};

#[test]
fn token_counting_does_not_apply_generation_limits_to_thinking() {
    let request: ClaudeRequest = serde_json::from_value(json!({
        "model":"claude-haiku-4.5", "max_tokens":1,
        "messages":[{"role":"user","content":"Hello"}],
        "thinking":{"type":"enabled","budget_tokens":1024}
    }))
    .unwrap();
    validate_claude_count(&request).unwrap();
    assert!(
        validate_claude(&request).is_err(),
        "generation must still validate its budget"
    );
}

#[test]
fn omitted_tool_result_and_nullable_document_fields_preserve_kiro_content() {
    let request: ClaudeRequest = serde_json::from_value(json!({
        "model":"claude-haiku-4.5", "max_tokens":128,
        "tools":[{"name":"lookup","input_schema":{"type":"object"}}],
        "messages":[
            {"role":"user","content":"Look up the value"},
            {"role":"assistant","content":[{"type":"tool_use","id":"tooluse_abc","name":"lookup","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"tooluse_abc"},
                {"type":"document","title":"","citations":null,"source":{"type":"text","media_type":"text/plain","data":"Keep this document."}}]}
        ]
    })).unwrap();
    validate_claude(&request).unwrap();
    let payload = claude_to_kiro(
        &request,
        &TranslationOptions::new("claude-haiku-4.5", "AI_EDITOR"),
    );
    let current = &payload
        .conversation_state
        .current_message
        .user_input_message;
    assert_eq!(current.documents.len(), 1);
    let results = &current
        .user_input_message_context
        .as_ref()
        .unwrap()
        .tool_results;
    assert_eq!(results[0].tool_use_id, "tooluse_abc");
    assert_eq!(results[0].content[0].text, "(empty result)");
    let wire = serde_json::to_value(payload).unwrap();
    assert!(!wire.to_string().contains("citations"));
}

#[test]
fn openai_nullable_fields_and_refusal_replay_keep_native_intent() {
    let request: OpenAiRequest = serde_json::from_value(json!({
        "model":"claude-haiku-4.5","stream":null,"max_completion_tokens":17,
        "reasoning_effort":"none","stop":[" CUT"],
        "tools":[{"type":"function","function":{"name":"lookup","strict":null}}],
        "messages":[{"role":"user","content":"Hello"},
            {"role":"assistant","content":[{"type":"refusal","refusal":"Cannot perform this action."}]},
            {"role":"user","content":"Reply OK"}]
    })).unwrap();
    validate_openai(&request).unwrap();
    assert!(!request.stream);
    assert_eq!(request.stop_sequences(), [" CUT"]);
    let payload = openai_to_kiro(
        &request,
        &TranslationOptions::new("claude-haiku-4.5", "AI_EDITOR"),
    );
    assert_eq!(
        payload.inference_config.as_ref().unwrap().max_tokens,
        Some(17)
    );
    let intent = payload.model_request_intent.as_ref().unwrap();
    assert_eq!(intent.thinking.as_ref().unwrap().r#type, "disabled");
    assert!(intent.effort.is_none());
    let wire = serde_json::to_string(&payload).unwrap();
    assert!(wire.contains("Cannot perform this action."));
    assert!(!wire.contains("strict"));
    assert!(!wire.contains("reasoning_effort"));
    assert!(!wire.contains("stop"));
}

#[test]
fn responses_refusal_is_preserved_and_unknown_parts_remain_invalid() {
    let mut body = json!({"model":"claude-haiku-4.5","input":[
        {"role":"user","content":"Hello"},
        {"type":"message","role":"assistant","id":"msg_refusal","status":"completed", "content":[{"type":"refusal","refusal":"Cannot perform this action."}]},
        {"role":"user","content":"Reply OK"}
    ]});
    let request: ResponsesRequest = serde_json::from_value(body.clone()).unwrap();
    let normalized = responses_to_openai(&request).unwrap();
    let wire = serde_json::to_string(&openai_to_kiro(
        &normalized.request,
        &TranslationOptions::new("claude-haiku-4.5", "AI_EDITOR"),
    ))
    .unwrap();
    assert!(wire.contains("Cannot perform this action."));
    body["input"][1]["content"][0]["type"] = json!("invalid_part");
    assert!(responses_to_openai(&serde_json::from_value(body).unwrap()).is_err());
}

#[test]
fn legacy_functions_normalize_stably_without_losing_pairing() {
    let body = json!({"model":"claude-haiku-4.5", "functions":[{"name":"lookup","parameters":{"type":"object"}}],
        "function_call":"auto", "messages":[
        {"role":"user","content":"Look up the value"},
        {"role":"assistant","content":null,"function_call":{"name":"lookup","arguments":"{}"}},
        {"role":"function","name":"lookup","content":"OK"},
        {"role":"user","content":"Reply OK"}
    ]});
    let request = parse_openai_request(body.clone()).unwrap();
    let again = parse_openai_request(body).unwrap();
    validate_openai(&request).unwrap();
    assert!(request.legacy_functions);
    assert_eq!(request.messages[1].tool_calls, again.messages[1].tool_calls);
    assert_eq!(request.messages[2].role, "tool");
    assert_eq!(
        request.messages[1].tool_calls[0]["id"].as_str(),
        request.messages[2].tool_call_id.as_deref()
    );
    let wire: Value = serde_json::to_value(openai_to_kiro(
        &request,
        &TranslationOptions::new("claude-haiku-4.5", "AI_EDITOR"),
    ))
    .unwrap();
    assert!(wire.to_string().contains("toolResults"));
    assert!(!wire.to_string().contains("function_call"));
    assert_eq!(
        wire["conversationState"]["currentMessage"]["userInputMessage"]["userInputMessageContext"]
            ["tools"][0]["toolSpecification"]["description"],
        "Tool lookup."
    );
}

#[test]
fn legacy_function_controls_treat_null_modern_fields_as_omitted() {
    let request = parse_openai_request(json!({"model":"test", "tools":null, "tool_choice":null,
        "functions":[{"name":"lookup"}], "function_call":"none", "messages":[
        {"role":"assistant", "content":null, "tool_calls":null, "function_call":{"name":"lookup","arguments":"{}"}},
        {"role":"function", "name":"lookup", "content":"OK"}
    ]})).unwrap();
    validate_openai(&request).unwrap();
    assert!(request.legacy_functions);
    assert_eq!(request.tool_choice, Some(json!("none")));
    assert_eq!(
        request.messages[1].tool_call_id.as_deref(),
        request.messages[0].tool_calls[0]["id"].as_str()
    );
    let payload = openai_to_kiro(&request, &TranslationOptions::new("test", "AI_EDITOR"));
    assert!(payload
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context
        .as_ref()
        .is_none_or(|context| context.tools.is_empty()));
}

#[test]
fn inline_file_data_can_infer_its_format_with_an_extensionless_name() {
    use base64::Engine as _;
    for (name, bytes, format) in [
        ("evidence", "Plain text evidence", "txt"),
        ("evidence", "%PDF-1.7\nexample", "pdf"),
    ] {
        let request = parse_openai_request(json!({"model":"test","messages":[{"role":"user","content":[{
            "type":"file","file":{"filename":name,"file_data":base64::engine::general_purpose::STANDARD.encode(bytes)}
        }]}]})).unwrap();
        validate_openai(&request).unwrap();
        let payload = openai_to_kiro(&request, &TranslationOptions::new("test", "AI_EDITOR"));
        assert_eq!(
            payload
                .conversation_state
                .current_message
                .user_input_message
                .documents[0]
                .format,
            format
        );
    }
}

#[tokio::test]
async fn automatic_truncation_preserves_instructions_current_input_and_tool_chain() {
    let request = parse_openai_request(json!({"model":"test","tools":[{"type":"function","function":{"name":"lookup"}}],"messages":[
        {"role":"system","content":"Keep these instructions."},
        {"role":"user","content":"Old discarded context. ".repeat(1000)},
        {"role":"assistant","content":"Acknowledged."},
        {"role":"user","content":"Look up the value."},
        {"role":"assistant","content":null,"tool_calls":[{"id":"call_lookup","type":"function","function":{"name":"lookup","arguments":"{}"}}]},
        {"role":"tool","tool_call_id":"call_lookup","content":"The current evidence."}
    ]})).unwrap();
    let mut payload = openai_to_kiro(&request, &TranslationOptions::new("test", "AI_EDITOR"));
    crosslane_translate::sanitize_kiro_tool_history(&mut payload);
    let current = serde_json::to_value(&payload.conversation_state.current_message).unwrap();
    let cache = crosslane_translate::TokenCountCache::new(16).unwrap();
    let original = cache.estimate_kiro_payload(&payload).await.unwrap();
    let tokens = cache
        .truncate_kiro_history(&mut payload, original / 2)
        .await
        .unwrap();
    assert!(tokens < original / 2);
    assert_eq!(tokens, cache.estimate_kiro_payload(&payload).await.unwrap());
    let wire = serde_json::to_string(&payload).unwrap();
    assert!(!wire.contains("Old discarded context."));
    assert!(wire.contains("Keep these instructions."));
    assert!(wire.contains("call_lookup"));
    assert_eq!(
        serde_json::to_value(&payload.conversation_state.current_message).unwrap(),
        current
    );
    crosslane_translate::validate_kiro_tool_history(&payload).unwrap();
    let before = serde_json::to_value(&payload).unwrap();
    assert!(cache.truncate_kiro_history(&mut payload, 1).await.unwrap() > 1);
    assert_eq!(
        serde_json::to_value(&payload).unwrap(),
        before,
        "cannot discard the active tool chain"
    );
}

#[test]
fn openai_files_preserve_document_bytes_in_current_and_historical_turns() {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD.encode("Document evidence: cobalt");
    let file = json!({"type":"file","file":{"filename":"evidence.txt","file_data":bytes}});
    let request = parse_openai_request(json!({"model":"claude-haiku-4.5","messages":[
        {"role":"user","content":[file.clone()]},
        {"role":"assistant","content":"Read it."},
        {"role":"user","content":[file]}
    ]}))
    .unwrap();
    validate_openai(&request).unwrap();
    let wire = serde_json::to_value(openai_to_kiro(
        &request,
        &TranslationOptions::new("claude-haiku-4.5", "AI_EDITOR"),
    ))
    .unwrap();
    assert_eq!(
        wire["conversationState"]["currentMessage"]["userInputMessage"]["documents"][0]["source"]
            ["bytes"],
        bytes
    );
    assert!(wire["conversationState"]["history"]
        .as_array()
        .unwrap()
        .iter()
        .any(|turn| turn["userInputMessage"]["documents"][0]["source"]["bytes"] == bytes));
    let responses: ResponsesRequest = serde_json::from_value(json!({"model":"claude-haiku-4.5","input":[
        {"type":"function_call","call_id":"tooluse_example","name":"lookup","arguments":"{}"},
        {"type":"function_call_output","call_id":"tooluse_example","output":[{"type":"input_file","filename":"evidence.txt","file_data":bytes}]}
    ]})).unwrap();
    let translated = responses_to_openai(&responses).unwrap();
    let payload = openai_to_kiro(
        &translated.request,
        &TranslationOptions::new("claude-haiku-4.5", "AI_EDITOR"),
    );
    assert_eq!(
        payload
            .conversation_state
            .current_message
            .user_input_message
            .documents
            .len(),
        1
    );
    assert!(parse_openai_request(json!({"model":"x","messages":[{"role":"user","content":[{"type":"file","file":{"file_id":"file_foreign"}}]}]})).is_err());
}

#[test]
fn claude_search_results_preserve_source_title_and_text_in_tool_results() {
    let search = json!({"type":"search_result","source":"https://example.com/evidence","title":"Evidence title",
        "content":[{"type":"text","text":"Cobalt source evidence."}],"citations":{"enabled":true}});
    let request: ClaudeRequest = serde_json::from_value(json!({"model":"claude-haiku-4.5","max_tokens":128,
        "tools":[{"name":"lookup","input_schema":{"type":"object"}}],"messages":[
        {"role":"user","content":[search.clone()]},
        {"role":"assistant","content":[{"type":"tool_use","id":"tooluse_example","name":"lookup","input":{}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"tooluse_example","content":[search]}]}
    ]})).unwrap();
    validate_claude(&request).unwrap();
    let payload = claude_to_kiro(
        &request,
        &TranslationOptions::new("claude-haiku-4.5", "AI_EDITOR"),
    );
    let result = &payload
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context
        .as_ref()
        .unwrap()
        .tool_results[0];
    for text in [
        "https://example.com/evidence",
        "Evidence title",
        "Cobalt source evidence.",
    ] {
        assert!(result.content[0].text.contains(text), "{result:?}");
        assert!(payload
            .conversation_state
            .history
            .iter()
            .filter_map(|turn| turn.user_input_message.as_ref())
            .any(|user| user.content.contains(text)));
    }
}
