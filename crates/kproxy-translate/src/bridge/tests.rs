use super::*;

const PROTOCOLS: [ProviderProtocol; 3] = [
    ProviderProtocol::ClaudeMessages,
    ProviderProtocol::OpenAiChat,
    ProviderProtocol::OpenAiResponses,
];

#[test]
fn all_six_request_directions_preserve_json_schema_output_constraints() {
    let schema = json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false});
    for from in PROTOCOLS {
        let mut body = request(from);
        body["model"] = json!("claude-sonnet-4.6");
        match from {
            ProviderProtocol::ClaudeMessages => {
                body["output_config"] =
                    json!({"format":{"type":"json_schema","schema":schema},"effort":"high"});
                body["thinking"] = json!({"type":"adaptive"});
            }
            ProviderProtocol::OpenAiChat => {
                body["response_format"] = json!({"type":"json_schema","json_schema":{"name":"answer","schema":schema,"strict":true}});
                body["reasoning_effort"] = json!("high");
            }
            ProviderProtocol::OpenAiResponses => {
                body["text"] = json!({"format":{"type":"json_schema","name":"answer","schema":schema,"strict":true}});
                body["reasoning"] = json!({"effort":"high"});
            }
        }
        for to in PROTOCOLS.into_iter().filter(|to| *to != from) {
            let converted = translate_request(&body, from, to, None).unwrap();
            let pointer = match to {
                ProviderProtocol::ClaudeMessages => "/output_config/format/schema",
                ProviderProtocol::OpenAiChat => "/response_format/json_schema/schema",
                ProviderProtocol::OpenAiResponses => "/text/format/schema",
            };
            assert_eq!(
                converted.pointer(pointer),
                Some(&schema),
                "{from:?}->{to:?}"
            );
            if to == ProviderProtocol::ClaudeMessages {
                assert_eq!(converted["output_config"]["effort"], "high");
            }
        }
    }
}

#[test]
fn messages_adaptation_does_not_silently_drop_invalid_or_unsupported_output_formats() {
    let mut body = request(ProviderProtocol::OpenAiChat);
    for format in [
        json!({"type":"json_object"}),
        json!({"type":"json_schema","json_schema":"invalid"}),
    ] {
        body["response_format"] = format;
        assert!(translate_request(
            &body,
            ProviderProtocol::OpenAiChat,
            ProviderProtocol::ClaudeMessages,
            None
        )
        .is_err());
    }
}

#[test]
fn truncated_tool_streams_preserve_partial_arguments_and_length_stop() {
    let partial = "{\"q\":";
    for from in PROTOCOLS {
        let chunks = match from {
            ProviderProtocol::OpenAiChat => vec![
                json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"lookup","arguments":partial}}]}}]}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}),
            ],
            ProviderProtocol::ClaudeMessages => vec![
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_a","name":"lookup","input":{}}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":partial}}),
                json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"}}),
                json!({"type":"message_stop"}),
            ],
            ProviderProtocol::OpenAiResponses => vec![
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_a","name":"lookup","arguments":""}}),
                json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":partial}),
                json!({"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[]}}),
            ],
        };
        let mut input = chunks
            .iter()
            .map(|chunk| format!("data: {chunk}\n\n"))
            .collect::<String>();
        if from == ProviderProtocol::OpenAiChat {
            input.push_str("data: [DONE]\n\n");
        }
        for to in PROTOCOLS.into_iter().filter(|to| *to != from) {
            let mut bridge = StreamBridge::new(from, to, "test");
            let mut frames = bridge
                .push(input.as_bytes())
                .unwrap_or_else(|error| panic!("{from:?}->{to:?}: {error}"));
            frames.extend(bridge.finish().unwrap());
            let values = events(&frames);
            match to {
                ProviderProtocol::ClaudeMessages => assert!(values
                    .iter()
                    .any(|v| v["delta"]["stop_reason"] == "max_tokens")),
                ProviderProtocol::OpenAiChat => assert!(values
                    .iter()
                    .any(|v| v["choices"][0]["finish_reason"] == "length")),
                ProviderProtocol::OpenAiResponses => {
                    let terminal = values.last().unwrap();
                    assert_eq!(terminal["type"], "response.incomplete");
                    assert_eq!(terminal["response"]["output"][0]["arguments"], partial);
                    assert_eq!(terminal["response"]["output"][0]["status"], "incomplete");
                }
            }
        }
    }
}

