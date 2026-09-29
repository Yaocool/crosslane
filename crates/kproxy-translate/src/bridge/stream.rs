use super::response::{BlockKind, ResponseState};
use super::{array, invalid, json, string, translate_error, BridgeError, ProviderProtocol, Value};

const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_STREAM_BYTES: usize = 64 * 1024 * 1024;

/// Incremental SSE translation, including split UTF-8, CRLF, and multiline data.
/// Completion is emitted only after an upstream terminal event. An interrupted
/// stream cannot be turned into an apparently successful response.
pub struct StreamBridge {
    from: ProviderProtocol,
    to: ProviderProtocol,
    state: ResponseState,
    pending: Vec<u8>,
    data: Vec<String>,
    event: String,
    event_bytes: usize,
    started: bool,
    finished: bool,
    sequence: u64,
    received_bytes: usize,
}

impl StreamBridge {
    pub fn new(from: ProviderProtocol, to: ProviderProtocol, model: &str) -> Self {
        Self {
            from,
            to,
            state: ResponseState::new(model),
            pending: Vec::new(),
            data: Vec::new(),
            event: String::new(),
            event_bytes: 0,
            started: false,
            finished: false,
            sequence: 0,
            received_bytes: 0,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>, BridgeError> {
        self.received_bytes = self.received_bytes.saturating_add(bytes.len());
        if self.received_bytes > MAX_STREAM_BYTES {
            return Err(invalid("upstream stream exceeds the adapter limit"));
        }
        let mut output = Vec::new();
        for byte in bytes {
            if *byte == b'\n' {
                let mut line = std::mem::take(&mut self.pending);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                self.line(&line, &mut output)?;
            } else {
                self.pending.push(*byte);
                if self.pending.len() + self.event_bytes > MAX_EVENT_BYTES {
                    return Err(invalid("upstream SSE event exceeds the adapter limit"));
                }
            }
        }
        Ok(output)
    }

    pub fn finish(&mut self) -> Result<Vec<Vec<u8>>, BridgeError> {
        let mut output = Vec::new();
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.line(&line, &mut output)?;
        }
        self.flush(&mut output)?;
        if !self.finished {
            return Err(invalid("upstream stream ended before a terminal event"));
        }
        Ok(output)
    }

    /// Emit an error in the caller's protocol, without a success terminator.
    pub fn error(&mut self, message: &str) -> Vec<u8> {
        self.finished = true;
        let error = translate_error(
            &json!({"error":{"type":"api_error","message":message}}),
            self.to,
        );
        self.frame(error, Some("error"))
    }

    fn line(&mut self, line: &[u8], output: &mut Vec<Vec<u8>>) -> Result<(), BridgeError> {
        if line.is_empty() {
            return self.flush(output);
        }
        let line = std::str::from_utf8(line)
            .map_err(|_| invalid("upstream SSE contains invalid UTF-8"))?;
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.strip_prefix(' ').unwrap_or(data);
            self.event_bytes += data.len();
            if self.event_bytes > MAX_EVENT_BYTES {
                return Err(invalid("upstream SSE event exceeds the adapter limit"));
            }
            self.data.push(data.into());
        } else if let Some(event) = line.strip_prefix("event:") {
            self.event = event.trim().into();
        }
        Ok(())
    }

    fn flush(&mut self, output: &mut Vec<Vec<u8>>) -> Result<(), BridgeError> {
        let data = std::mem::take(&mut self.data).join("\n");
        let event = std::mem::take(&mut self.event);
        self.event_bytes = 0;
        if data.is_empty() {
            return Ok(());
        }
        if data.trim() == "[DONE]" {
            if self.from == ProviderProtocol::OpenAiChat && !self.finished {
                self.complete(output)?;
            }
            return Ok(());
        }
        let mut value: Value = serde_json::from_str(&data)
            .map_err(|_| invalid("upstream SSE contains invalid JSON"))?;
        if !value.is_object() {
            return Err(invalid("upstream SSE data must be a JSON object"));
        }
        if value.get("type").is_none() && !event.is_empty() {
            value["type"] = json!(event);
        }
        // A provider may append billing after its protocol's completion marker.
        // Forward the exact extension so the metering scanner still observes it.
        if self.finished {
            if value.get("copilot_usage").is_some()
                || value.pointer("/usage/copilot_usage").is_some()
                || value.pointer("/response/copilot_usage").is_some()
            {
                self.state.capture_billing(&value);
                output.push(self.frame(
                    json!({"type":"copilot_usage","copilot_usage":self.state.billing}),
                    Some("copilot_usage"),
                ));
            }
            return Ok(());
        }
        self.state.capture_billing(&value);
        if value["type"] == "error" || value.get("error").is_some_and(|v| !v.is_null()) {
            self.finished = true;
            output.push(self.frame(translate_error(&value, self.to), Some("error")));
            return Ok(());
        }
        match self.from {
            ProviderProtocol::OpenAiChat => self.chat(&value, output),
            ProviderProtocol::ClaudeMessages => self.claude(&value, output),
            ProviderProtocol::OpenAiResponses => self.responses(&value, output),
        }
    }

