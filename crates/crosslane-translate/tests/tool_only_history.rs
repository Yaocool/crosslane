//! Tool-only turns must not acquire assistant prose when replayed upstream.

use crosslane_translate::{
    claude_to_kiro, openai_to_kiro, responses_to_openai, sanitize_kiro_tool_history,
    validate_kiro_tool_history, ClaudeRequest, KiroPayload, OpenAiRequest, ResponsesRequest,
    TranslationOptions,
};
use serde_json::{json, Value};

fn options() -> TranslationOptions {
    let mut options = TranslationOptions::new("model", "AI_EDITOR");
    options.enhance_system_prompt = false;
    options
}

fn wire(mut payload: KiroPayload) -> Value {
    let repairs = sanitize_kiro_tool_history(&mut payload);
    assert!(!repairs.has_structured_tool_repair(), "{repairs:?}");
    validate_kiro_tool_history(&payload).expect("paired tool history");
    serde_json::to_value(payload).expect("wire payload")
}

fn assistant_turns(wire: &Value) -> Vec<&Value> {
    wire["conversationState"]["history"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item.get("assistantResponseMessage"))
        .collect()
}

#[test]
fn openai_tool_history_preserves_absent_empty_and_real_text() {
    for (content, expected) in [
        (Value::Null, ""),
        (json!(""), ""),
        (json!([]), ""),
        (json!([{"type":"text","text":""}]), ""),
        (json!("继续检查进度。"), "继续检查进度。"),
        // Real client text must survive even if it matches the old filler.
        (json!("Using tools."), "Using tools."),
    ] {
        let request: OpenAiRequest = serde_json::from_value(json!({
            "model":"model",
            "messages":[
                {"role":"user","content":"Check progress."},
                {"role":"assistant","content":content,"tool_calls":[{
                    "id":"call_poll","type":"function",
                    "function":{"name":"poll","arguments":"{\"session_id\":42}"}
                }]},
                {"role":"tool","tool_call_id":"call_poll","content":"running"}
            ],
            "tools":[{"type":"function","function":{
                "name":"poll","parameters":{"type":"object"}
            }}]
        }))
        .unwrap();
        let wire = wire(openai_to_kiro(&request, &options()));
        let assistants = assistant_turns(&wire);
        assert_eq!(assistants.len(), 1);
        assert_eq!(assistants[0]["content"], expected);
        assert_eq!(
            assistants[0]["toolUses"],
            json!([{
                "toolUseId":"call_poll","name":"poll","input":{"session_id":42}
            }])
        );
        assert_eq!(
            wire["conversationState"]["currentMessage"]["userInputMessage"]
                ["userInputMessageContext"]["toolResults"],
            json!([{
                "toolUseId":"call_poll","status":"success","content":[{"text":"running"}]
            }])
        );
    }
}

#[test]
fn claude_tool_history_preserves_absent_and_real_text() {
    for text in ["", "继续检查进度。", "Using tools."] {
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(json!({"type":"text","text":text}));
        }
        content.push(json!({
            "type":"tool_use","id":"call_poll","name":"poll","input":{"session_id":42}
        }));
        let request: ClaudeRequest = serde_json::from_value(json!({
            "model":"model","max_tokens":128,
            "messages":[
                {"role":"user","content":"Check progress."},
                {"role":"assistant","content":content},
                {"role":"user","content":[{
                    "type":"tool_result","tool_use_id":"call_poll","content":"running"
                }]}
            ],
            "tools":[{"name":"poll","input_schema":{"type":"object"}}]
        }))
        .unwrap();
        let wire = wire(claude_to_kiro(&request, &options()));
        let assistants = assistant_turns(&wire);
        assert_eq!(assistants.len(), 1);
        assert_eq!(assistants[0]["content"], text);
        assert_eq!(
            assistants[0]["toolUses"],
            json!([{
                "toolUseId":"call_poll","name":"poll","input":{"session_id":42}
            }])
        );
        assert_eq!(
            wire["conversationState"]["currentMessage"]["userInputMessage"]
                ["userInputMessageContext"]["toolResults"][0]["toolUseId"],
            "call_poll"
        );
    }
}

#[test]
fn responses_polling_replay_keeps_tool_only_rounds_empty() {
    let mut input = vec![json!({"role":"user","content":"检查进度，直到完成。"})];
    let mut expected_uses = Vec::new();
    let mut expected_text = Vec::new();
    let mut expected_results = Vec::new();
    // Replay the complete growing history, mixing function/custom calls,
    // separate commentary items and polling rounds with no terminal output.
    for round in 0..8 {
        let id = format!("call_{round}");
        let text = if round % 3 == 1 {
            "继续检查进度。"
        } else {
            ""
        };
        if !text.is_empty() {
            input.push(json!({"type":"message","role":"assistant","content":[
                {"type":"output_text","text":text}
            ]}));
        }
        let custom = round % 2 == 0;
        let tool_input = if custom {
            "const r = await tools.write_stdin({session_id:42,chars:\"\",yield_time_ms:30000}); text(r);"
        } else {
            "{\"cell_id\":\"poll-cell\",\"yield_time_ms\":1000}"
        };
        let name = if custom { "exec" } else { "wait" };
        let mut call = json!({
            "type":if custom {"custom_tool_call"} else {"function_call"},
            "call_id":id,"namespace":"functions","name":name
        });
        call[if custom { "input" } else { "arguments" }] = json!(tool_input);
        input.push(call);
        let output = if round % 2 == 0 { "" } else { "still running" };
        input.push(json!({
            "type":if custom {"custom_tool_call_output"} else {"function_call_output"},
            "call_id":id,"output":output
        }));
        expected_text.push(text);
        expected_uses.push(json!({
            "toolUseId":id,"name":format!("functions_{name}"),
            "input":if custom {json!({"input":tool_input})} else {serde_json::from_str::<Value>(tool_input).unwrap()}
        }));
        expected_results.push(json!({
            "toolUseId":id,"status":"success","content":[{"text":output}]
        }));
        let request: ResponsesRequest = serde_json::from_value(json!({
            "model":"model","input":input,
            "tools":[{"type":"namespace","name":"functions","tools":[
                {"type":"custom","name":"exec"},
                {"type":"function","name":"wait","parameters":{"type":"object"}}
            ]}]
        }))
        .unwrap();
        let translated = responses_to_openai(&request).unwrap();
        let wire = wire(openai_to_kiro(&translated.request, &options()));
        let assistants = assistant_turns(&wire);
        assert_eq!(assistants.len(), round + 1);
        for (index, assistant) in assistants.iter().enumerate() {
            assert_eq!(assistant["content"], expected_text[index], "round {index}");
            assert_eq!(assistant["toolUses"], json!([expected_uses[index]]));
        }
        let state = &wire["conversationState"];
        let results = state["history"]
            .as_array()
            .unwrap()
            .iter()
            .chain(std::iter::once(&state["currentMessage"]))
            .filter_map(|item| {
                item["userInputMessage"]["userInputMessageContext"]["toolResults"].as_array()
            })
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(results, expected_results);
    }
}