#[test]
fn refusal_content_survives_json_and_stream_adaptation() {
    let input = json!({"model":"test","choices":[{"index":0,"message":{"role":"assistant","content":null,"refusal":"Unable to help"},"finish_reason":"stop"}]});
    for to in [
        ProviderProtocol::ClaudeMessages,
        ProviderProtocol::OpenAiResponses,
    ] {
        let output = translate_response(&input, ProviderProtocol::OpenAiChat, to).unwrap();
        let mut bridge = StreamBridge::new(ProviderProtocol::OpenAiChat, to, "test");
        let chunks = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"refusal\":\"Unable to help\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let mut frames = bridge.push(chunks).unwrap();
        frames.extend(bridge.finish().unwrap());
        assert!(String::from_utf8(frames.concat())
            .unwrap()
            .contains("Unable to help"));
        if to == ProviderProtocol::ClaudeMessages {
            assert_eq!(output["content"][0]["text"], "Unable to help");
        } else {
            assert_eq!(output["output"][0]["content"][0]["type"], "refusal");
            assert_eq!(
                events(&frames).last().unwrap()["response"]["output"][0]["content"],
                output["output"][0]["content"]
            );
            let replay = translate_response(&output, to, ProviderProtocol::OpenAiChat).unwrap();
            assert_eq!(replay["choices"][0]["message"]["refusal"], "Unable to help");
            let mut reverse = StreamBridge::new(to, ProviderProtocol::OpenAiChat, "test");
            let mut reversed = reverse.push(&frames.concat()).unwrap();
            reversed.extend(reverse.finish().unwrap());
            let refusal = events(&reversed)
                .iter()
                .filter_map(|v| v["choices"][0]["delta"]["refusal"].as_str())
                .collect::<String>();
            assert_eq!(refusal, "Unable to help");
        }
    }
}

#[test]
fn malformed_structured_fields_and_sse_data_return_errors() {
    for extra in [
        json!({"tools":[{"type":"function","function":"invalid"}]}),
        json!({"response_format":{"type":"json_schema","json_schema":"invalid"}}),
    ] {
        let mut input = json!({"model":"test","messages":[{"role":"user","content":"hello"}]});
        input
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(translate_request(
            &input,
            ProviderProtocol::OpenAiChat,
            ProviderProtocol::OpenAiResponses,
            None
        )
        .is_err());
    }
    let mut bridge = StreamBridge::new(
        ProviderProtocol::OpenAiChat,
        ProviderProtocol::ClaudeMessages,
        "test",
    );
    assert!(bridge.push(b"event: chunk\ndata: [1,2]\n\n").is_err());
}

#[test]
fn unreported_usage_is_distinct_from_reported_zero_usage() {
    let mut input = response(ProviderProtocol::OpenAiChat);
    input.as_object_mut().unwrap().remove("usage");
    let output = translate_response(
        &input,
        ProviderProtocol::OpenAiChat,
        ProviderProtocol::ClaudeMessages,
    )
    .unwrap();
    assert_eq!(output["kproxy_usage_source"], "unreported");
    assert_eq!(output["copilot_usage"], input["copilot_usage"]);
    input["usage"] = json!({"prompt_tokens":0,"completion_tokens":0});
    let output = translate_response(
        &input,
        ProviderProtocol::OpenAiChat,
        ProviderProtocol::ClaudeMessages,
    )
    .unwrap();
    assert!(output.get("kproxy_usage_source").is_none());
}

#[test]
fn adapted_tool_outputs_preserve_multimodal_content_in_both_directions() {
    let response = json!({"model":"test","input":[
        {"type":"function_call","call_id":"a","name":"read","arguments":"{}"},
        {"type":"function_call_output","call_id":"a","output":[
            {"type":"input_text","text":"result"},{"type":"input_image","image_url":"data:image/png;base64,aGVsbG8="}
        ]}
    ]});
    for native in [
        ProviderProtocol::OpenAiChat,
        ProviderProtocol::ClaudeMessages,
    ] {
        let request =
            translate_request(&response, ProviderProtocol::OpenAiResponses, native, None).unwrap();
        let replay =
            translate_request(&request, native, ProviderProtocol::OpenAiResponses, None).unwrap();
        let result = replay["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call_output")
            .unwrap();
        assert_eq!(result["output"][0]["type"], "input_text");
        assert_eq!(result["output"][1]["type"], "input_image");
        assert_eq!(
            result["output"][1]["image_url"],
            "data:image/png;base64,aGVsbG8="
        );
    }
}

