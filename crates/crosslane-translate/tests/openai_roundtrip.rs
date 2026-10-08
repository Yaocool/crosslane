//! Property checks for semantic preservation across OpenAI -> Kiro conversion.

use crosslane_translate::{openai_to_kiro, OpenAiRequest, TranslationOptions};
use proptest::prelude::*;
use serde_json::json;

fn text() -> impl Strategy<Value = String> {
    "[A-Za-z0-9][A-Za-z0-9 _.,:-]{0,39}"
}

fn identifier() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{0,15}"
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn openai_kiro_projection_preserves_messages_tools_and_results(
        system in text(),
        first_user in text(),
        assistant in text(),
        final_user in text(),
        tool_result in text(),
        tool_name in identifier(),
        tool_id in identifier(),
        argument in -10_000i64..10_000,
    ) {
        let request: OpenAiRequest = serde_json::from_value(json!({
            "model": "client-model",
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": first_user},
                {
                    "role": "assistant",
                    "content": assistant,
                    "tool_calls": [{
                        "id": tool_id,
                        "type": "function",
                        "function": {
                            "name": tool_name,
                            "arguments": serde_json::to_string(&json!({"value": argument})).expect("arguments")
                        }
                    }]
                },
                {"role": "tool", "tool_call_id": tool_id, "content": tool_result},
                {"role": "user", "content": final_user}
            ],
            "tools": [{
                "type": "function",
                "function": {
                    "name": tool_name,
                    "description": "property tool",
                    "parameters": {
                        "type": "object",
                        "properties": {"value": {"type": "integer"}},
                        "required": ["value"]
                    }
                }
            }],
            "max_tokens": 512
        })).expect("valid OpenAI request");

        let mut options = TranslationOptions::new("resolved-model", "CLI");
        options.enhance_system_prompt = false;
        let payload = openai_to_kiro(&request, &options);
        let state = &payload.conversation_state;

        prop_assert_eq!(payload.protected_history_len(), 2);
        prop_assert_eq!(state.history.len(), 5);
        let protected_system = state.history[0]
            .user_input_message
            .as_ref()
            .expect("protected system turn");
        prop_assert_eq!(&protected_system.content, system.trim());
        prop_assert!(state.history[1].assistant_response_message.is_some());

        let first = state.history[2]
            .user_input_message
            .as_ref()
            .expect("first user turn");
        prop_assert_eq!(&first.content, &first_user);

        let translated_assistant = state.history[3]
            .assistant_response_message
            .as_ref()
            .expect("assistant turn");
        prop_assert_eq!(&translated_assistant.content, &assistant);
        prop_assert_eq!(translated_assistant.tool_uses.len(), 1);
        prop_assert_eq!(&translated_assistant.tool_uses[0].tool_use_id, &tool_id);
        prop_assert_eq!(&translated_assistant.tool_uses[0].name, &tool_name);
        prop_assert_eq!(&translated_assistant.tool_uses[0].input, &json!({"value": argument}));

        let result_turn = state.history[4]
            .user_input_message
            .as_ref()
            .expect("tool-result turn");
        let results = &result_turn
            .user_input_message_context
            .as_ref()
            .expect("result context")
            .tool_results;
        prop_assert_eq!(results.len(), 1);
        prop_assert_eq!(&results[0].tool_use_id, &tool_id);
        prop_assert_eq!(&results[0].content[0].text, &tool_result);

        let current = &state.current_message.user_input_message;
        prop_assert_eq!(&current.content, &final_user);
        prop_assert_eq!(&current.model_id, "resolved-model");
        let tools = &current
            .user_input_message_context
            .as_ref()
            .expect("tool context")
            .tools;
        prop_assert_eq!(tools.len(), 1);
        prop_assert_eq!(
            &tools[0].specification().expect("tool specification").name,
            &tool_name
        );
    }
}
