use super::{array, invalid, json, string, BridgeError, ProviderProtocol, Value};

/// Convert only when the selected account does not support the caller's wire
/// protocol. Unknown native fields remain untouched in the native path.
pub fn translate_request(
    body: &Value,
    from: ProviderProtocol,
    to: ProviderProtocol,
    max_output_tokens: Option<u64>,
) -> Result<Value, BridgeError> {
    if from == to {
        return Ok(body.clone());
    }
    if !body.is_object() {
        return Err(invalid("request body must be an object"));
    }
    let chat = match from {
        ProviderProtocol::OpenAiChat => body.clone(),
        ProviderProtocol::ClaudeMessages => messages_to_chat(body)?,
        ProviderProtocol::OpenAiResponses => responses_to_chat(body)?,
    };
    if chat["messages"].as_array().is_none() {
        return Err(invalid("messages must be an array"));
    }
    match to {
        ProviderProtocol::OpenAiChat => Ok(chat),
        ProviderProtocol::ClaudeMessages => chat_to_messages(&chat, max_output_tokens),
        ProviderProtocol::OpenAiResponses => chat_to_responses(&chat),
    }
}

fn copy_fields(from: &Value, to: &mut Value, fields: &[&str]) {
    for field in fields {
        if let Some(value) = from.get(*field).filter(|value| !value.is_null()) {
            to[*field] = value.clone();
        }
    }
}

fn base(body: &Value) -> Value {
    let mut output =
        json!({"model":body["model"],"stream":body["stream"].as_bool().unwrap_or(false)});
    copy_fields(body, &mut output, &["temperature", "top_p", "user"]);
    output
}

fn anthropic_part(part: &Value) -> Result<Option<Value>, BridgeError> {
    let mut output = match string(&part["type"]) {
        "text" => json!({"type":"text","text":part["text"]}),
        "image" => {
            let source = &part["source"];
            let url = if source["type"] == "base64" {
                format!(
                    "data:{};base64,{}",
                    string(&source["media_type"]),
                    string(&source["data"])
                )
            } else if source["type"] == "url" {
                string(&source["url"]).to_owned()
            } else {
                return Err(invalid("cannot adapt this image source"));
            };
            json!({"type":"image_url","image_url":{"url":url}})
        }
        "document" if part["source"]["type"] == "text" => {
            json!({"type":"text","text":part["source"]["data"]})
        }
        "document" if part["source"]["type"] == "base64" => {
            json!({"type":"file","file":{"filename":part["title"].as_str().unwrap_or("document.pdf"),"file_data":format!("data:{};base64,{}", string(&part["source"]["media_type"]), string(&part["source"]["data"]))}})
        }
        "tool_use" | "tool_result" | "thinking" | "redacted_thinking" => return Ok(None),
        _ => {
            return Err(invalid(format!(
                "cannot adapt content block {}",
                string(&part["type"])
            )))
        }
    };
    copy_fields(part, &mut output, &["cache_control"]);
    Ok(Some(output))
}

fn anthropic_content(content: &Value) -> Result<Value, BridgeError> {
    if content.is_string() {
        return Ok(content.clone());
    }
    if !content.is_array() {
        return Err(invalid("message content must be a string or array"));
    }
    Ok(Value::Array(
        array(content)
            .iter()
            .map(anthropic_part)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect(),
    ))
}