#[test]
fn thinking_uses_the_selected_claude_dialect_and_respects_output_budget() {
    for (model, kind) in [
        ("claude-sonnet-4.5", "enabled"),
        ("claude-sonnet-4.6", "adaptive"),
        ("claude-opus-4-6", "adaptive"),
    ] {
        let input = json!({"model":model,"messages":[{"role":"user","content":"think"}],"reasoning_effort":"high","max_completion_tokens":2048,"temperature":0.2});
        let output = translate_request(
            &input,
            ProviderProtocol::OpenAiChat,
            ProviderProtocol::ClaudeMessages,
            None,
        )
        .unwrap();
        assert_eq!(output["thinking"]["type"], kind);
        assert!(output.get("temperature").is_none());
        if kind == "enabled" {
            assert_eq!(output["thinking"]["budget_tokens"], 2047);
        }
    }
}

#[test]
fn responses_stream_errors_use_the_responses_event_schema() {
    let mut bridge = StreamBridge::new(
        ProviderProtocol::OpenAiChat,
        ProviderProtocol::OpenAiResponses,
        "test",
    );
    let frames = bridge
        .push(b"data: {\"error\":{\"message\":\"failed\",\"code\":\"rate_limit\"}}\n\n")
        .unwrap();
    let text = String::from_utf8(frames.concat()).unwrap();
    let data: Value = serde_json::from_str(
        text.lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(data["type"], "error");
    assert_eq!(data["message"], "failed");
    assert_eq!(data["code"], "rate_limit");
    assert!(!text.contains("response.completed"));
}

fn request(protocol: ProviderProtocol) -> Value {
    match protocol {
        ProviderProtocol::ClaudeMessages => json!({"model":"test","system":"system",
            "messages":[{"role":"user","content":[{"type":"text","text":"你好"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGVsbG8="}}]},
                {"role":"assistant","content":[{"type":"tool_use","id":"call_a","name":"lookup","input":{"q":"x"}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_a","content":"found"}]}],
            "tools":[{"name":"lookup","description":"look up","input_schema":{"type":"object","properties":{"q":{"type":"string"}}}}],
            "tool_choice":{"type":"tool","name":"lookup"},"max_tokens":64,"stream":true}),
        ProviderProtocol::OpenAiChat => {
            json!({"model":"test","messages":[{"role":"system","content":"system"},
            {"role":"user","content":[{"type":"text","text":"你好"},{"type":"image_url","image_url":{"url":"data:image/png;base64,aGVsbG8="}}]},
            {"role":"assistant","tool_calls":[{"id":"call_a","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"x\"}"}}]},
            {"role":"tool","tool_call_id":"call_a","content":"found"}],
            "tools":[{"type":"function","function":{"name":"lookup","description":"look up","parameters":{"type":"object","properties":{"q":{"type":"string"}}}}}],
            "tool_choice":{"type":"function","function":{"name":"lookup"}},"max_completion_tokens":64,"stream":true})
        }
        ProviderProtocol::OpenAiResponses => {
            json!({"model":"test","instructions":"system","input":[
            {"role":"user","content":[{"type":"input_text","text":"你好"},{"type":"input_image","image_url":"data:image/png;base64,aGVsbG8="}]},
            {"type":"function_call","call_id":"call_a","name":"lookup","arguments":"{\"q\":\"x\"}"},
            {"type":"function_call_output","call_id":"call_a","output":"found"}],
            "tools":[{"type":"function","name":"lookup","description":"look up","parameters":{"type":"object","properties":{"q":{"type":"string"}}}}],
            "tool_choice":{"type":"function","name":"lookup"},"max_output_tokens":64,"stream":true})
        }
    }
}

fn response(protocol: ProviderProtocol) -> Value {
    let mut value = match protocol {
        ProviderProtocol::ClaudeMessages => {
            json!({"id":"message_1","model":"test","type":"message","role":"assistant",
            "content":[{"type":"text","text":"你好"},{"type":"tool_use","id":"call_a","name":"lookup","input":{"q":"x"}}],
            "stop_reason":"tool_use","usage":{"input_tokens":8,"output_tokens":5,"cache_read_input_tokens":12}})
        }
        ProviderProtocol::OpenAiChat => {
            json!({"id":"chat_1","model":"test","choices":[{"index":0,"message":{"role":"assistant","content":"你好",
            "tool_calls":[{"id":"call_a","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"x\"}"}}]},"finish_reason":"tool_calls"}],
            "usage":{"prompt_tokens":20,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":12}}})
        }
        ProviderProtocol::OpenAiResponses => {
            json!({"id":"response_1","model":"test","status":"completed","output":[
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"你好"}]},
            {"type":"function_call","call_id":"call_a","name":"lookup","arguments":"{\"q\":\"x\"}"}],
            "usage":{"input_tokens":20,"output_tokens":5,"input_tokens_details":{"cached_tokens":12}}})
        }
    };
    value["copilot_usage"] = json!({"total_nano_aiu":12345,"opaque":{"new_field":true}});
    value
}

#[test]
fn all_six_request_directions_preserve_history_tools_images_and_limits() {
    for from in PROTOCOLS {
        for to in PROTOCOLS {
            let original = request(from);
            let converted = translate_request(&original, from, to, Some(128)).unwrap();
            assert_eq!(original, request(from), "input must not be mutated");
            assert_eq!(converted["model"], "test");
            assert_eq!(converted["stream"], true);
            let raw = converted.to_string();
            for marker in ["system", "你好", "aGVsbG8=", "call_a", "lookup", "found"] {
                assert!(raw.contains(marker), "{from:?}->{to:?}: missing {marker}");
            }
            let limit = match to {
                ProviderProtocol::ClaudeMessages => &converted["max_tokens"],
                ProviderProtocol::OpenAiResponses => &converted["max_output_tokens"],
                _ => converted
                    .get("max_completion_tokens")
                    .unwrap_or(&converted["max_tokens"]),
            };
            assert_eq!(limit, 64);
        }
    }
}

#[test]
fn all_six_responses_preserve_tools_usage_cache_and_raw_billing() {
    for from in PROTOCOLS {
        for to in PROTOCOLS {
            let converted = translate_response(&response(from), from, to).unwrap();
            let raw = converted.to_string();
            for marker in ["你好", "call_a", "lookup"] {
                assert!(raw.contains(marker), "{from:?}->{to:?}");
            }
            assert_eq!(converted["copilot_usage"], response(from)["copilot_usage"]);
            match to {
                ProviderProtocol::ClaudeMessages => {
                    assert_eq!(converted["usage"]["input_tokens"], 8);
                    assert_eq!(converted["usage"]["cache_read_input_tokens"], 12);
                    assert_eq!(converted["stop_reason"], "tool_use");
                }
                ProviderProtocol::OpenAiChat => {
                    assert_eq!(converted["usage"]["prompt_tokens"], 20);
                    assert_eq!(
                        converted["usage"]["prompt_tokens_details"]["cached_tokens"],
                        12
                    );
                }
                ProviderProtocol::OpenAiResponses => {
                    assert_eq!(converted["usage"]["input_tokens"], 20);
                    assert_eq!(
                        converted["usage"]["input_tokens_details"]["cached_tokens"],
                        12
                    );
                }
            }
        }
    }
}

fn stream(protocol: ProviderProtocol) -> Vec<u8> {
    let data = match protocol {
        ProviderProtocol::OpenAiChat => vec![
            json!({"id":"chat_1","model":"test","choices":[{"index":0,"delta":{"role":"assistant","content":"你好"}}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"lookup","arguments":"{\"q\":"}}]}}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"x\"}"}}]},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":response(protocol)["usage"],"copilot_usage":response(protocol)["copilot_usage"]}),
        ],
        ProviderProtocol::ClaudeMessages => vec![
            json!({"type":"message_start","message":{"id":"message_1","model":"test","usage":{"input_tokens":8,"cache_read_input_tokens":12,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"你好"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_a","name":"lookup","input":{}}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"q\":"}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"x\"}"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5},"copilot_usage":response(protocol)["copilot_usage"]}),
            json!({"type":"message_stop"}),
        ],
        ProviderProtocol::OpenAiResponses => vec![
            json!({"type":"response.created","response":{"id":"response_1","model":"test"}}),
            json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"你好"}),
            json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call_a","name":"lookup","arguments":""}}),
            json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"q\":"}),
            json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"\"x\"}"}),
            json!({"type":"response.completed","response":response(protocol)}),
        ],
    };
    let mut text = data
        .into_iter()
        .map(|event| format!(": heartbeat\r\ndata: {event}\r\n\r\n"))
        .collect::<String>();
    if protocol == ProviderProtocol::OpenAiChat {
        text.push_str("data: [DONE]\r\n\r\n");
    }
    text.into_bytes()
}

