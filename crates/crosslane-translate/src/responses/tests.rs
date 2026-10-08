use super::*;
use crate::{
    openai_to_kiro, sanitize_kiro_tool_history, validate_kiro_tool_history, TranslationOptions,
};

fn normalize(value: Value) -> ResponsesTranslation {
    responses_to_openai(&serde_json::from_value(value).expect("Responses request"))
        .expect("supported request")
}

#[test]
fn codex_request_preserves_instructions_controls_and_namespace_tools() {
    let translated = normalize(json!({
        "model":"claude-sonnet-4.5", "instructions":"Follow repository instructions.",
        "input":[{"role":"developer","content":"Use safe commands."},{"role":"user","content":[{"type":"input_text","text":"Inspect the project."}]}],
        "store":false,"stream":true,"parallel_tool_calls":false,
        "stream_options":{"reasoning_summary_delivery":"sequential_cutoff"},
        "include":["reasoning.encrypted_content"],"prompt_cache_key":"thread-1",
        "client_metadata":{"turn_id":"turn-1"},
        "max_output_tokens":2048,"temperature":0.4,"top_p":0.9,
        "reasoning":{"effort":"high","summary":"auto"},"text":{"verbosity":"low"},
        "tools":[{"type":"namespace","name":"functions","description":"Repository tools.","tools":[
            {"type":"function","name":"read_file","parameters":{"type":"object","properties":{"path":{"type":"string"}}},"strict":false},
            {"type":"custom","name":"apply_patch","format":{"type":"grammar","syntax":"lark","definition":"start: /.+/"}}
        ]}],
        "tool_choice":{"type":"function","namespace":"functions","name":"read_file"}
    }));
    let request = &translated.request;
    assert_eq!(
        request.messages[0].content,
        Some(json!("Follow repository instructions."))
    );
    assert_eq!(request.messages[2].role, "developer");
    assert_eq!(request.max_tokens, Some(2048));
    assert_eq!(request.reasoning_effort.as_deref(), Some("high"));
    assert!(!request.parallel_tool_calls);
    assert_eq!(request.stream_options, Some(json!({"include_usage":true})));
    assert_eq!(
        request.tools[0].body["function"]["name"],
        "functions.read_file"
    );
    assert!(request.tools[0].body["function"]["description"]
        .as_str()
        .unwrap()
        .contains("Repository tools."));
    assert_eq!(
        request.tools[1].body["custom"]["format"]["grammar"]["syntax"],
        "lark"
    );
    assert_eq!(
        request.tool_choice.as_ref().unwrap()["function"]["name"],
        "functions.read_file"
    );
    assert_eq!(
        translated.tool_names["functions.apply_patch"].name,
        "apply_patch"
    );
    assert_eq!(
        translated.tool_names["functions.apply_patch"]
            .namespace
            .as_deref(),
        Some("functions")
    );
}

#[test]
fn responses_lite_additional_tools_are_callable_and_not_messages() {
    let translated = normalize(json!({
        "model":"gpt-5.6-sol",
        "input":[
            {"type":"additional_tools","role":"developer","tools":[
                {"type":"custom","name":"exec","description":"Run JavaScript",
                 "format":{"type":"grammar","syntax":"lark","definition":"start: /.+/"}},
                {"type":"namespace","name":"collaboration","tools":[
                    {"type":"function","name":"spawn_agent","parameters":{"type":"object"}}
                ]}
            ]},
            {"type":"message","role":"developer","content":"Follow repository rules."},
            {"type":"message","role":"user","content":"Inspect the project."}
        ],
        "prompt_cache_key":"thread-123"
    }));

    assert_eq!(translated.tools.len(), 2);
    assert_eq!(translated.request.tools.len(), 2);
    assert_eq!(translated.request.messages.len(), 2);
    assert_eq!(translated.request.messages[0].role, "developer");
    assert_eq!(translated.request.tools[0].body["custom"]["name"], "exec");
    assert_eq!(
        translated.request.tools[1].body["function"]["name"],
        "collaboration.spawn_agent"
    );
    assert_eq!(
        translated.request.conversation_id.as_deref(),
        Some("thread-123")
    );
    assert_eq!(translated.tool_names["exec"].name, "exec");
    assert_eq!(
        translated.tool_names["collaboration.spawn_agent"]
            .namespace
            .as_deref(),
        Some("collaboration")
    );
    let payload = openai_to_kiro(
        &translated.request,
        &TranslationOptions::new("gpt-5.6-sol", "AI_EDITOR"),
    );
    let custom = payload
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context
        .as_ref()
        .unwrap()
        .tools
        .iter()
        .filter_map(|tool| tool.specification())
        .find(|tool| tool.name == "exec")
        .unwrap();
    assert!(custom.description.contains("Input format (lark grammar)"));
    assert!(custom.description.contains("start: /.+/"));
    assert_eq!(custom.input_schema.json["additionalProperties"], false);
}