fn messages_to_chat(body: &Value) -> Result<Value, BridgeError> {
    if !body["messages"].is_array() {
        return Err(invalid("messages must be an array"));
    }
    let mut output = base(body);
    let mut messages = Vec::new();
    if let Some(system) = body.get("system").filter(|v| !v.is_null()) {
        messages.push(json!({"role":"system","content":anthropic_content(system)?}));
    }
    for message in array(&body["messages"]) {
        let content = &message["content"];
        // Tool results must immediately follow the assistant's tool calls.
        for part in array(content)
            .iter()
            .filter(|part| part["type"] == "tool_result")
        {
            messages.push(json!({"role":"tool","tool_call_id":part["tool_use_id"],
                "content":anthropic_content(part.get("content").unwrap_or(&json!("")))?}));
        }
        let mut converted = json!({"role":message["role"],"content":anthropic_content(content)?});
        let tools = array(content).iter().filter(|part| part["type"] == "tool_use").map(|part| {
            json!({"id":part["id"],"type":"function","function":{"name":part["name"],"arguments":part["input"].to_string()}})
        }).collect::<Vec<_>>();
        if !tools.is_empty() {
            converted["tool_calls"] = json!(tools);
        }
        let thinking = array(content)
            .iter()
            .filter(|part| part["type"] == "thinking")
            .cloned()
            .collect::<Vec<_>>();
        if !thinking.is_empty() {
            converted["reasoning_content"] = json!(thinking
                .iter()
                .filter_map(|p| p["thinking"].as_str())
                .collect::<String>());
            converted["thinking_blocks"] = json!(thinking);
        }
        if converted["content"]
            .as_array()
            .is_none_or(|parts| !parts.is_empty())
            || converted.get("tool_calls").is_some()
            || converted.get("reasoning_content").is_some()
        {
            messages.push(converted);
        }
    }
    output["messages"] = json!(messages);
    if let Some(limit) = body.get("max_tokens") {
        output["max_completion_tokens"] = limit.clone();
    }
    if let Some(stop) = body.get("stop_sequences") {
        output["stop"] = stop.clone();
    }
    if let Some(thinking) = body.get("thinking") {
        if thinking["type"] == "disabled" {
            output["reasoning_effort"] = json!("none");
        } else if thinking["type"] == "enabled" || thinking["type"] == "adaptive" {
            output["reasoning_effort"] = body
                .pointer("/output_config/effort")
                .cloned()
                .unwrap_or(json!("high"));
        }
    }
    if let Some(tools) = body.get("tools") {
        output["tools"] = json!(array(tools)
            .iter()
            .map(|tool| {
                if tool
                    .get("type")
                    .is_some_and(|t| t != "custom" && t != "function")
                {
                    return Err(invalid(
                        "hosted Anthropic tools require native Messages support",
                    ));
                }
                let mut function = json!({"name":tool["name"],"parameters":tool["input_schema"]});
                copy_fields(tool, &mut function, &["description", "strict"]);
                let mut converted = json!({"type":"function","function":function});
                copy_fields(tool, &mut converted, &["cache_control"]);
                Ok(converted)
            })
            .collect::<Result<Vec<_>, BridgeError>>()?);
    }
    if let Some(choice) = body.get("tool_choice") {
        output["tool_choice"] = match string(&choice["type"]) {
            "tool" => json!({"type":"function","function":{"name":choice["name"]}}),
            "any" => json!("required"),
            "none" => json!("none"),
            _ => json!("auto"),
        };
        if let Some(disable) = choice["disable_parallel_tool_use"].as_bool() {
            output["parallel_tool_calls"] = json!(!disable);
        }
    }
    if let Some(format) = body
        .pointer("/output_config/format")
        .filter(|v| !v.is_null())
    {
        if format["type"] != "json_schema" || !format["schema"].is_object() {
            return Err(invalid(
                "output_config.format must contain a JSON schema object",
            ));
        }
        output["response_format"] = json!({"type":"json_schema","json_schema":{
            "name":"response","schema":format["schema"],"strict":true
        }});
    }
    Ok(output)
}

fn responses_content_to_chat(content: &Value) -> Result<Value, BridgeError> {
    if content.is_string() {
        return Ok(content.clone());
    }
    if !content.is_array() {
        return Err(invalid("Responses content must be a string or array"));
    }
    Ok(json!(array(content).iter().map(|part| {
        Ok(match string(&part["type"]) {
            "input_text" | "output_text" | "text" => json!({"type":"text","text":part["text"]}),
            "input_image" => json!({"type":"image_url","image_url":{"url":part["image_url"],"detail":part["detail"].as_str().unwrap_or("auto")}}),
            "input_file" => {
                if part["file_data"].is_null() { return Err(invalid("file IDs and remote files require native Responses support")); }
                json!({"type":"file","file":{"filename":part["filename"],"file_data":part["file_data"]}})
            }
            "refusal" => json!({"type":"text","text":part["refusal"]}),
            _ => return Err(invalid(format!("cannot adapt Responses content {}", string(&part["type"])))),
        })
    }).collect::<Result<Vec<Value>, BridgeError>>()?))
}