fn events(frames: &[Vec<u8>]) -> Vec<Value> {
    frames
        .iter()
        .filter_map(|frame| std::str::from_utf8(frame).ok())
        .flat_map(str::lines)
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str(data).ok())
        .collect()
}

#[test]
fn all_six_stream_directions_keep_utf8_tools_and_trailing_usage() {
    for from in PROTOCOLS {
        for to in PROTOCOLS {
            if from == to {
                continue;
            }
            let mut bridge = StreamBridge::new(from, to, "test");
            let mut frames = Vec::new();
            // Every byte boundary, including multibyte text and CRLF.
            for byte in stream(from) {
                frames.extend(bridge.push(&[byte]).unwrap());
            }
            frames.extend(bridge.finish().unwrap());
            let values = events(&frames);
            let raw = String::from_utf8(frames.concat()).unwrap();
            assert!(raw.contains("你好"), "{from:?}->{to:?}");
            assert!(raw.contains("lookup") && raw.contains("call_a"));
            assert!(raw.contains("12345"), "billing missing");
            match to {
                ProviderProtocol::ClaudeMessages => {
                    let start = values
                        .iter()
                        .filter(|v| v["type"] == "content_block_start")
                        .collect::<Vec<_>>();
                    assert_eq!(start.len(), 2);
                    assert_eq!(start[1]["index"], 1);
                    let end = values
                        .iter()
                        .find(|v| v["type"] == "message_delta")
                        .unwrap();
                    assert_eq!(end["usage"]["input_tokens"], 8);
                    assert_eq!(end["usage"]["cache_read_input_tokens"], 12);
                    assert_eq!(end["delta"]["stop_reason"], "tool_use");
                    assert_eq!(values.last().unwrap()["type"], "message_stop");
                }
                ProviderProtocol::OpenAiChat => {
                    assert!(raw.ends_with("data: [DONE]\n\n"));
                    let usage = values.iter().find(|v| v.get("usage").is_some()).unwrap();
                    assert_eq!(usage["usage"]["prompt_tokens"], 20);
                    assert_eq!(usage["usage"]["completion_tokens"], 5);
                }
                ProviderProtocol::OpenAiResponses => {
                    let done = &values.last().unwrap()["response"];
                    assert_eq!(done["output"][1]["arguments"], "{\"q\":\"x\"}");
                    assert_eq!(done["usage"]["input_tokens"], 20);
                    for (index, value) in values.iter().enumerate() {
                        assert_eq!(value["sequence_number"], index);
                    }
                }
            }
        }
    }
}