#[test]
fn duplicate_tool_channels_are_coalesced_but_conflicts_are_rejected() {
    let tool = json!({
        "type":"function","name":"read_file","parameters":{"type":"object"}
    });
    let translated = normalize(json!({
        "model":"test",
        "input":[
            {"type":"additional_tools","tools":[tool.clone()]},
            {"role":"user","content":"Read it."}
        ],
        "tools":[tool]
    }));
    assert_eq!(translated.tools.len(), 1);
    assert_eq!(translated.request.tools.len(), 1);

    let request: ResponsesRequest = serde_json::from_value(json!({
        "model":"test",
        "input":[
            {"type":"additional_tools","tools":[
                {"type":"function","name":"read_file","description":"different"}
            ]},
            {"role":"user","content":"Read it."}
        ],
        "tools":[{"type":"function","name":"read_file"}]
    }))
    .unwrap();
    assert!(responses_to_openai(&request).is_err());

    let request: ResponsesRequest = serde_json::from_value(json!({
        "model":"test","input":"Read it.","tools":[tool.clone(),tool]
    }))
    .unwrap();
    assert!(responses_to_openai(&request).is_err());
}

#[test]
fn malformed_additional_tools_are_rejected_as_known_controls() {
    for item in [
        json!({"type":"additional_tools"}),
        json!({"type":"additional_tools","tools":{}}),
    ] {
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"test","input":[item,{"role":"user","content":"hello"}]
        }))
        .unwrap();
        assert!(responses_to_openai(&request).is_err());
    }
}

#[test]
fn stateless_tool_roundtrip_keeps_call_ids_inputs_reasoning_and_results() {
    let translated = normalize(json!({
        "model":"claude-sonnet-4.5", "input":[
            {"type":"message","role":"user","content":"Update the file."},
            {"type":"reasoning","id":"rs_previous","summary":[{"type":"summary_text","text":"Read the file before editing."}]},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"Checking the file."}],"status":"completed","phase":"commentary"},
            {"type":"function_call","id":"fc_item","call_id":"call_read","namespace":"functions","name":"read_file","arguments":"{\"path\":\"README.md\"}"},
            {"type":"custom_tool_call","id":"ctc_item","call_id":"call_patch","name":"apply_patch","input":"*** Begin Patch\n*** End Patch"},
            {"type":"function_call_output","call_id":"call_read","output":[{"type":"input_text","text":"read result"}]},
            {"type":"custom_tool_call_output","call_id":"call_patch","output":"patched"}
        ], "tools":[
            {"type":"namespace","name":"functions","tools":[{"type":"function","name":"read_file","parameters":{"type":"object"}}]},
            {"type":"custom","name":"apply_patch"}
        ]
    }));
    let mut payload = openai_to_kiro(
        &translated.request,
        &TranslationOptions::new("claude-sonnet-4.5", "AI_EDITOR"),
    );
    sanitize_kiro_tool_history(&mut payload);
    validate_kiro_tool_history(&payload).expect("paired tool history");
    let assistant = payload
        .conversation_state
        .history
        .iter()
        .filter_map(|item| item.assistant_response_message.as_ref())
        .next_back()
        .unwrap();
    assert!(assistant.content.contains("Read the file before editing."));
    assert!(assistant.content.contains("Checking the file."));
    assert_eq!(assistant.tool_uses.len(), 2);
    assert_eq!(assistant.tool_uses[0].tool_use_id, "call_read");
    assert_eq!(assistant.tool_uses[0].input["path"], "README.md");
    assert_eq!(
        assistant.tool_uses[1].input["input"],
        "*** Begin Patch\n*** End Patch"
    );
    let results = &payload
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context
        .as_ref()
        .unwrap()
        .tool_results;
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].tool_use_id, "call_read");
    assert_eq!(results[0].content[0].text, "read result");
    assert_eq!(results[1].content[0].text, "patched");
}

