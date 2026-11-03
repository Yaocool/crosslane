//! Enforce Chat's opt-in usage without hiding the upstream usage from metering.

use serde_json::Value;

const MAX_EVENT_BYTES: usize = 64 * 1024 * 1024;

pub(super) fn requested_omission(body: &[u8]) -> bool {
    #[derive(serde::Deserialize)]
    struct StreamOptions {
        include_usage: Option<bool>,
    }
    #[derive(serde::Deserialize)]
    struct Request {
        stream: Option<bool>,
        stream_options: Option<StreamOptions>,
    }
    // Ignore prompt/tool values while decoding just these small controls.
    serde_json::from_slice::<Request>(body).is_ok_and(|request| {
        request.stream == Some(true)
            && request
                .stream_options
                .and_then(|options| options.include_usage)
                != Some(true)
    })
}

pub(super) struct ChatUsageFilter {
    frame: Vec<u8>,
    line_bytes: usize,
    pending_cr: bool,
    limit: usize,
}

impl Default for ChatUsageFilter {
    fn default() -> Self {
        Self {
            frame: Vec::new(),
            line_bytes: 0,
            pending_cr: false,
            limit: MAX_EVENT_BYTES,
        }
    }
}

impl ChatUsageFilter {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
        let mut output = Vec::new();
        for &byte in bytes {
            if self.pending_cr {
                self.pending_cr = false;
                if byte == b'\n' {
                    self.append(byte)?;
                    self.end_line(&mut output);
                    continue;
                }
                self.end_line(&mut output);
            }
            self.append(byte)?;
            match byte {
                b'\r' => self.pending_cr = true,
                b'\n' => self.end_line(&mut output),
                _ => self.line_bytes += 1,
            }
        }
        Ok(output)
    }

    pub fn finish(&mut self) -> Vec<u8> {
        let mut output = Vec::new();
        if self.pending_cr {
            self.pending_cr = false;
            self.end_line(&mut output);
        }
        if !self.frame.is_empty() {
            emit(&self.frame, &mut output);
            self.frame.clear();
        }
        output
    }

    fn append(&mut self, byte: u8) -> Result<(), &'static str> {
        if self.frame.len() >= self.limit {
            return Err("upstream Chat SSE event exceeds the response adapter limit");
        }
        self.frame.push(byte);
        Ok(())
    }

    fn end_line(&mut self, output: &mut Vec<u8>) {
        if self.line_bytes == 0 {
            emit(&self.frame, output);
            self.frame.clear();
        }
        self.line_bytes = 0;
    }
}

fn lines(mut bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    std::iter::from_fn(move || {
        if bytes.is_empty() {
            return None;
        }
        let end = bytes
            .iter()
            .position(|byte| matches!(byte, b'\r' | b'\n'))
            .unwrap_or(bytes.len());
        let line = &bytes[..end];
        let delimiter = if bytes.get(end) == Some(&b'\r') && bytes.get(end + 1) == Some(&b'\n') {
            2
        } else {
            1
        };
        bytes = &bytes[(end + delimiter).min(bytes.len())..];
        Some(line)
    })
}

fn data(line: &[u8]) -> Option<&[u8]> {
    let value = line.strip_prefix(b"data:")?;
    Some(value.strip_prefix(b" ").unwrap_or(value))
}

fn emit(frame: &[u8], output: &mut Vec<u8>) {
    let payload = lines(frame)
        .filter_map(data)
        .collect::<Vec<_>>()
        .join(&b'\n');
    let Ok(Value::Object(mut event)) = serde_json::from_slice::<Value>(&payload) else {
        output.extend_from_slice(frame);
        return;
    };
    let has_billing = event.contains_key("copilot_usage");
    if !event.contains_key("usage") && !has_billing {
        output.extend_from_slice(frame);
        return;
    }
    let no_choices = event
        .get("choices")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty);
    if no_choices
        && (has_billing || event.get("usage").is_some_and(Value::is_object))
        && event.get("error").is_none_or(Value::is_null)
    {
        return;
    }
    event.remove("usage");
    event.remove("copilot_usage");
    event.remove("crosslane_usage_source");
    let encoded = Value::Object(event).to_string();
    let mut written = false;
    for line in lines(frame) {
        if data(line).is_some() {
            if !written {
                output.extend_from_slice(b"data: ");
                output.extend_from_slice(encoded.as_bytes());
                output.push(b'\n');
                written = true;
            }
        } else {
            output.extend_from_slice(line);
            output.push(b'\n');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_utf8_multiline_and_all_sse_line_endings_keep_content_and_completion() {
        for newline in ["\n", "\r\n", "\r"] {
            let text = [
                ": heartbeat", "", "id: 7", "event: message",
                r#"data: {"choices":[{"delta":{"content":"你好"}}],"usage":null}"#, "",
                r#"data: {"choices":[],"usage":{"prompt_tokens":100,"prompt_tokens_details":{"cached_tokens":60,"cache_write_tokens":10}}}"#, "",
                r#"data: {"choices":[{"finish_reason":"stop"}],"usage":{"completion_tokens":20}}"#, "",
                "data: [DONE]", "", "",
            ].join(newline);
            let mut filter = ChatUsageFilter::default();
            let mut output = Vec::new();
            for byte in text.bytes() {
                output.extend(filter.push(&[byte]).unwrap());
            }
            output.extend(filter.finish());
            let text = String::from_utf8(output).unwrap();
            assert!(
                text.contains("你好") && text.contains("id: 7") && text.contains("event: message")
            );
            assert!(
                text.contains("heartbeat")
                    && text.contains("[DONE]")
                    && text.contains("finish_reason")
            );
            assert!(!text.contains("usage") && !text.contains("cached_tokens"));
        }
    }

    #[test]
    fn multiline_json_and_errors_at_eof_are_preserved_without_usage() {
        let mut filter = ChatUsageFilter::default();
        let mut output = filter.push(b"data: {\r\ndata: \"choices\":[{\"delta\":{\"content\":\"ok\"}}],\r\ndata: \"usage\":null}\r\n\r\n").unwrap();
        output.extend(filter.push(b"event: error\ndata: {\"error\":{\"message\":\"failed\"},\"usage\":{\"prompt_tokens\":3}}").unwrap());
        output.extend(filter.finish());
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("ok") && text.contains("event: error") && text.contains("failed"));
        assert!(!text.contains("usage"));
        let mut bounded = ChatUsageFilter {
            limit: 32,
            ..Default::default()
        };
        assert!(bounded.push(&[b'x'; 33]).is_err());
    }
}