    fn frame(&mut self, mut value: Value, event: Option<&str>) -> Vec<u8> {
        if value.get("usage").is_some() && !self.state.usage.reported {
            value["kproxy_usage_source"] = json!("unreported");
        }
        if self.to == ProviderProtocol::OpenAiResponses && event == Some("error") {
            value = json!({"type":"error","message":value["error"]["message"],
                "code":value["error"]["code"],"param":value["error"]["param"]});
        }
        if self.to == ProviderProtocol::OpenAiResponses && value.get("type").is_some() {
            value["sequence_number"] = json!(self.sequence);
            self.sequence += 1;
        }
        if self.to == ProviderProtocol::OpenAiChat
            && event != Some("error")
            && event != Some("copilot_usage")
        {
            format!("data: {value}\n\n").into_bytes()
        } else {
            let name = event.unwrap_or_else(|| value["type"].as_str().unwrap_or("message"));
            format!("event: {name}\ndata: {value}\n\n").into_bytes()
        }
    }

    fn chat_chunk(&self, delta: Value, finish: Value) -> Value {
        json!({"id":self.state.id,"object":"chat.completion.chunk","created":self.state.created,
            "model":self.state.model,"choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    }

    fn start(&mut self, output: &mut Vec<Vec<u8>>) -> Result<(), BridgeError> {
        if self.started {
            return Ok(());
        }
        self.started = true;
        match self.to {
            ProviderProtocol::OpenAiChat => {
                output.push(self.frame(
                    self.chat_chunk(json!({"role":"assistant","content":""}), Value::Null),
                    None,
                ));
            }
            ProviderProtocol::ClaudeMessages => {
                output.push(self.frame(
                    json!({"type":"message_start","message":self.state.value(self.to,false)?}),
                    None,
                ));
            }
            ProviderProtocol::OpenAiResponses => {
                let response = self.state.value(self.to, false)?;
                output
                    .push(self.frame(json!({"type":"response.created","response":response}), None));
                output.push(self.frame(
                    json!({"type":"response.in_progress","response":response}),
                    None,
                ));
            }
        }
        Ok(())
    }

    fn start_block(&mut self, index: usize, output: &mut Vec<Vec<u8>>) -> Result<(), BridgeError> {
        self.start(output)?;
        if self.state.blocks[index].emitted {
            return Ok(());
        }
        self.state.blocks[index].emitted = true;
        let block = &self.state.blocks[index];
        let frames = match self.to {
            ProviderProtocol::ClaudeMessages => vec![
                json!({"type":"content_block_start","index":index,"content_block":block.claude_block(false)?}),
            ],
            ProviderProtocol::OpenAiChat if block.kind == BlockKind::Tool => {
                let tool_index = self.state.blocks[..index]
                    .iter()
                    .filter(|b| b.kind == BlockKind::Tool)
                    .count();
                vec![self.chat_chunk(json!({"tool_calls":[{"index":tool_index,"id":block.call_id,"type":"function","function":{"name":block.name,"arguments":""}}]}),Value::Null)]
            }
            ProviderProtocol::OpenAiChat => vec![],
            ProviderProtocol::OpenAiResponses => {
                let mut frames = vec![
                    json!({"type":"response.output_item.added","output_index":index,"item":block.response_item("in_progress")}),
                ];
                match block.kind {
                    BlockKind::Text => frames.push(json!({"type":"response.content_part.added","item_id":block.id,"output_index":index,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}})),
                    BlockKind::Refusal => frames.push(json!({"type":"response.content_part.added","item_id":block.id,"output_index":index,"content_index":0,"part":{"type":"refusal","refusal":""}})),
                    BlockKind::Reasoning => frames.push(json!({"type":"response.reasoning_summary_part.added","item_id":block.id,"output_index":index,"summary_index":0,"part":{"type":"summary_text","text":""}})),
                    BlockKind::Tool => {}
                }
                frames
            }
        };
        for frame in frames {
            output.push(self.frame(frame, None));
        }
        Ok(())
    }

    fn delta(
        &mut self,
        index: usize,
        text: &str,
        output: &mut Vec<Vec<u8>>,
    ) -> Result<(), BridgeError> {
        if text.is_empty() {
            return Ok(());
        }
        self.start_block(index, output)?;
        let block = &mut self.state.blocks[index];
        block.text.push_str(text);
        let frame = match self.to {
            ProviderProtocol::ClaudeMessages => {
                json!({"type":"content_block_delta","index":index,"delta":match block.kind {
                    BlockKind::Text | BlockKind::Refusal => json!({"type":"text_delta","text":text}),
                    BlockKind::Reasoning => json!({"type":"thinking_delta","thinking":text}),
                    BlockKind::Tool => json!({"type":"input_json_delta","partial_json":text}),
                }})
            }
            ProviderProtocol::OpenAiResponses => {
                let mut frame = json!({"type":match block.kind {
                    BlockKind::Text => "response.output_text.delta",
                    BlockKind::Refusal => "response.refusal.delta",
                    BlockKind::Reasoning => "response.reasoning_summary_text.delta",
                    BlockKind::Tool => "response.function_call_arguments.delta",
                },"item_id":block.id,"output_index":index,"delta":text});
                if matches!(block.kind, BlockKind::Text | BlockKind::Refusal) {
                    frame["content_index"] = json!(0);
                }
                if block.kind == BlockKind::Reasoning {
                    frame["summary_index"] = json!(0);
                }
                frame
            }
            ProviderProtocol::OpenAiChat => {
                let kind = block.kind.clone();
                let tool_index = self.state.blocks[..index]
                    .iter()
                    .filter(|b| b.kind == BlockKind::Tool)
                    .count();
                self.chat_chunk(match kind {
                    BlockKind::Text => json!({"content":text}),
                    BlockKind::Refusal => json!({"refusal":text}),
                    BlockKind::Reasoning => json!({"reasoning_content":text}),
                    BlockKind::Tool => json!({"tool_calls":[{"index":tool_index,"function":{"arguments":text}}]}),
                },Value::Null)
            }
        };
        output.push(self.frame(frame, None));
        Ok(())
    }

    fn chat(&mut self, value: &Value, output: &mut Vec<Vec<u8>>) -> Result<(), BridgeError> {
        self.state.metadata(value, self.from);
        for choice in array(&value["choices"]) {
            if choice["index"].as_u64().is_some_and(|i| i != 0) {
                return Err(invalid("multiple upstream choices cannot be adapted"));
            }
            self.start(output)?;
            let delta = &choice["delta"];
            for (field, kind) in [
                ("reasoning_content", BlockKind::Reasoning),
                ("reasoning", BlockKind::Reasoning),
                ("content", BlockKind::Text),
                ("refusal", BlockKind::Refusal),
            ] {
                if let Some(text) = delta[field].as_str() {
                    let key = match kind {
                        BlockKind::Reasoning => "reasoning",
                        BlockKind::Refusal => "refusal",
                        _ => "text",
                    };
                    let index = self.state.block(key.into(), kind);
                    self.delta(index, text, output)?;
                }
            }
            for tool in array(&delta["tool_calls"]) {
                let index = self.state.block(
                    format!("tool:{}", tool["index"].as_u64().unwrap_or(0)),
                    BlockKind::Tool,
                );
                let block = &mut self.state.blocks[index];
                if let Some(id) = tool["id"].as_str() {
                    block.call_id.push_str(id);
                }
                if let Some(name) = tool["function"]["name"].as_str() {
                    block.name.push_str(name);
                }
                if let Some(arguments) = tool["function"]["arguments"].as_str() {
                    self.delta(index, arguments, output)?;
                }
            }
            if let Some(reason) = choice["finish_reason"].as_str() {
                self.state.finish = reason.into();
            }
        }
        Ok(())
    }

    fn claude(&mut self, value: &Value, output: &mut Vec<Vec<u8>>) -> Result<(), BridgeError> {
        let key = format!("block:{}", value["index"].as_u64().unwrap_or(0));
        match string(&value["type"]) {
            "message_start" => {
                self.state.metadata(&value["message"], self.from);
                self.start(output)?;
            }
            "content_block_start" => {
                let part = &value["content_block"];
                let kind = match string(&part["type"]) {
                    "text" => BlockKind::Text,
                    "thinking" => BlockKind::Reasoning,
                    "tool_use" => BlockKind::Tool,
                    _ => return Ok(()),
                };
                let index = self.state.block(key, kind.clone());
                if kind == BlockKind::Tool {
                    self.state.blocks[index].call_id = string(&part["id"]).into();
                    self.state.blocks[index].name = string(&part["name"]).into();
                    self.start_block(index, output)?;
                    if part["input"].as_object().is_some_and(|v| !v.is_empty()) {
                        self.delta(index, &part["input"].to_string(), output)?;
                    }
                } else {
                    self.start_block(index, output)?;
                    self.delta(
                        index,
                        string(
                            &part[if kind == BlockKind::Text {
                                "text"
                            } else {
                                "thinking"
                            }],
                        ),
                        output,
                    )?;
                }
            }
            "content_block_delta" => {
                let Some(index) = self.state.blocks.iter().position(|b| b.key == key) else {
                    return Ok(());
                };
                let delta = &value["delta"];
                match string(&delta["type"]) {
                    "text_delta" => self.delta(index, string(&delta["text"]), output)?,
                    "thinking_delta" => self.delta(index, string(&delta["thinking"]), output)?,
                    "input_json_delta" => {
                        self.delta(index, string(&delta["partial_json"]), output)?
                    }
                    "signature_delta" => self.state.blocks[index]
                        .signature
                        .push_str(string(&delta["signature"])),
                    _ => {}
                }
            }
            "message_delta" => {
                self.state.usage.update(&value["usage"], self.from);
                if let Some(reason) = value["delta"]["stop_reason"].as_str() {
                    self.state.finish = reason.into();
                }
            }
            "message_stop" => self.complete(output)?,
            _ => {}
        }
        Ok(())
    }

    fn response_item(
        &mut self,
        item: &Value,
        index: u64,
        output: &mut Vec<Vec<u8>>,
    ) -> Result<(), BridgeError> {
        match string(&item["type"]) {
            "function_call" => {
                let i = self
                    .state
                    .block(format!("output:{index}:tool"), BlockKind::Tool);
                self.state.blocks[i].call_id = string(&item["call_id"]).into();
                self.state.blocks[i].name = string(&item["name"]).into();
                self.start_block(i, output)?;
                if self.state.blocks[i].text.is_empty() {
                    self.delta(i, string(&item["arguments"]), output)?;
                }
            }
            "message" => {
                for (part_index, part) in array(&item["content"]).iter().enumerate() {
                    let kind = if part["type"] == "refusal" {
                        BlockKind::Refusal
                    } else {
                        BlockKind::Text
                    };
                    let i = self
                        .state
                        .block(format!("output:{index}:text:{part_index}"), kind);
                    if self.state.blocks[i].text.is_empty() {
                        self.delta(
                            i,
                            part["text"]
                                .as_str()
                                .or_else(|| part["refusal"].as_str())
                                .unwrap_or_default(),
                            output,
                        )?;
                    }
                }
            }
            "reasoning" => {
                let i = self
                    .state
                    .block(format!("output:{index}:reasoning"), BlockKind::Reasoning);
                if self.state.blocks[i].text.is_empty() {
                    let text = array(&item["summary"])
                        .iter()
                        .filter_map(|p| p["text"].as_str())
                        .collect::<String>();
                    self.delta(i, &text, output)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn responses(&mut self, value: &Value, output: &mut Vec<Vec<u8>>) -> Result<(), BridgeError> {
        let index = value["output_index"].as_u64().unwrap_or(0);
        match string(&value["type"]) {
            "response.created" | "response.in_progress" => {
                self.state.metadata(&value["response"], self.from);
                self.start(output)?;
            }
            "response.output_item.added" | "response.output_item.done" => {
                self.response_item(&value["item"], index, output)?
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let i = self.state.block(
                    format!(
                        "output:{index}:text:{}",
                        value["content_index"].as_u64().unwrap_or(0)
                    ),
                    if value["type"] == "response.refusal.delta" {
                        BlockKind::Refusal
                    } else {
                        BlockKind::Text
                    },
                );
                self.delta(i, string(&value["delta"]), output)?;
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let i = self
                    .state
                    .block(format!("output:{index}:reasoning"), BlockKind::Reasoning);
                self.delta(i, string(&value["delta"]), output)?;
            }
            "response.function_call_arguments.delta" => {
                let i = self
                    .state
                    .block(format!("output:{index}:tool"), BlockKind::Tool);
                self.delta(i, string(&value["delta"]), output)?;
            }
            "response.completed" | "response.incomplete" => {
                let response = &value["response"];
                self.state.metadata(response, self.from);
                for (i, item) in array(&response["output"]).iter().enumerate() {
                    self.response_item(item, i as u64, output)?;
                }
                if response["status"] == "incomplete" {
                    self.state.finish = string(&response["incomplete_details"]["reason"]).into();
                }
                self.complete(output)?;
            }
            "response.failed" => {
                self.finished = true;
                output
                    .push(self.frame(translate_error(&value["response"], self.to), Some("error")));
            }
            _ => {}
        }
        Ok(())
    }

    fn complete(&mut self, output: &mut Vec<Vec<u8>>) -> Result<(), BridgeError> {
        if self.finished {
            return Ok(());
        }
        self.start(output)?;
        for i in 0..self.state.blocks.len() {
            self.start_block(i, output)?;
            let block = &self.state.blocks[i];
            if block.kind == BlockKind::Tool {
                if block.call_id.is_empty() || block.name.is_empty() {
                    return Err(invalid("upstream tool call has no id or name"));
                }
                // OpenAI arguments are strings; an output limit can interrupt
                // valid JSON in any protocol. Preserve the partial arguments
                // and incomplete finish reason rather than replacing it with
                // a bridge error. Completed Messages calls still require JSON.
                if self.to == ProviderProtocol::ClaudeMessages && self.state.incomplete().is_none()
                {
                    serde_json::from_str::<Value>(block.arguments())
                        .map_err(|_| invalid("upstream returned invalid tool arguments"))?;
                }
            }
            let mut frames = Vec::new();
            match self.to {
                ProviderProtocol::ClaudeMessages => {
                    if block.kind == BlockKind::Reasoning && !block.signature.is_empty() {
                        frames.push(json!({"type":"content_block_delta","index":i,"delta":{"type":"signature_delta","signature":block.signature}}));
                    }
                    frames.push(json!({"type":"content_block_stop","index":i}));
                }
                ProviderProtocol::OpenAiResponses => {
                    let mut done = json!({"item_id":block.id,"output_index":i});
                    match block.kind {
                        BlockKind::Text => {
                            done["type"] = json!("response.output_text.done");
                            done["content_index"] = json!(0);
                            done["text"] = json!(block.text);
                            frames.push(done);
                            frames.push(json!({"type":"response.content_part.done","item_id":block.id,"output_index":i,"content_index":0,"part":{"type":"output_text","text":block.text,"annotations":[]}}));
                        }
                        BlockKind::Refusal => {
                            done["type"] = json!("response.refusal.done");
                            done["content_index"] = json!(0);
                            done["refusal"] = json!(block.text);
                            frames.push(done);
                            frames.push(json!({"type":"response.content_part.done","item_id":block.id,"output_index":i,"content_index":0,"part":{"type":"refusal","refusal":block.text}}));
                        }
                        BlockKind::Reasoning => {
                            done["type"] = json!("response.reasoning_summary_text.done");
                            done["summary_index"] = json!(0);
                            done["text"] = json!(block.text);
                            frames.push(done);
                            frames.push(json!({"type":"response.reasoning_summary_part.done","item_id":block.id,"output_index":i,"summary_index":0,"part":{"type":"summary_text","text":block.text}}));
                        }
                        BlockKind::Tool => {
                            done["type"] = json!("response.function_call_arguments.done");
                            done["arguments"] = json!(block.arguments());
                            frames.push(done);
                        }
                    }
                    frames.push(json!({"type":"response.output_item.done","output_index":i,"item":block.response_item(self.state.output_status())}));
                }
                ProviderProtocol::OpenAiChat => {}
            }
            for frame in frames {
                output.push(self.frame(frame, None));
            }
        }
        match self.to {
            ProviderProtocol::ClaudeMessages => {
                let mut delta = json!({"type":"message_delta","delta":{"stop_reason":self.state.claude_stop(),"stop_sequence":null},"usage":self.state.usage.value(self.to)});
                if let Some(billing) = &self.state.billing {
                    delta["copilot_usage"] = billing.clone();
                }
                output.push(self.frame(delta, None));
                output.push(self.frame(json!({"type":"message_stop"}), None));
            }
            ProviderProtocol::OpenAiChat => {
                output.push(self.frame(
                    self.chat_chunk(json!({}), json!(self.state.chat_stop())),
                    None,
                ));
                let mut usage = self.chat_chunk(json!({}), Value::Null);
                usage["choices"] = json!([]);
                usage["usage"] = self.state.usage.value(self.to);
                if let Some(billing) = &self.state.billing {
                    usage["copilot_usage"] = billing.clone();
                }
                output.push(self.frame(usage, None));
                output.push(b"data: [DONE]\n\n".to_vec());
            }
            ProviderProtocol::OpenAiResponses => {
                let event = if self.state.incomplete().is_some() {
                    "response.incomplete"
                } else {
                    "response.completed"
                };
                output.push(self.frame(
                    json!({"type":event,"response":self.state.value(self.to,true)?}),
                    None,
                ));
            }
        }
        self.finished = true;
        Ok(())
    }
}