#[test]
fn codex_automation_bootstrap_preserves_context_without_a_tool_call() {
    let text =
        "Automation: Daily check\nAutomation ID: daily-check\nRead the saved task instructions.";
    for call_id in [None, Some(Value::Null)] {
        for output in [json!(text), json!([{"type":"input_text","text":text}])] {
            let mut bootstrap = json!({
                "type":"function_call_output", "id":"fco_bootstrap",
                "name":"automation_update", "namespace":"codex_app", "output":output
            });
            if let Some(call_id) = &call_id {
                bootstrap["call_id"] = call_id.clone();
            }
            // A standalone bootstrap is enough to start a scheduled turn.
            let request: ResponsesRequest = serde_json::from_value(json!({
                "model":"gpt-5.6-sol", "input":[bootstrap]
            }))
            .unwrap();
            let original = request.input.clone();
            let translated = responses_to_openai(&request).expect("Codex automation bootstrap");
            assert_eq!(request.input, original, "keep replayable Responses history");
            assert_eq!(translated.request.messages.len(), 1);
            let message = &translated.request.messages[0];
            assert_eq!(message.role, "developer");
            assert_eq!(
                message.content,
                Some(if output.is_array() {
                    json!([{"type":"text","text":text}])
                } else {
                    json!(text)
                })
            );
            assert!(message.tool_calls.is_empty());
            assert!(message.tool_call_id.is_none());

            let mut payload = openai_to_kiro(
                &translated.request,
                &TranslationOptions::new("gpt-5.6-sol", "AI_EDITOR"),
            );
            sanitize_kiro_tool_history(&mut payload);
            validate_kiro_tool_history(&payload).expect("no orphan Kiro tool result");
            assert!(payload.conversation_state.history[0]
                .user_input_message
                .as_ref()
                .unwrap()
                .content
                .contains(text));
            assert_eq!(payload.protected_history_messages, 2);
            assert!(!serde_json::to_string(&payload)
                .unwrap()
                .contains("toolUseId"));
        }
    }
}

#[test]
fn codex_automation_bootstrap_keeps_regular_tool_calls_paired() {
    let translated = normalize(json!({
        "model":"gpt-5.6-sol", "input":[
            {"role":"user","content":"Run the scheduled check."},
            {"type":"function_call_output","name":"automation_update",
                "namespace":"codex_app","output":"Automation ID: daily-check"},
            {"type":"function_call","call_id":"call_update","namespace":"codex_app",
                "name":"automation_update","arguments":"{\"mode\":\"view\"}"},
            {"type":"function_call_output","call_id":"call_update","namespace":"codex_app",
                "name":"automation_update","output":"The automation is active."}
        ]
    }));
    let messages = &translated.request.messages;
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0].content, Some(json!("Run the scheduled check.")));
    assert_eq!(messages[1].role, "developer");
    assert_eq!(messages[2].tool_calls[0]["id"], "call_update");
    assert_eq!(messages[3].role, "tool");
    assert_eq!(messages[3].tool_call_id.as_deref(), Some("call_update"));
    assert_eq!(
        messages[3].content,
        Some(json!("The automation is active."))
    );
}

#[test]
fn codex_automation_bootstrap_does_not_relax_other_call_id_validation() {
    let bootstrap = json!({
        "type":"function_call_output", "name":"automation_update",
        "namespace":"codex_app", "output":"Automation context"
    });
    for (key, value) in [
        ("name", json!("read_file")),
        ("name", Value::Null),
        ("namespace", json!("other_app")),
        ("namespace", Value::Null),
        ("type", json!("custom_tool_call_output")),
        ("call_id", json!("")),
        ("call_id", json!(" ")),
        ("call_id", json!(123)),
        ("call_id", json!(false)),
        ("call_id", json!({})),
        ("call_id", json!([])),
        ("call_id", json!("unmatched_call")),
    ] {
        let mut item = bootstrap.clone();
        item[key] = value;
        let request = serde_json::from_value(json!({
            "model":"test", "input":[{"role":"user","content":"Check."}, item]
        }))
        .unwrap();
        assert!(
            matches!(
                responses_to_openai(&request),
                Err(ValidationError::InvalidField { field, .. }) if field == "input.1.call_id"
            ),
            "invalid tool result must still fail: {item}"
        );
    }

    let request = serde_json::from_value(json!({
        "model":"test", "input":[
            {"type":"function_call","call_id":"pending","namespace":"codex_app",
                "name":"automation_update","arguments":"{}"},
            bootstrap
        ]
    }))
    .unwrap();
    assert!(matches!(
        responses_to_openai(&request),
        Err(ValidationError::InvalidField { field, message })
            if field == "input" && message.contains("matching tool output")
    ));
}

