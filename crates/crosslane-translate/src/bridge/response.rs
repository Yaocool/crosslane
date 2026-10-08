use super::{array, invalid, json, string, BridgeError, ProviderProtocol, Value};

#[derive(Default, Debug)]
pub(super) struct Usage {
    pub reported: bool,
    pub input: u64,
    pub output: u64,
    pub cached: u64,
    pub cache_write: u64,
    pub reasoning: u64,
}

impl Usage {
    pub fn update(&mut self, usage: &Value, protocol: ProviderProtocol) {
        self.reported |= usage.is_object();
        let assign = |target: &mut u64, value: &Value| {
            if let Some(value) = value.as_u64() {
                *target = value;
            }
        };
        match protocol {
            ProviderProtocol::ClaudeMessages => {
                let uncached = self
                    .input
                    .saturating_sub(self.cached.saturating_add(self.cache_write));
                assign(&mut self.cached, &usage["cache_read_input_tokens"]);
                assign(&mut self.cache_write, &usage["cache_creation_input_tokens"]);
                self.input = usage["input_tokens"]
                    .as_u64()
                    .unwrap_or(uncached)
                    .saturating_add(self.cached)
                    .saturating_add(self.cache_write);
                assign(&mut self.output, &usage["output_tokens"]);
            }
            ProviderProtocol::OpenAiChat => {
                assign(&mut self.input, &usage["prompt_tokens"]);
                assign(&mut self.output, &usage["completion_tokens"]);
                assign(
                    &mut self.cached,
                    &usage["prompt_tokens_details"]["cached_tokens"],
                );
                assign(
                    &mut self.reasoning,
                    &usage["completion_tokens_details"]["reasoning_tokens"],
                );
            }
            ProviderProtocol::OpenAiResponses => {
                assign(&mut self.input, &usage["input_tokens"]);
                assign(&mut self.output, &usage["output_tokens"]);
                assign(
                    &mut self.cached,
                    &usage["input_tokens_details"]["cached_tokens"],
                );
                assign(
                    &mut self.reasoning,
                    &usage["output_tokens_details"]["reasoning_tokens"],
                );
            }
        }
    }

