use serde_json::{json, Value};

use crate::{
    KiroAssistantMessage, KiroConversationState, KiroCurrentMessage, KiroHistoryMessage,
    KiroPayload, KiroText, KiroToolResult, KiroToolUse, KiroUserInputMessage, OpenAiRequest,
};

use super::common::{
    content_cache_point, content_text, context, enhance_system, extract_documents,
    extract_openai_images, inference, kiro_cache_point, kiro_tool_named, merged_cache_point,
    needs_chunked_write_hint, ToolNameRegistry,
};
use super::{TranslationOptions, SYSTEM_PROMPT_ACKNOWLEDGEMENT};

pub fn openai_to_kiro(request: &OpenAiRequest, options: &TranslationOptions) -> KiroPayload {
    super::common::log_ignored_controls(
        "openai",
        &[
            ("response_format", request.response_format.is_some()),
            (
                "tools.strict",
                request.tools.iter().any(|tool| {
                    tool.body
                        .get(&tool.r#type)
                        .and_then(|definition| definition.get("strict"))
                        == Some(&Value::Bool(true))
                }),
            ),
        ],
    );
    let tool_names = ToolNameRegistry::new(request.tools.iter().filter_map(|tool| {
        tool.body
            .get(&tool.r#type)
            .and_then(|definition| definition.get("name"))
            .and_then(Value::as_str)
    }));
    let selected = selected_tools(request);
    let mut documentation = Vec::new();
    let tools = selected
        .iter()
        .flat_map(|tool| {
            let mut translated = Vec::new();
            let cache_point = options
                .enable_prompt_cache
                .then(|| kiro_cache_point(tool.body.get("cache_control")))
                .flatten();
            let definition = tool.body.get(&tool.r#type)?;
            let name = definition.get("name")?.as_str()?;
            let mut description = definition
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let schema = if tool.r#type == "custom" {
                if let Some(grammar) = definition
                    .get("format")
                    .filter(|format| format.get("type").and_then(Value::as_str) == Some("grammar"))
                    .and_then(|format| format.get("grammar"))
                {
                    let syntax = grammar
                        .get("syntax")
                        .and_then(Value::as_str)
                        .unwrap_or("grammar");
                    if let Some(value) = grammar.get("definition").and_then(Value::as_str) {
                        description = join(
                            &description,
                            &format!("Input format ({syntax} grammar):\n{value}"),
                        );
                    }
                }
                json!({
                    "type":"object",
                    "properties":{"input":{
                        "type":"string",
                        "description":"The complete raw tool input text. Do not wrap or escape it again."
                    }},
                    "required":["input"],
                    "additionalProperties":false
                })
            } else {
                definition
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({"type":"object","properties":{}}))
            };
            let (translated_tool, docs) = kiro_tool_named(
                name,
                &tool_names.kiro_name(name),
                &description,
                &schema,
            );
            documentation.extend(docs);
            translated.push(translated_tool);
            if let Some(cache_point) = cache_point {
                translated.push(crate::KiroTool::CachePoint { cache_point });
            }
            Some(translated)
        })
        .flatten()
        .collect::<Vec<_>>();

    let mut system = request
        .messages
        .iter()
        .filter(|message| matches!(message.role.as_str(), "system" | "developer"))
        .filter_map(|message| message.content.as_ref())
        .map(content_text)
        .collect::<Vec<_>>()
        .join("\n");
    let system_cache_point = options
        .enable_prompt_cache
        .then(|| {
            request
                .messages
                .iter()
                .filter(|message| matches!(message.role.as_str(), "system" | "developer"))
                .fold(None, |current, message| {
                    merged_cache_point(current, openai_message_cache_point(message))
                })
        })
        .flatten();
    if options.enhance_system_prompt {
        let chunked_write_hint = tools.iter().any(|tool| {
            tool.specification()
                .is_some_and(|tool| needs_chunked_write_hint(&tool.name))
        });
        system = enhance_system(system, chunked_write_hint);
    }
    if !documentation.is_empty() {
        system = join(&system, &documentation.join("\n\n"));
    }
    system = join(&system, &choice_directive(request));

    let non_system = request
        .messages
        .iter()
        .filter(|message| !matches!(message.role.as_str(), "system" | "developer"))
        .collect::<Vec<_>>();
    let mut history = Vec::new();
    let mut current_text = String::new();
    let mut current_results = Vec::new();
    let mut current_images = Vec::new();
    let mut current_documents = Vec::new();
    let mut current_cache_point = None;
    let mut current_result_cache_point = None;

    for (index, message) in non_system.iter().enumerate() {
        let last = index + 1 == non_system.len();
        match message.role.as_str() {
            "user" => {
                let text = message
                    .content
                    .as_ref()
                    .map(content_text)
                    .unwrap_or_default();
                let images = message
                    .content
                    .as_ref()
                    .map(extract_openai_images)
                    .unwrap_or_default();
                let documents = message
                    .content
                    .as_ref()
                    .map(extract_documents)
                    .unwrap_or_default();
                current_cache_point = openai_message_cache_point(message);
                if last {
                    current_text = text;
                    current_images = images;
                    current_documents = documents;
                } else {
                    push_user(
                        &mut history,
                        make_user(
                            text,
                            images,
                            documents,
                            Vec::new(),
                            current_cache_point.take(),
                            options,
                        ),
                    );
                }
            }
            "assistant" => push_assistant(
                &mut history,
                assistant_message(message, &tool_names, options.enable_prompt_cache),
            ),
            "tool" => {
                // Responses tool outputs may contain images (for example a
                // Codex view_image result). Keep them with the tool-result turn.
                if let Some(content) = &message.content {
                    current_images.extend(extract_openai_images(content));
                    current_documents.extend(extract_documents(content));
                }
                current_result_cache_point = merged_cache_point(
                    current_result_cache_point,
                    openai_message_cache_point(message),
                );
                if let Some(id) = &message.tool_call_id {
                    let text = message
                        .content
                        .as_ref()
                        .map(content_text)
                        .unwrap_or_default();
                    current_results.push(KiroToolResult {
                        content: vec![KiroText { text }],
                        status: "success".into(),
                        tool_use_id: id.clone(),
                    });
                }
                let next_is_tool = non_system
                    .get(index + 1)
                    .is_some_and(|next| next.role == "tool");
                if !last && !next_is_tool {
                    push_user(
                        &mut history,
                        make_user(
                            "Tool results provided.".into(),
                            std::mem::take(&mut current_images),
                            std::mem::take(&mut current_documents),
                            current_results.split_off(0),
                            current_result_cache_point.take(),
                            options,
                        ),
                    );
                }
            }
            _ => {}
        }
    }
    current_cache_point = merged_cache_point(current_cache_point, current_result_cache_point);
    if current_text.trim().is_empty() {
        current_text = if current_results.is_empty() {
            "Continue.".into()
        } else {
            "Tool results provided.".into()
        };
    }
    let current = KiroUserInputMessage {
        content: current_text,
        model_id: options.model_id.clone(),
        origin: options.origin.clone(),
        images: current_images,
        documents: current_documents,
        cache_point: current_cache_point,
        client_cache_config: None,
        user_input_message_context: context(tools.clone(), current_results),
    };
    let protected_history_messages = if system.trim().is_empty() {
        0
    } else {
        history.splice(
            0..0,
            [
                KiroHistoryMessage {
                    user_input_message: Some(make_user(
                        system.trim().to_owned(),
                        Vec::new(),
                        Vec::new(),
                        Vec::new(),
                        system_cache_point,
                        options,
                    )),
                    assistant_response_message: None,
                },
                KiroHistoryMessage {
                    user_input_message: None,
                    assistant_response_message: Some(KiroAssistantMessage {
                        content: SYSTEM_PROMPT_ACKNOWLEDGEMENT.into(),
                        cache_point: None,
                        tool_uses: Vec::new(),
                    }),
                },
            ],
        );
        2
    };

    let mut payload = KiroPayload {
        conversation_state: KiroConversationState {
            agent_continuation_id: Some(random_id()),
            agent_task_type: Some("vibe".into()),
            chat_trigger_type: "MANUAL".into(),
            conversation_id: options.conversation_id.clone().unwrap_or_else(random_id),
            current_message: KiroCurrentMessage {
                user_input_message: current,
            },
            history,
        },
        profile_arn: options.profile_arn.clone(),
        inference_config: inference(
            request.output_token_limit(),
            !tools.is_empty(),
            request.temperature,
            request.top_p,
        ),
        additional_model_request_fields: None,
        model_request_intent: Some(crate::ModelRequestIntent {
            requested_model: request.model.clone(),
            thinking: if request.reasoning_effort.as_deref() == Some("none") {
                Some(crate::ThinkingConfig {
                    r#type: "disabled".into(),
                    budget_tokens: None,
                    display: None,
                })
            } else {
                request.thinking.clone()
            },
            effort: request
                .reasoning_effort
                .clone()
                .filter(|effort| effort != "none"),
            automatic_history_truncation: false,
        }),
        protected_history_messages,
    };
    crate::model::apply_adaptive_thinking(
        &mut payload,
        options.additional_model_request_fields_schema.as_ref(),
        true,
    );
    payload
}

fn selected_tools(request: &OpenAiRequest) -> Vec<&crate::OpenAiTool> {
    let Some(choice) = request.tool_choice.as_ref() else {
        return request.tools.iter().collect();
    };
    if choice == "none" {
        return Vec::new();
    }
    if choice.get("type").and_then(Value::as_str) == Some("allowed_tools") {
        let allowed = choice
            .pointer("/allowed_tools/tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|reference| {
                let kind = reference.get("type")?.as_str()?;
                let name = reference.get(kind)?.get("name")?.as_str()?;
                Some((kind, name))
            })
            .collect::<Vec<_>>();
        return request
            .tools
            .iter()
            .filter(|tool| {
                tool.body
                    .get(&tool.r#type)
                    .and_then(|definition| definition.get("name"))
                    .and_then(Value::as_str)
                    .is_some_and(|name| {
                        allowed.iter().any(|(kind, allowed_name)| {
                            *kind == tool.r#type && *allowed_name == name
                        })
                    })
            })
            .collect();
    }
    let selected_name = choice
        .get("function")
        .or_else(|| choice.get("custom"))
        .and_then(|body| body.get("name"))
        .and_then(Value::as_str);
    match selected_name {
        Some(name) => request
            .tools
            .iter()
            .filter(|tool| {
                tool.body
                    .get(&tool.r#type)
                    .and_then(|body| body.get("name"))
                    .and_then(Value::as_str)
                    == Some(name)
            })
            .collect(),
        None => request.tools.iter().collect(),
    }
}

fn assistant_message(
    message: &crate::OpenAiMessage,
    tool_names: &ToolNameRegistry,
    prompt_cache_enabled: bool,
) -> KiroAssistantMessage {
    let mut content = message
        .content
        .as_ref()
        .map(content_text)
        .unwrap_or_default();
    if let Some(refusal) = &message.refusal {
        content = join(&content, refusal);
    }
    let tool_uses = message
        .tool_calls
        .iter()
        .filter_map(|call| {
            let id = call.get("id")?.as_str()?.to_string();
            let (name, input) = if let Some(function) = call.get("function") {
                let arguments = function.get("arguments")?.as_str().unwrap_or("{}");
                (
                    function.get("name")?.as_str()?,
                    serde_json::from_str(arguments).unwrap_or_else(|_| json!({"raw":arguments})),
                )
            } else {
                let custom = call.get("custom")?;
                (
                    custom.get("name")?.as_str()?,
                    json!({"input":custom.get("input").and_then(Value::as_str).unwrap_or_default()}),
                )
            };
            Some(KiroToolUse {
                tool_use_id: id,
                name: tool_names.kiro_name(name),
                input,
            })
        })
        .collect::<Vec<_>>();
    // Tool uses already represent the assistant turn. Invented prose here
    // becomes model-visible history and can be echoed in later responses.
    if content.trim().is_empty() && tool_uses.is_empty() {
        content = "I understand.".into();
    }
    KiroAssistantMessage {
        content,
        cache_point: prompt_cache_enabled
            .then(|| openai_message_cache_point(message))
            .flatten(),
        tool_uses,
    }
}

fn make_user(
    text: String,
    images: Vec<crate::KiroImage>,
    documents: Vec<crate::KiroDocument>,
    results: Vec<KiroToolResult>,
    cache_point: Option<crate::KiroCachePoint>,
    options: &TranslationOptions,
) -> KiroUserInputMessage {
    KiroUserInputMessage {
        content: if text.trim().is_empty() {
            "Continue".into()
        } else {
            text
        },
        model_id: options.model_id.clone(),
        origin: options.origin.clone(),
        images,
        documents,
        cache_point,
        client_cache_config: None,
        user_input_message_context: context(Vec::new(), results),
    }
}

fn openai_message_cache_point(message: &crate::OpenAiMessage) -> Option<crate::KiroCachePoint> {
    merged_cache_point(
        kiro_cache_point(message.cache_control.as_ref()),
        message.content.as_ref().and_then(content_cache_point),
    )
}

fn push_user(history: &mut Vec<KiroHistoryMessage>, message: KiroUserInputMessage) {
    history.push(KiroHistoryMessage {
        user_input_message: Some(message),
        assistant_response_message: None,
    });
}

fn push_assistant(history: &mut Vec<KiroHistoryMessage>, message: KiroAssistantMessage) {
    history.push(KiroHistoryMessage {
        user_input_message: None,
        assistant_response_message: Some(message),
    });
}

fn choice_directive(request: &OpenAiRequest) -> String {
    let required = request.tool_choice.as_ref().is_some_and(|choice| {
        choice == "required"
            || choice.get("function").is_some()
            || choice.get("custom").is_some()
            || choice
                .pointer("/allowed_tools/mode")
                .and_then(Value::as_str)
                == Some("required")
    });
    let mut parts = Vec::new();
    if required {
        parts.push("You must call at least one of the provided tools.");
    }
    if !request.parallel_tool_calls && !request.tools.is_empty() {
        parts.push(if required {
            "Make exactly one tool call."
        } else {
            "Make at most one tool call."
        });
    }
    parts.join("\n")
}

fn join(left: &str, right: &str) -> String {
    match (left.trim().is_empty(), right.trim().is_empty()) {
        (true, _) => right.trim().into(),
        (_, true) => left.trim().into(),
        _ => format!("{}\n\n{}", left.trim(), right.trim()),
    }
}

fn random_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_messages_use_a_protected_kiro_history_pair() {
        let request: OpenAiRequest = serde_json::from_value(json!({
            "model":"claude-sonnet",
            "messages":[
                {"role":"system","content":"Follow the project rules."},
                {"role":"user","content":"hello"}
            ],
            "max_tokens":128
        }))
        .expect("request");
        let payload = openai_to_kiro(
            &request,
            &TranslationOptions::new("claude-sonnet", "AI_EDITOR"),
        );
        assert_eq!(payload.protected_history_len(), 2);
        let protected_system = payload.conversation_state.history[0]
            .user_input_message
            .as_ref()
            .expect("protected system message");
        assert!(protected_system
            .content
            .starts_with("Follow the project rules."));
        assert!(protected_system
            .content
            .contains("Execute the user's request directly"));
        assert_eq!(
            payload.conversation_state.history[1]
                .assistant_response_message
                .as_ref()
                .map(|message| message.content.as_str()),
            Some(SYSTEM_PROMPT_ACKNOWLEDGEMENT)
        );
        assert_eq!(
            payload
                .conversation_state
                .current_message
                .user_input_message
                .content,
            "hello"
        );
    }

    #[test]
    fn data_url_images_are_forwarded_for_history_and_current_turn() {
        let request: OpenAiRequest = serde_json::from_value(json!({
            "model":"claude-sonnet",
            "messages":[
                {"role":"user","content":[
                    {"type":"text","text":"first"},
                    {"type":"image_url","image_url":{"url":"data:image/png;base64,AQID"}}
                ]},
                {"role":"assistant","content":"ok"},
                {"role":"user","content":[
                    {"type":"text","text":"second"},
                    {"type":"image_url","image_url":{"url":"data:image/jpeg;base64,BAUG"}}
                ]}
            ],
            "max_tokens":128
        }))
        .expect("request");
        let payload = openai_to_kiro(
            &request,
            &TranslationOptions::new("claude-sonnet", "AI_EDITOR"),
        );
        let history_image = payload
            .conversation_state
            .history
            .iter()
            .skip(payload.protected_history_len())
            .filter_map(|message| message.user_input_message.as_ref())
            .flat_map(|message| message.images.iter())
            .next()
            .expect("history image");
        assert_eq!(history_image.format, "png");
        let current = &payload
            .conversation_state
            .current_message
            .user_input_message
            .images[0];
        assert_eq!(current.format, "jpeg");
        assert_eq!(current.source.bytes, "BAUG");
    }

    #[test]
    fn allowed_tools_only_forward_the_declared_subset() {
        let request: OpenAiRequest = serde_json::from_value(json!({
            "model":"claude-sonnet",
            "messages":[{"role":"user","content":"use a tool"}],
            "tools":[
                {"type":"function","function":{"name":"one","parameters":{"type":"object"}}},
                {"type":"function","function":{"name":"two","parameters":{"type":"object"}}}
            ],
            "tool_choice":{"type":"allowed_tools","allowed_tools":{"mode":"required","tools":[
                {"type":"function","function":{"name":"two"}}
            ]}}
        }))
        .expect("request");
        let payload = openai_to_kiro(
            &request,
            &TranslationOptions::new("claude-sonnet", "AI_EDITOR"),
        );
        let tools = &payload
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .expect("context")
            .tools;
        assert_eq!(tools.len(), 1);
        assert_eq!(
            tools[0].specification().expect("tool specification").name,
            "two"
        );
    }

    #[test]
    fn colliding_tool_names_use_the_same_registry_for_definitions_and_history() {
        let request: OpenAiRequest = serde_json::from_value(json!({
            "model":"claude-sonnet",
            "messages":[
                {"role":"user","content":"look it up"},
                {"role":"assistant","tool_calls":[{
                    "id":"call_1","type":"function",
                    "function":{"name":"mcp.a/read","arguments":"{}"}
                }]},
                {"role":"tool","tool_call_id":"call_1","content":"done"}
            ],
            "tools":[
                {"type":"function","function":{"name":"mcp.a/read","parameters":{"type":"object"}}},
                {"type":"function","function":{"name":"mcp_a/read","parameters":{"type":"object"}}}
            ]
        }))
        .expect("request");

        let payload = openai_to_kiro(
            &request,
            &TranslationOptions::new("claude-sonnet", "AI_EDITOR"),
        );
        let mapped = ToolNameRegistry::new(["mcp.a/read", "mcp_a/read"]).kiro_name("mcp.a/read");
        let context = payload
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .as_ref()
            .expect("context");
        assert!(context
            .tools
            .iter()
            .any(|tool| tool.specification().is_some_and(|tool| tool.name == mapped)));
        let history_name = &payload
            .conversation_state
            .history
            .iter()
            .skip(payload.protected_history_len())
            .filter_map(|message| message.assistant_response_message.as_ref())
            .flat_map(|message| message.tool_uses.iter())
            .next()
            .expect("assistant tool use")
            .name;
        assert_eq!(history_name, &mapped);
    }
}