#[test]
fn native_requests_keep_all_extensions_and_stateful_features() {
    let body = json!({"model":"test","input":"hello","previous_response_id":"r1","background":true,"unknown":{"future":1}});
    assert_eq!(
        translate_request(&body, PROTOCOLS[2], PROTOCOLS[2], None).unwrap(),
        body
    );
    assert!(translate_request(&body, PROTOCOLS[2], PROTOCOLS[0], None)
        .unwrap_err()
        .to_string()
        .contains("previous_response_id"));
}

#[test]
fn interrupted_and_failed_streams_do_not_claim_completion() {
    let mut bridge = StreamBridge::new(PROTOCOLS[1], PROTOCOLS[2], "test");
    let output = bridge
        .push(b"data: {\"choices\":[{\"delta\":{\"content\":\"part\"}}]}\n\n")
        .unwrap();
    assert!(!String::from_utf8(output.concat())
        .unwrap()
        .contains("response.completed"));
    assert!(bridge.finish().is_err());
    let mut bridge = StreamBridge::new(PROTOCOLS[2], PROTOCOLS[0], "test");
    let frames = bridge.push(b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"failed\"}}}\n\n").unwrap();
    assert_eq!(events(&frames)[0]["type"], "error");
    assert!(bridge.finish().unwrap().is_empty());
}

#[test]
fn parallel_tool_outputs_are_merged_into_one_anthropic_user_turn() {
    let input = json!({"model":"test","messages":[{"role":"assistant","tool_calls":[
        {"id":"a","function":{"name":"lookup","arguments":"{}"}},{"id":"b","function":{"name":"lookup","arguments":"{}"}}]},
        {"role":"tool","tool_call_id":"a","content":"a"},{"role":"tool","tool_call_id":"b","content":"b"}]});
    let value = translate_request(&input, PROTOCOLS[1], PROTOCOLS[0], Some(1024)).unwrap();
    assert_eq!(value["messages"].as_array().unwrap().len(), 2);
    assert_eq!(value["messages"][1]["content"].as_array().unwrap().len(), 2);
    assert_eq!(value["max_tokens"], 1024);
}