fn responses_to_chat(body: &Value) -> Result<Value, BridgeError> {
    let names = super::ToolNames::for_responses(body)?;
    for field in ["previous_response_id", "conversation"] {
        if body.get(field).is_some_and(|v| !v.is_null()) {
            return Err(invalid(format!("{field} requires native Responses support; send the complete input history for this model")));
        }
    }
    if body["background"] == true {
        return Err(invalid(
            "background execution requires native Responses support",
        ));
    }
    let mut output = base(body);
    let mut messages = Vec::new();
    if let Some(instructions) = body.get("instructions").filter(|v| !v.is_null()) {
        messages.push(json!({"role":"system","content":instructions}));
    }
    if let Some(input) = body["input"].as_str() {
        messages.push(json!({"role":"user","content":input}));
    } else if let Some(items) = body["input"].as_array() {
        for item in items {
            match item["type"].as_str().unwrap_or("message") {
                "message" => {
                    let content = responses_content_to_chat(&item["content"])?;
                    messages.push(json!({"role":item["role"],"content":content}));
                }
                "function_call" => {
                    let tool = json!({"id":item["call_id"],"type":"function","function":{"name":names.upstream_name(item),"arguments":item["arguments"]}});
                    if let Some(previous) = messages
                        .last_mut()
                        .filter(|previous| previous["role"] == "assistant")
                    {
                        previous
                            .as_object_mut()
                            .unwrap()
                            .entry("tool_calls")
                            .or_insert(json!([]))
                            .as_array_mut()
                            .unwrap()
                            .push(tool);
                    } else {
                        messages
                            .push(json!({"role":"assistant","content":null,"tool_calls":[tool]}));
                    }
                }
                "function_call_output" => messages.push(
                    json!({"role":"tool","tool_call_id":item["call_id"],"content":responses_content_to_chat(&item["output"])?}),
                ),
                "reasoning" => {
                    let summary = array(&item["summary"])
                        .iter()
                        .filter_map(|part| part["text"].as_str())
                        .collect::<String>();
                    if !summary.is_empty() {
                        messages.push(
                            json!({"role":"assistant","content":null,"reasoning_content":summary}),
                        );
                    }
                }
                // Completed provider-side bookkeeping does not add a user turn.
                _ => {}
            }
        }
    } else {
        return Err(invalid("input must be a string or array"));
    }
    output["messages"] = json!(messages);
    if let Some(limit) = body.get("max_output_tokens") {
        output["max_completion_tokens"] = limit.clone();
    }
    if let Some(effort) = body.pointer("/reasoning/effort") {
        output["reasoning_effort"] = effort.clone();
    }
    if let Some(format) = body.pointer("/text/format") {
        output["response_format"] = if format["type"] == "json_schema" {
            let mut schema = format.clone();
            schema.as_object_mut().unwrap().remove("type");
            json!({"type":"json_schema","json_schema":schema})
        } else {
            format.clone()
        };
    }
    copy_fields(
        body,
        &mut output,
        &[
            "parallel_tool_calls",
            "service_tier",
            "prompt_cache_key",
            "prompt_cache_retention",
        ],
    );
    if let Some(tools) = body.get("tools") {
        let mut flattened = Vec::new();
        for tool in array(tools) {
            if tool["type"] == "namespace" {
                for child in array(&tool["tools"]) {
                    let mut child = child.clone();
                    child["namespace"] = tool["name"].clone();
                    flattened.push(child);
                }
            } else {
                flattened.push(tool.clone());
            }
        }
        output["tools"] = json!(flattened
            .iter()
            .map(|tool| {
                if tool["type"] != "function" {
                    return Err(invalid(
                        "hosted Responses tools require native Responses support",
                    ));
                }
                let mut function = tool.clone();
                function.as_object_mut().unwrap().remove("type");
                function.as_object_mut().unwrap().remove("namespace");
                function["name"] = json!(names.upstream_name(tool));
                Ok(json!({"type":"function","function":function}))
            })
            .collect::<Result<Vec<_>, BridgeError>>()?);
    }
    if let Some(choice) = body.get("tool_choice").filter(|choice| !choice.is_null()) {
        output["tool_choice"] = match choice["type"].as_str() {
            Some("function") => {
                json!({"type":"function","function":{"name":names.upstream_name(choice)}})
            }
            Some("auto" | "none" | "required") => choice["type"].clone(),
            Some("allowed_tools") => {
                let allowed = choice.get("allowed_tools").unwrap_or(choice);
                let mode = string(&allowed["mode"]);
                if !matches!(mode, "auto" | "required") || !allowed["tools"].is_array() {
                    return Err(invalid(
                        "allowed_tools requires mode auto or required and a tools array",
                    ));
                }
                let selected = array(&allowed["tools"])
                    .iter()
                    .map(|tool| names.upstream_name(tool))
                    .collect::<std::collections::HashSet<_>>();
                let tools = output["tools"]
                    .as_array_mut()
                    .ok_or_else(|| invalid("allowed_tools requires tool definitions"))?;
                if selected.iter().any(|name| {
                    !tools
                        .iter()
                        .any(|tool| tool["function"]["name"] == name.as_str())
                }) {
                    return Err(invalid("allowed_tools references an unknown function"));
                }
                tools.retain(|tool| {
                    tool["function"]["name"]
                        .as_str()
                        .is_some_and(|name| selected.contains(name))
                });
                if tools.is_empty() {
                    if mode == "required" {
                        return Err(invalid("required allowed_tools must not be empty"));
                    }
                    json!("none")
                } else {
                    json!(mode)
                }
            }
            _ => choice.clone(),
        };
    }
    Ok(output)
}