    pub fn value(&self, protocol: ProviderProtocol) -> Value {
        match protocol {
            ProviderProtocol::ClaudeMessages => json!({
                "input_tokens":self.input.saturating_sub(self.cached.saturating_add(self.cache_write)),
                "output_tokens":self.output,"cache_read_input_tokens":self.cached,
                "cache_creation_input_tokens":self.cache_write
            }),
            ProviderProtocol::OpenAiChat => json!({
                "prompt_tokens":self.input,"completion_tokens":self.output,"total_tokens":self.input.saturating_add(self.output),
                "prompt_tokens_details":{"cached_tokens":self.cached},
                "completion_tokens_details":{"reasoning_tokens":self.reasoning}
            }),
            ProviderProtocol::OpenAiResponses => json!({
                "input_tokens":self.input,"output_tokens":self.output,"total_tokens":self.input.saturating_add(self.output),
                "input_tokens_details":{"cached_tokens":self.cached},
                "output_tokens_details":{"reasoning_tokens":self.reasoning}
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum BlockKind {
    Text,
    Refusal,
    Reasoning,
    Tool,
}

#[derive(Debug)]
pub(super) struct Block {
    pub key: String,
    pub kind: BlockKind,
    pub id: String,
    pub call_id: String,
    pub name: String,
    pub text: String,
    pub signature: String,
    pub emitted: bool,
}

impl Block {
    pub fn new(key: String, kind: BlockKind) -> Self {
        let prefix = match kind {
            BlockKind::Text | BlockKind::Refusal => "msg",
            BlockKind::Reasoning => "rs",
            BlockKind::Tool => "fc",
        };
        Self {
            key,
            kind,
            id: format!("{prefix}_{}", uuid::Uuid::new_v4().simple()),
            call_id: String::new(),
            name: String::new(),
            text: String::new(),
            signature: String::new(),
            emitted: false,
        }
    }

    pub fn response_item(&self, status: &str) -> Value {
        let done = status != "in_progress";
        match self.kind {
            BlockKind::Text => {
                json!({"id":self.id,"type":"message","role":"assistant","status":status,
                "content":if done { json!([{"type":"output_text","text":self.text,"annotations":[]}]) } else { json!([]) }})
            }
            BlockKind::Refusal => {
                json!({"id":self.id,"type":"message","role":"assistant","status":status,
                "content":if done { json!([{"type":"refusal","refusal":self.text}]) } else { json!([]) }})
            }
            BlockKind::Reasoning => json!({"id":self.id,"type":"reasoning","status":status,
                "summary":if done { json!([{"type":"summary_text","text":self.text}]) } else { json!([]) }}),
            BlockKind::Tool => {
                json!({"id":self.id,"type":"function_call","status":status,"call_id":self.call_id,"name":self.name,
                "arguments":if done { self.arguments() } else { "" }})
            }
        }
    }

    pub fn arguments(&self) -> &str {
        if self.text.is_empty() {
            "{}"
        } else {
            &self.text
        }
    }

    pub fn claude_block(&self, done: bool) -> Result<Value, BridgeError> {
        Ok(match self.kind {
            BlockKind::Text | BlockKind::Refusal => {
                json!({"type":"text","text":if done {&self.text} else {""}})
            }
            BlockKind::Reasoning => {
                let mut value =
                    json!({"type":"thinking","thinking":if done {&self.text} else {""}});
                if done && !self.signature.is_empty() {
                    value["signature"] = json!(self.signature);
                }
                value
            }
            BlockKind::Tool => json!({"type":"tool_use","id":self.call_id,"name":self.name,
                "input":if done { serde_json::from_str::<Value>(self.arguments()).map_err(|_| invalid("upstream returned invalid tool arguments"))? } else { json!({}) }}),
        })
    }
}

#[derive(Debug)]
pub(super) struct ResponseState {
    pub id: String,
    pub model: String,
    pub created: u64,
    pub blocks: Vec<Block>,
    pub usage: Usage,
    pub finish: String,
    pub billing: Option<Value>,
}

impl ResponseState {
    pub fn new(model: &str) -> Self {
        Self {
            id: format!("resp_{}", uuid::Uuid::new_v4().simple()),
            model: model.into(),
            created: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            blocks: Vec::new(),
            usage: Usage::default(),
            finish: "stop".into(),
            billing: None,
        }
    }

    pub fn block(&mut self, key: String, kind: BlockKind) -> usize {
        if let Some(index) = self.blocks.iter().position(|block| block.key == key) {
            return index;
        }
        self.blocks.push(Block::new(key, kind));
        self.blocks.len() - 1
    }

    pub fn metadata(&mut self, value: &Value, protocol: ProviderProtocol) {
        if let Some(id) = value["id"].as_str() {
            self.id = id.to_owned();
        }
        if let Some(model) = value["model"].as_str() {
            self.model = model.to_owned();
        }
        if let Some(created) = value["created"]
            .as_u64()
            .or_else(|| value["created_at"].as_u64())
        {
            self.created = created;
        }
        self.usage.update(&value["usage"], protocol);
        self.capture_billing(value);
    }

    pub fn capture_billing(&mut self, value: &Value) {
        if let Some(billing) = value
            .get("copilot_usage")
            .or_else(|| value.pointer("/usage/copilot_usage"))
            .or_else(|| value.pointer("/response/copilot_usage"))
            .filter(|v| !v.is_null())
        {
            self.billing = Some(billing.clone());
        }
    }

    pub fn claude_stop(&self) -> &str {
        match self.finish.as_str() {
            "length" | "max_tokens" | "max_output_tokens" => "max_tokens",
            "tool_calls" | "tool_use" => "tool_use",
            "stop_sequence" => "stop_sequence",
            _ if self.blocks.iter().any(|b| b.kind == BlockKind::Tool) => "tool_use",
            _ => "end_turn",
        }
    }

    pub fn chat_stop(&self) -> &str {
        match self.finish.as_str() {
            "length" | "max_tokens" | "max_output_tokens" => "length",
            "content_filter" | "refusal" => "content_filter",
            _ if self.claude_stop() == "tool_use" => "tool_calls",
            _ => "stop",
        }
    }

    pub fn incomplete(&self) -> Option<&str> {
        match self.chat_stop() {
            "length" => Some("max_output_tokens"),
            "content_filter" => Some("content_filter"),
            _ => None,
        }
    }

    pub fn output_status(&self) -> &str {
        if self.incomplete().is_some() {
            "incomplete"
        } else {
            "completed"
        }
    }

    pub fn value(&self, protocol: ProviderProtocol, done: bool) -> Result<Value, BridgeError> {
        let usage = self.usage.value(protocol);
        let mut value = match protocol {
            ProviderProtocol::ClaudeMessages => json!({
                "id":self.id,"type":"message","role":"assistant","model":self.model,
                "content":if done { self.blocks.iter().map(|b| b.claude_block(true)).collect::<Result<Vec<_>,_>>()? } else { vec![] },
                "stop_reason":if done { json!(self.claude_stop()) } else { Value::Null },
                "stop_sequence":null,"usage":usage
            }),
            ProviderProtocol::OpenAiChat => {
                let text = self
                    .blocks
                    .iter()
                    .filter(|b| b.kind == BlockKind::Text)
                    .map(|b| b.text.as_str())
                    .collect::<String>();
                let thinking = self
                    .blocks
                    .iter()
                    .filter(|b| b.kind == BlockKind::Reasoning)
                    .map(|b| b.text.as_str())
                    .collect::<String>();
                let mut message = json!({"role":"assistant","content":text});
                if !thinking.is_empty() {
                    message["reasoning_content"] = json!(thinking);
                }
                let refusal = self
                    .blocks
                    .iter()
                    .filter(|b| b.kind == BlockKind::Refusal)
                    .map(|b| b.text.as_str())
                    .collect::<String>();
                if !refusal.is_empty() {
                    message["refusal"] = json!(refusal);
                }
                let tools = self.blocks.iter().filter(|b| b.kind == BlockKind::Tool).map(|b| json!({
                    "id":b.call_id,"type":"function","function":{"name":b.name,"arguments":b.arguments()}
                })).collect::<Vec<_>>();
                if !tools.is_empty() {
                    message["tool_calls"] = json!(tools);
                }
                json!({"id":self.id,"object":"chat.completion","created":self.created,"model":self.model,
                    "choices":[{"index":0,"message":message,"finish_reason":self.chat_stop()}],"usage":usage})
            }
            ProviderProtocol::OpenAiResponses => json!({
                "id":self.id,"object":"response","created_at":self.created,"model":self.model,
                "status":if !done {"in_progress"} else {self.output_status()},
                "error":null,"incomplete_details":self.incomplete().map(|reason| json!({"reason":reason})),
                "output":if done {self.blocks.iter().map(|b|b.response_item(self.output_status())).collect::<Vec<_>>()} else {vec![]},
                "parallel_tool_calls":true,"store":false,"usage":if done {usage} else {Value::Null}
            }),
        };
        if let Some(billing) = &self.billing {
            value["copilot_usage"] = billing.clone();
        }
        if !self.usage.reported {
            // Messages clients require numeric usage in the start envelope,
            // even when a Chat upstream reports usage only at the very end.
            // Distinguish that protocol scaffold from real reported zeroes.
            value["crosslane_usage_source"] = json!("unreported");
        }
        Ok(value)
    }
}

pub fn translate_response(
    body: &Value,
    from: ProviderProtocol,
    to: ProviderProtocol,
) -> Result<Value, BridgeError> {
    if from == to {
        return Ok(body.clone());
    }
    if body.get("error").is_some_and(|v| !v.is_null()) {
        return Ok(translate_error(body, to));
    }
    let mut state = ResponseState::new(string(&body["model"]));
    state.metadata(body, from);
    match from {
        ProviderProtocol::ClaudeMessages => {
            if !body["content"].is_array() {
                return Err(invalid("upstream Messages response has no content array"));
            }
            for (index, part) in array(&body["content"]).iter().enumerate() {
                let kind = match string(&part["type"]) {
                    "text" => BlockKind::Text,
                    "thinking" => BlockKind::Reasoning,
                    "tool_use" => BlockKind::Tool,
                    _ => continue,
                };
                let i = state.block(index.to_string(), kind.clone());
                let block = &mut state.blocks[i];
                match kind {
                    BlockKind::Text | BlockKind::Refusal => {
                        block.text = string(&part["text"]).into()
                    }
                    BlockKind::Reasoning => {
                        block.text = string(&part["thinking"]).into();
                        block.signature = string(&part["signature"]).into();
                    }
                    BlockKind::Tool => {
                        block.call_id = string(&part["id"]).into();
                        block.name = string(&part["name"]).into();
                        block.text = part["input"].to_string();
                    }
                }
            }
            state.finish = string(&body["stop_reason"]).into();
        }
        ProviderProtocol::OpenAiChat => {
            let choice = body["choices"]
                .get(0)
                .ok_or_else(|| invalid("upstream Chat response has no choices"))?;
            let message = &choice["message"];
            for (field, kind) in [
                ("reasoning_content", BlockKind::Reasoning),
                ("content", BlockKind::Text),
                ("refusal", BlockKind::Refusal),
            ] {
                if let Some(text) = message[field].as_str().filter(|v| !v.is_empty()) {
                    let i = state.block(field.into(), kind);
                    state.blocks[i].text = text.into();
                }
            }
            for (index, tool) in array(&message["tool_calls"]).iter().enumerate() {
                let i = state.block(format!("tool:{index}"), BlockKind::Tool);
                let block = &mut state.blocks[i];
                block.call_id = string(&tool["id"]).into();
                block.name = string(&tool["function"]["name"]).into();
                block.text = string(&tool["function"]["arguments"]).into();
            }
            state.finish = string(&choice["finish_reason"]).into();
        }
        ProviderProtocol::OpenAiResponses => {
            if !body["output"].is_array() {
                return Err(invalid("upstream Responses response has no output array"));
            }
            for (index, item) in array(&body["output"]).iter().enumerate() {
                match string(&item["type"]) {
                    "message" => {
                        for (part_index, part) in array(&item["content"]).iter().enumerate() {
                            let kind = if part["type"] == "refusal" {
                                BlockKind::Refusal
                            } else {
                                BlockKind::Text
                            };
                            let i = state.block(format!("{index}:{part_index}"), kind);
                            state.blocks[i].text = part["text"]
                                .as_str()
                                .or_else(|| part["refusal"].as_str())
                                .unwrap_or_default()
                                .into();
                        }
                    }
                    "reasoning" => {
                        let i = state.block(index.to_string(), BlockKind::Reasoning);
                        state.blocks[i].text = array(&item["summary"])
                            .iter()
                            .filter_map(|v| v["text"].as_str())
                            .collect();
                    }
                    "function_call" => {
                        let i = state.block(index.to_string(), BlockKind::Tool);
                        let block = &mut state.blocks[i];
                        block.call_id = string(&item["call_id"]).into();
                        block.name = string(&item["name"]).into();
                        block.text = string(&item["arguments"]).into();
                    }
                    _ => {}
                }
            }
            if body["status"] == "incomplete" {
                state.finish = string(&body["incomplete_details"]["reason"]).into();
            }
            if body["status"] == "failed" {
                return Err(invalid("upstream response failed"));
            }
        }
    }
    state.value(to, true)
}

pub fn translate_error(body: &Value, to: ProviderProtocol) -> Value {
    let error = body.get("error").filter(|v| v.is_object()).unwrap_or(body);
    let message = error["message"]
        .as_str()
        .or_else(|| body["message"].as_str())
        .unwrap_or("upstream request failed");
    let kind = error["type"].as_str().unwrap_or("api_error");
    match to {
        ProviderProtocol::ClaudeMessages => {
            json!({"type":"error","error":{"type":kind,"message":message}})
        }
        _ => {
            json!({"error":{"type":kind,"message":message,"code":error["code"],"param":error["param"]}})
        }
    }
}