#[test]
fn codex_automation_bootstrap_validates_its_output() {
    for (output, field) in [
        (Value::Null, "input.0.output"),
        (json!({"text":"invalid object"}), "input.0.output"),
        (
            json!([{"type":"input_text","text":123}]),
            "input.0.output.0.text",
        ),
        (
            json!([{"type":"input_image","image_url":"https://example.com/a.png"}]),
            "input.0.output.0.type",
        ),
    ] {
        let request = serde_json::from_value(json!({
            "model":"test", "input":[{
                "type":"function_call_output", "name":"automation_update",
                "namespace":"codex_app", "output":output
            }]
        }))
        .unwrap();
        assert!(
            matches!(
                responses_to_openai(&request),
                Err(ValidationError::InvalidField { field: actual, .. }) if actual == field
            ),
            "invalid bootstrap output: {output}"
        );
    }
}

#[test]
fn image_tool_output_remains_in_the_kiro_tool_result_turn() {
    let image = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jB9sAAAAASUVORK5CYII=";
    for next_user in [false, true] {
        let mut input = vec![
            json!({"role":"user","content":"Look at the image."}),
            json!({"type":"function_call","call_id":"image","name":"view_image","arguments":"{}"}),
            json!({"type":"function_call_output","call_id":"image","output":[
                {"type":"input_text","text":"Image follows."},
                {"type":"input_image","image_url":image}
            ]}),
        ];
        if next_user {
            input.push(json!({"role":"user","content":"Describe it."}));
        }
        let translated = normalize(
            json!({"model":"test","input":input,"tools":[{"type":"function","name":"view_image","parameters":{"type":"object"}}]}),
        );
        let mut payload = openai_to_kiro(
            &translated.request,
            &TranslationOptions::new("test", "AI_EDITOR"),
        );
        sanitize_kiro_tool_history(&mut payload);
        validate_kiro_tool_history(&payload).expect("paired image tool");
        let user = if next_user {
            payload
                .conversation_state
                .history
                .iter()
                .filter_map(|m| m.user_input_message.as_ref())
                .next_back()
                .unwrap()
        } else {
            &payload
                .conversation_state
                .current_message
                .user_input_message
        };
        assert_eq!(user.images.len(), 1);
        assert_eq!(
            user.user_input_message_context
                .as_ref()
                .unwrap()
                .tool_results[0]
                .tool_use_id,
            "image"
        );
    }
}

#[test]
fn omitted_token_limit_and_none_reasoning_are_preserved() {
    let translated =
        normalize(json!({"model":"test","input":"hello","reasoning":{"effort":"none"}}));
    assert_eq!(translated.request.max_tokens, None);
    assert_eq!(
        translated.request.thinking.as_ref().unwrap().r#type,
        "disabled"
    );
    assert_eq!(translated.request.reasoning_effort, None);
    let translated = normalize(json!({"model":"test","input":"hello"}));
    assert!(translated.request.thinking.is_none());
}

#[test]
fn nullable_response_controls_use_their_omitted_defaults() {
    for field in ["stream", "include"] {
        let mut request = json!({"model":"test","input":"hello"});
        request[field] = Value::Null;
        let translated = normalize(request);
        assert!(!translated.request.stream);
        assert!(translated.request.stream_options.is_none());
    }
}

#[test]
fn nullable_function_fields_preserve_a_no_argument_tool() {
    let translated = normalize(json!({
        "model":"test","input":"hello",
        "tools":[{"type":"function","name":"list_files",
            "parameters":null,"strict":null,"description":null}]
    }));
    let payload = openai_to_kiro(
        &translated.request,
        &TranslationOptions::new("test", "AI_EDITOR"),
    );
    let tool = payload
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context
        .as_ref()
        .unwrap()
        .tools[0]
        .specification()
        .unwrap();
    assert_eq!(tool.name, "list_files");
    assert_eq!(
        tool.input_schema.json,
        json!({"type":"object","properties":{}})
    );
}