fn chat_part_to_messages(part: &Value) -> Result<Value, BridgeError> {
    let mut output = match string(&part["type"]) {
        "text" => part.clone(),
        "image_url" => {
            let url = part["image_url"]
                .as_str()
                .unwrap_or_else(|| string(&part["image_url"]["url"]));
            json!({"type":"image","source":data_source(url)?})
        }
        "file" => {
            json!({"type":"document","source":data_source(string(&part["file"]["file_data"]))?})
        }
        _ => {
            return Err(invalid(format!(
                "cannot adapt Chat content {}",
                string(&part["type"])
            )))
        }
    };
    copy_fields(part, &mut output, &["cache_control"]);
    Ok(output)
}

fn data_source(url: &str) -> Result<Value, BridgeError> {
    if let Some(data) = url.strip_prefix("data:") {
        let (media_type, data) = data
            .split_once(";base64,")
            .ok_or_else(|| invalid("expected a base64 data URL"))?;
        Ok(json!({"type":"base64","media_type":media_type,"data":data}))
    } else if url.starts_with("https://") || url.starts_with("http://") {
        Ok(json!({"type":"url","url":url}))
    } else {
        Err(invalid(
            "image or document must have a URL or inline base64 data",
        ))
    }
}

fn message_content(content: &Value) -> Result<Vec<Value>, BridgeError> {
    if let Some(text) = content.as_str() {
        return Ok(vec![json!({"type":"text","text":text})]);
    }
    array(content).iter().map(chat_part_to_messages).collect()
}

fn chat_to_messages(body: &Value, max_output_tokens: Option<u64>) -> Result<Value, BridgeError> {
    if body["n"].as_u64().is_some_and(|n| n > 1) {
        return Err(invalid("multiple choices require native Chat support"));
    }
    let mut output = base(body);
    let mut messages: Vec<Value> = Vec::new();
    let mut system = Vec::new();
    for message in array(&body["messages"]) {
        let role = string(&message["role"]);
        if role == "system" || role == "developer" {
            system.extend(message_content(&message["content"])?);
            continue;
        }
        let content = if role == "tool" {
            vec![
                json!({"type":"tool_result","tool_use_id":message["tool_call_id"],"content":message_content(&message["content"])?}),
            ]
        } else {
            let mut content = array(&message["thinking_blocks"]).to_vec();
            content.extend(message_content(&message["content"])?);
            if let Some(refusal) = message["refusal"].as_str() {
                content.push(json!({"type":"text","text":refusal}));
            }
            for tool in array(&message["tool_calls"]) {
                let arguments = string(&tool["function"]["arguments"]);
                let input = serde_json::from_str::<Value>(if arguments.trim().is_empty() {
                    "{}"
                } else {
                    arguments
                })
                .map_err(|_| invalid("tool call arguments must be valid JSON for Messages"))?;
                content.push(json!({"type":"tool_use","id":tool["id"],"name":tool["function"]["name"],"input":input}));
            }
            content
        };
        let role = if role == "tool" { "user" } else { role };
        if let Some(previous) = messages
            .last_mut()
            .filter(|previous| previous["role"] == role)
        {
            previous["content"].as_array_mut().unwrap().extend(content);
        } else {
            messages.push(json!({"role":role,"content":content}));
        }
    }
    if !system.is_empty() {
        output["system"] = json!(system);
    }
    output["messages"] = json!(messages);
    output["max_tokens"] = body
        .get("max_completion_tokens")
        .or_else(|| body.get("max_tokens"))
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or_else(|| json!(max_output_tokens.unwrap_or(4096)));
    if let Some(stop) = body.get("stop").filter(|v| !v.is_null()) {
        output["stop_sequences"] = if stop.is_string() {
            json!([stop])
        } else {
            stop.clone()
        };
    }
    apply_messages_thinking(body, &mut output)?;
    if let Some(format) = body.get("response_format").filter(|v| !v.is_null()) {
        match string(&format["type"]) {
            "text" => {}
            "json_schema" => {
                let schema = &format["json_schema"]["schema"];
                if !schema.is_object() {
                    return Err(invalid("response_format.json_schema.schema must be an object"));
                }
                // Thinking and structured output share output_config. Preserve
                // both controls rather than replacing either with the other.
                if output.get("output_config").is_none() {
                    output["output_config"] = json!({});
                }
                output["output_config"]["format"] = json!({"type":"json_schema","schema":schema});
            }
            _ => return Err(invalid("Messages adaptation requires response_format text or json_schema; use a JSON schema or a native OpenAI endpoint")),
        }
    }
    if let Some(tools) = body.get("tools") {
        output["tools"] = json!(array(tools)
            .iter()
            .map(|tool| {
                if tool["type"] != "function" {
                    return Err(invalid("only function tools can be adapted to Messages"));
                }
                let function = &tool["function"];
                let mut converted =
                    json!({"name":function["name"],"input_schema":function["parameters"]});
                copy_fields(function, &mut converted, &["description", "strict"]);
                copy_fields(tool, &mut converted, &["cache_control"]);
                Ok(converted)
            })
            .collect::<Result<Vec<_>, BridgeError>>()?);
    }
    if let Some(choice) = body.get("tool_choice").filter(|choice| !choice.is_null()) {
        output["tool_choice"] = match choice.as_str() {
            Some("required") => json!({"type":"any"}),
            Some(other) => json!({"type":other}),
            None => json!({"type":"tool","name":choice["function"]["name"]}),
        };
    }
    if let Some(parallel) = body["parallel_tool_calls"].as_bool() {
        if output.get("tool_choice").is_none() {
            output["tool_choice"] = json!({"type":"auto"});
        }
        output["tool_choice"]["disable_parallel_tool_use"] = json!(!parallel);
    }
    Ok(output)
}

fn apply_messages_thinking(body: &Value, output: &mut Value) -> Result<(), BridgeError> {
    let Some(effort) = body["reasoning_effort"].as_str() else {
        return Ok(());
    };
    // Unknown Messages-compatible models need not implement Claude thinking.
    // Restrict dialect selection to Claude families, preserving plain generation.
    let model = string(&body["model"])
        .to_ascii_lowercase()
        .replace('.', "-");
    let family = ["claude-opus-", "claude-sonnet-", "claude-haiku-"]
        .iter()
        .find_map(|prefix| model.strip_prefix(prefix));
    let version = family.and_then(|version| {
        let mut parts = version.split('-');
        Some((
            parts.next()?.parse::<u32>().ok()?,
            parts
                .next()
                .and_then(|part| part.parse::<u32>().ok())
                .filter(|minor| *minor < 100)
                .unwrap_or(0),
        ))
    });
    let legacy =
        model.starts_with("claude-3-7-sonnet") || version.is_some_and(|(major, _)| major == 4);
    let adaptive = version.is_some_and(|version| version >= (4, 6));
    if !legacy && !adaptive {
        return Ok(());
    }
    if effort == "none" {
        output["thinking"] = json!({"type":"disabled"});
    } else if adaptive {
        let effort = match effort {
            "minimal" | "low" => "low",
            "medium" => "medium",
            "max" | "xhigh" if model.starts_with("claude-opus-") => "max",
            _ => "high",
        };
        output["thinking"] = json!({"type":"adaptive"});
        output["output_config"] = json!({"effort":effort});
        output.as_object_mut().unwrap().remove("temperature");
        output.as_object_mut().unwrap().remove("top_p");
    } else {
        let limit = output["max_tokens"].as_u64().unwrap_or(4096);
        if limit <= 1024 {
            return Err(invalid(
                "Messages thinking requires max_tokens greater than 1024",
            ));
        }
        let budget = match effort {
            "minimal" | "low" => 1024,
            "medium" => 4096,
            "high" => 8192,
            _ => 16384,
        };
        output["thinking"] = json!({"type":"enabled","budget_tokens":budget.min(limit - 1)});
        output.as_object_mut().unwrap().remove("temperature");
        output.as_object_mut().unwrap().remove("top_p");
    }
    Ok(())
}