#[test]
fn in_memory_prompt_cache_retention_is_accepted() {
    for retention in ["in_memory", "in-memory"] {
        normalize(json!({"model":"test","input":"hello","prompt_cache_retention":retention}));
    }
}

#[test]
fn allowed_tools_uses_the_responses_shape_and_limits_the_kiro_payload() {
    let translated = normalize(json!({
        "model":"test","input":"Inspect the project.",
        "tools":[
            {"type":"function","name":"read_file","parameters":{"type":"object"}},
            {"type":"function","name":"list_files","parameters":{"type":"object"}},
            {"type":"custom","name":"apply_patch"}
        ],
        "tool_choice":{
            "type":"allowed_tools","mode":"required","tools":[
                {"type":"function","name":"read_file"},
                {"type":"custom","name":"apply_patch"}
            ]
        }
    }));
    assert_eq!(
        translated.request.tool_choice,
        Some(json!({
            "type":"allowed_tools",
            "allowed_tools":{
                "mode":"required",
                "tools":[
                    {"type":"function","function":{"name":"read_file"}},
                    {"type":"custom","custom":{"name":"apply_patch"}}
                ]
            }
        }))
    );
    let payload = openai_to_kiro(
        &translated.request,
        &TranslationOptions::new("test", "AI_EDITOR"),
    );
    let tools = &payload
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context
        .as_ref()
        .unwrap()
        .tools;
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].specification().unwrap().name, "read_file");
    assert_eq!(tools[1].specification().unwrap().name, "apply_patch");
}

#[test]
fn object_shorthand_tool_choices_match_their_string_forms() {
    for choice in ["auto", "none", "required"] {
        let translated = normalize(json!({
            "model":"test","input":"Inspect the project.",
            "tools":[{"type":"function","name":"read_file"}],
            "tool_choice":{"type":choice}
        }));
        assert_eq!(translated.request.tool_choice, Some(json!(choice)));
        let payload = openai_to_kiro(
            &translated.request,
            &TranslationOptions::new("test", "AI_EDITOR"),
        );
        let tool_count = payload
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .as_ref()
            .map_or(0, |context| context.tools.len());
        assert_eq!(tool_count, usize::from(choice != "none"));
    }
}

#[test]
fn unsupported_execution_controls_are_rejected_explicitly() {
    for (field, value) in [
        ("background", json!(true)),
        ("conversation", json!("conv_unknown")),
        ("truncation", json!("invalid")),
        ("max_output_tokens", json!(0)),
        ("context_management", json!([])),
        ("tools", json!([{"type":"web_search"}])),
        (
            "tools",
            json!([{"type":"function","name":"deferred","defer_loading":true}]),
        ),
        (
            "tools",
            json!([{"type":"function","name":"same"},{"type":"custom","name":"same"}]),
        ),
    ] {
        let mut value_request = json!({"model":"test","input":"hello"});
        value_request[field] = value;
        let request = serde_json::from_value(value_request).expect("request JSON");
        assert!(
            responses_to_openai(&request).is_err(),
            "unsupported field: {field}"
        );
    }
}

#[test]
fn unsupported_or_broken_history_is_never_silently_dropped() {
    for input in [
        json!([]),
        json!(null),
        json!({"role":"user"}),
        json!([{"role":"user"}]),
        json!([{"type":123,"role":"user","content":"hello"}]),
        json!([
            {"type":"custom_tool_call","call_id":"call","name":"run","input":"text"},
            {"type":"function_call_output","call_id":"call","output":"wrong kind"}
        ]),
        json!([{"type":"function_call_output","call_id":"missing","output":"ok"}]),
        json!([{"type":"function_call","call_id":"pending","name":"run","arguments":"{}"}]),
        json!([{"type":"reasoning","summary":[],"encrypted_content":"opaque"}]),
        json!([{"type":"item_reference","id":"item_old"}]),
        json!([{"role":"user","content":[{"type":"input_file","file_id":"file_unknown"}]}]),
        json!([{"role":"assistant","content":[{"type":"input_image","image_url":"https://example.com/a.png"}]}]),
        json!([{"type":"unknown_future_action","payload":"do something"}]),
    ] {
        let request = serde_json::from_value(json!({"model":"test","input":input})).unwrap();
        assert!(
            responses_to_openai(&request).is_err(),
            "invalid history: {input}"
        );
    }
}