fn chat_content_to_responses(content: &Value, role: &str) -> Result<Value, BridgeError> {
    if let Some(text) = content.as_str() {
        return Ok(
            json!([{"type":if role == "assistant" {"output_text"} else {"input_text"},"text":text}]),
        );
    }
    Ok(json!(array(content).iter().map(|part| {
        Ok(match string(&part["type"]) {
            "text" => json!({"type":if role == "assistant" {"output_text"} else {"input_text"},"text":part["text"]}),
            "image_url" => json!({"type":"input_image","image_url":part["image_url"].as_str().map(Value::from).unwrap_or_else(||part["image_url"]["url"].clone()),"detail":part["image_url"]["detail"].as_str().unwrap_or("auto")}),
            "file" => json!({"type":"input_file","filename":part["file"]["filename"],"file_data":part["file"]["file_data"]}),
            _ => return Err(invalid(format!("cannot adapt Chat content {}", string(&part["type"])))),
        })
    }).collect::<Result<Vec<Value>, BridgeError>>()?))
}

fn chat_to_responses(body: &Value) -> Result<Value, BridgeError> {
    if body["n"].as_u64().is_some_and(|n| n > 1) {
        return Err(invalid("multiple choices require native Chat support"));
    }
    let mut output = base(body);
    let mut input = Vec::new();
    for message in array(&body["messages"]) {
        let role = string(&message["role"]);
        if role == "tool" {
            let content = &message["content"];
            let output = if content.is_string() {
                content.clone()
            } else {
                chat_content_to_responses(content, "user")?
            };
            input.push(json!({"type":"function_call_output","call_id":message["tool_call_id"],"output":output}));
            continue;
        }
        let mut content = chat_content_to_responses(&message["content"], role)?;
        if let Some(refusal) = message["refusal"].as_str() {
            content
                .as_array_mut()
                .unwrap()
                .push(json!({"type":"refusal","refusal":refusal}));
        }
        if !array(&content).is_empty() {
            input.push(json!({"type":"message","role":role,"content":content}));
        }
        for tool in array(&message["tool_calls"]) {
            input.push(json!({"type":"function_call","call_id":tool["id"],"name":tool["function"]["name"],"arguments":tool["function"]["arguments"]}));
        }
    }
    output["input"] = json!(input);
    output["store"] = json!(false);
    if let Some(limit) = body
        .get("max_completion_tokens")
        .or_else(|| body.get("max_tokens"))
    {
        output["max_output_tokens"] = limit.clone();
    }
    if let Some(effort) = body.get("reasoning_effort") {
        output["reasoning"] = json!({"effort":effort});
    }
    copy_fields(
        body,
        &mut output,
        &[
            "parallel_tool_calls",
            "service_tier",
            "prompt_cache_key",
            "prompt_cache_retention",
        ],
    );
    if let Some(format) = body.get("response_format") {
        output["text"] = json!({"format":if format["type"] == "json_schema" {
            let mut schema = format["json_schema"].clone();
            if !schema.is_object() { return Err(invalid("json_schema must be an object")); }
            schema["type"] = json!("json_schema"); schema
        } else { format.clone() }});
    }
    if let Some(tools) = body.get("tools") {
        output["tools"] = json!(array(tools)
            .iter()
            .map(|tool| {
                if tool["type"] != "function" {
                    return Err(invalid("only function tools can be adapted to Responses"));
                }
                let mut function = tool["function"].clone();
                if !function.is_object() {
                    return Err(invalid("function tool definition must be an object"));
                }
                function["type"] = json!("function");
                Ok(function)
            })
            .collect::<Result<Vec<_>, BridgeError>>()?);
    }
    if let Some(choice) = body.get("tool_choice") {
        output["tool_choice"] = if choice["type"] == "function" {
            json!({"type":"function","name":choice["function"]["name"]})
        } else {
            choice.clone()
        };
    }
    Ok(output)
}
