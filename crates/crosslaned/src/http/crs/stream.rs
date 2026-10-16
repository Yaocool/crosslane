//! Bounded Claude SSE observation and CRS-safe wire encoding.

use std::collections::BTreeMap;

use super::*;

#[derive(Default)]
pub(super) struct EventBoundary {
    suffix: [u8; 4],
}

impl EventBoundary {
    pub(super) fn push(&mut self, byte: u8) -> bool {
        self.suffix.rotate_left(1);
        self.suffix[3] = byte;
        self.suffix.ends_with(b"\n\n") || self.suffix == *b"\r\n\r\n"
    }
}

pub(super) fn event_value(frame: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(frame).ok()?;
    let data = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|line| line.strip_prefix(' ').unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n");
    serde_json::from_str(&data).ok()
}

/// Observes frames without retaining an unbounded text/tool delta. Unknown
/// oversized events prohibit synthetic block completion, but later frames are
/// still inspected. No observer ever fabricates a successful terminal event.
#[derive(Default)]
pub(crate) struct ClaudeStreamState {
    pending: Vec<u8>,
    boundary: EventBoundary,
    oversized: bool,
    opaque: bool,
    started: bool,
    stopped: bool,
    stop_reason: bool,
    saw_error: bool,
    saw_content: bool,
    closed_blocks: usize,
    blocks: BTreeMap<u64, bool>, // true only for an untouched empty text block
}

impl ClaudeStreamState {
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        for byte in bytes {
            if self.pending.len() < MAX_PREFLIGHT_BYTES && !self.oversized {
                self.pending.push(*byte);
            } else {
                self.oversized = true;
                self.opaque = true;
                self.pending.clear();
            }
            if self.boundary.push(*byte) {
                if !self.oversized {
                    if let Some(value) = event_value(&self.pending) {
                        self.observe(&value);
                    }
                }
                self.pending.clear();
                self.oversized = false;
            }
        }
    }

    fn observe(&mut self, value: &Value) {
        let index = value["index"].as_u64();
        match value["type"].as_str() {
            Some("message_start") => self.started = true,
            Some("content_block_start") => {
                if let Some(index) = index {
                    // Bound even a malicious stream with no block stops.
                    if self.blocks.len() < 4096 {
                        self.blocks.insert(
                            index,
                            value["content_block"]["type"] == "text"
                                && value["content_block"]["text"] == "",
                        );
                    } else {
                        self.opaque = true;
                    }
                }
            }
            Some("content_block_delta") => {
                self.saw_content = true;
                if let Some(block) = index.and_then(|index| self.blocks.get_mut(&index)) {
                    *block = false;
                }
            }
            Some("content_block_stop") => {
                if let Some(index) = index {
                    self.blocks.remove(&index);
                    self.closed_blocks += 1;
                }
            }
            Some("message_delta") => {
                self.stop_reason |= value["delta"]["stop_reason"].as_str().is_some();
            }
            Some("message_stop") => self.stopped = true,
            Some("error") => self.saw_error = true,
            _ => {}
        }
    }

    pub(crate) fn incomplete(&self) -> bool {
        !self.saw_error
            && (!(self.started || self.saw_content)
                || !self.pending.is_empty()
                || self.oversized
                || !self.blocks.is_empty()
                || !(self.stopped || self.stop_reason))
    }

    fn error_prefix(&self, request_id: &str, model: &str) -> String {
        if self.opaque
            || self.saw_error
            || self.saw_content
            || self.closed_blocks > 0
            || self.stopped
        {
            return String::new();
        }
        if !self.started {
            let envelope = serde_json::json!({});
            let full = initial_error_stream(&envelope, request_id, model);
            return full.split("event: error\n").next().unwrap().to_owned();
        }
        if self.blocks.is_empty() {
            return concat!(
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n"
            ).into();
        }
        // Closing an incomplete tool block could execute a partial action.
        // Only an explicitly empty, untouched text block can be closed safely.
        if self.blocks.len() == 1 {
            if let Some((index, true)) = self.blocks.first_key_value() {
                return format!("event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":{index}}}\n\n");
            }
        }
        String::new()
    }
}

/// Escape Unicode inside SSE JSON strings before CRS decodes HTTP chunks with
/// Buffer.toString(). An ASCII escape can be split at any transport byte safely.
/// UTF-8, string escapes and SSE line prefixes can themselves cross chunks.
#[derive(Default)]
struct AsciiJsonEncoder {
    carry: Vec<u8>,
    prefix: usize,
    data_line: bool,
    line_len: usize,
    in_string: bool,
    escaped: bool,
}

impl AsciiJsonEncoder {
    fn push(&mut self, bytes: Bytes) -> Result<Bytes, ()> {
        // Most SSE bytes are ASCII. Keep their original allocation while still
        // tracking quotes and escapes for a later Unicode chunk.
        if self.carry.is_empty() && bytes.is_ascii() {
            for byte in &bytes {
                self.observe_ascii(*byte);
            }
            return Ok(bytes);
        }
        let joined = if self.carry.is_empty() {
            bytes
        } else {
            let mut joined = std::mem::take(&mut self.carry);
            joined.extend_from_slice(&bytes);
            Bytes::from(joined)
        };
        let (text, valid_end) = match std::str::from_utf8(&joined) {
            Ok(text) => (text, joined.len()),
            Err(error) if error.error_len().is_none() => (
                std::str::from_utf8(&joined[..error.valid_up_to()]).map_err(|_| ())?,
                error.valid_up_to(),
            ),
            Err(_) => return Err(()),
        };
        let mut output = None::<Vec<u8>>;
        for (offset, character) in text.char_indices() {
            if self.data_line && self.in_string && !character.is_ascii() {
                let out = output.get_or_insert_with(|| {
                    // Input slices are at most 8 KiB; supplementary characters
                    // have the largest expansion, from four bytes to twelve.
                    let mut out = Vec::with_capacity(joined.len().saturating_mul(3));
                    out.extend_from_slice(&joined[..offset]);
                    out
                });
                const HEX: &[u8; 16] = b"0123456789abcdef";
                for unit in character.encode_utf16(&mut [0; 2]) {
                    out.extend_from_slice(&[
                        b'\\',
                        b'u',
                        HEX[(*unit >> 12) as usize],
                        HEX[((*unit >> 8) & 15) as usize],
                        HEX[((*unit >> 4) & 15) as usize],
                        HEX[(*unit & 15) as usize],
                    ]);
                }
            } else if let Some(out) = output.as_mut() {
                out.extend_from_slice(&joined[offset..offset + character.len_utf8()]);
            }
            if character.is_ascii() {
                self.observe_ascii(character as u8);
            } else {
                self.line_len += 1;
                if !self.data_line {
                    self.prefix = usize::MAX;
                } else if self.escaped {
                    self.escaped = false;
                }
            }
        }
        self.carry.extend_from_slice(&joined[valid_end..]);
        Ok(output.map_or_else(|| joined.slice(..valid_end), Bytes::from))
    }

    fn observe_ascii(&mut self, byte: u8) {
        if byte == b'\n' {
            if self.line_len == 0 {
                self.in_string = false;
                self.escaped = false;
            }
            self.prefix = 0;
            self.data_line = false;
            self.line_len = 0;
            return;
        }
        if byte == b'\r' {
            return;
        }
        self.line_len += 1;
        if !self.data_line {
            if b"data:".get(self.prefix).copied() == Some(byte) {
                self.prefix += 1;
                self.data_line = self.prefix == 5;
            } else {
                self.prefix = usize::MAX;
            }
            return;
        }
        if self.escaped {
            self.escaped = false;
        } else if self.in_string && byte == b'\\' {
            self.escaped = true;
        } else if byte == b'"' {
            self.in_string = !self.in_string;
        }
    }
}

pub(super) fn protect_response(response: Response, request_id: &str, model: &str) -> Response {
    if !response.status().is_success()
        || !response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .eq_ignore_ascii_case("text/event-stream")
            })
    {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    parts.headers.remove(header::CONTENT_LENGTH);
    let request_id = request_id.to_owned();
    let model = model.to_owned();
    let stream = async_stream::stream! {
        let mut source = body.into_data_stream();
        let mut state = ClaudeStreamState::default();
        let mut encoder = AsciiJsonEncoder::default();
        // The inner rewriter emits each bounded error as a complete frame.
        while let Some(result) = source.next().await {
            let bytes = match result {
                Ok(bytes) => bytes,
                Err(_) => {
                    let error = sse_error_bytes("Upstream stream transport failed", "upstream_transport_interrupted", StatusCode::BAD_GATEWAY, &request_id);
                    let prefix = state.error_prefix(&request_id, &model);
                    yield Ok::<Bytes, std::io::Error>(Bytes::from(prefix));
                    yield Ok(error);
                    return;
                }
            };
            if bytes.len() <= MAX_PREFLIGHT_BYTES && event_value(&bytes).is_some_and(|value| value["type"] == "error") {
                let prefix = state.error_prefix(&request_id, &model);
                state.push(prefix.as_bytes());
                if let Ok(encoded) = encoder.push(Bytes::from(prefix)) {
                    if !encoded.is_empty() { yield Ok(encoded); }
                }
            }
            state.push(&bytes);
            // Bound the expansion of a single provider/network chunk too.
            for offset in (0..bytes.len()).step_by(8192) {
                let chunk = bytes.slice(offset..(offset + 8192).min(bytes.len()));
                match encoder.push(chunk) {
                    Ok(encoded) => if !encoded.is_empty() { yield Ok(encoded); },
                    Err(()) => {
                        yield Ok(Bytes::from_static(b"\n\n"));
                        yield Ok(sse_error_bytes("Upstream SSE contained invalid UTF-8", "upstream_event_stream_corrupt", StatusCode::BAD_GATEWAY, &request_id));
                        return;
                    }
                }
            }
        }
        if state.incomplete() || !encoder.carry.is_empty() {
            // Separate a partial frame from the diagnostic. Leave open tool
            // blocks open; a client must not execute an incomplete tool call.
            yield Ok(Bytes::from_static(b"\n\n"));
            let prefix = state.error_prefix(&request_id, &model);
            if let Ok(encoded) = encoder.push(Bytes::from(prefix)) {
                if !encoded.is_empty() { yield Ok(encoded); }
            }
            yield Ok(sse_error_bytes("Upstream event stream ended before message completion", "upstream_transport_interrupted", StatusCode::BAD_GATEWAY, &request_id));
        }
    };
    Response::from_parts(parts, Body::from_stream(stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn frame(value: Value) -> String {
        format!(
            "event: {}\ndata: {value}\n\n",
            value["type"].as_str().unwrap()
        )
    }

    fn start() -> String {
        frame(json!({"type":"message_start","message":{"id":"m"}}))
            + &frame(
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            )
    }

    #[test]
    fn ascii_chunks_keep_the_original_allocation_and_unicode_state() {
        let mut encoder = AsciiJsonEncoder::default();
        let prefix = Bytes::from_static(b"data: {\"text\":\"ASCII prefix ");
        let pointer = prefix.as_ptr();
        let encoded = encoder.push(prefix).unwrap();
        assert_eq!(encoded.as_ptr(), pointer);
        let suffix = encoder
            .push(Bytes::from_static("中文😀\"}\n\n".as_bytes()))
            .unwrap();
        assert!(suffix.is_ascii());
        let mut frame = encoded.to_vec();
        frame.extend_from_slice(&suffix);
        assert_eq!(
            event_value(&frame),
            Some(json!({"text":"ASCII prefix 中文😀"}))
        );
    }

    #[tokio::test]
    async fn empty_or_ping_only_eof_is_an_error_without_a_success_event() {
        for input in ["", "event: ping\ndata: {\"type\":\"ping\"}\n\n"] {
            let mut state = ClaudeStreamState::default();
            state.push(input.as_bytes());
            assert!(state.incomplete());
            let response = Response::builder()
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from(input))
                .unwrap();
            let bytes = to_bytes(
                protect_response(response, "trace_empty", "model").into_body(),
                8192,
            )
            .await
            .unwrap();
            let text = std::str::from_utf8(&bytes).unwrap();
            assert!(text.contains("event: message_start"));
            assert!(text.contains("upstream_transport_interrupted"));
            assert!(!text.contains("event: message_stop"));
        }
    }

    #[test]
    fn unicode_and_tool_json_survive_every_utf8_and_escape_boundary() {
        let value = json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"路径\":\"中文😀\\n\"}"},"extension":"补充"});
        let input = frame(value.clone());
        for size in 1..=11 {
            let mut encoder = AsciiJsonEncoder::default();
            let output: Vec<u8> = input
                .as_bytes()
                .chunks(size)
                .flat_map(|chunk| encoder.push(Bytes::copy_from_slice(chunk)).unwrap())
                .collect();
            assert!(output.is_ascii());
            assert!(encoder.carry.is_empty());
            assert_eq!(event_value(&output), Some(value.clone()));
        }
    }

    #[test]
    fn large_event_does_not_hide_later_error_or_expand_observer_memory() {
        let mut state = ClaudeStreamState::default();
        state.push(start().as_bytes());
        state.push(frame(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"中".repeat(MAX_PREFLIGHT_BYTES)}})).as_bytes());
        assert!(state.pending.len() <= MAX_PREFLIGHT_BYTES);
        state.push(frame(json!({"type":"error","error":{"message":"known failure"}})).as_bytes());
        assert!(state.saw_error);
        assert!(!state.incomplete());
        assert!(state.error_prefix("trace", "model").is_empty());
    }

    #[test]
    fn incomplete_tool_cannot_be_synthetically_closed() {
        let mut state = ClaudeStreamState::default();
        state.push(frame(json!({"type":"message_start"})).as_bytes());
        state.push(frame(json!({"type":"content_block_start","index":7,"content_block":{"type":"tool_use","id":"call1","name":"Bash","input":{}}})).as_bytes());
        assert!(state.incomplete());
        assert!(state.error_prefix("trace", "model").is_empty());
        state.push(frame(json!({"type":"message_stop"})).as_bytes());
        assert!(
            state.incomplete(),
            "message_stop cannot close an unfinished tool"
        );
    }

    #[test]
    fn closed_blocks_and_reported_stop_reason_are_distinct_from_partial_eof() {
        let mut state = ClaudeStreamState::default();
        state.push(start().as_bytes());
        assert!(state.incomplete());
        state.push(frame(json!({"type":"content_block_stop","index":0})).as_bytes());
        assert!(state.incomplete());
        state.push(
            frame(json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}})).as_bytes(),
        );
        assert!(!state.incomplete());
        state.push(b"event: message_stop\ndata: {\"type\":");
        assert!(state.incomplete());
    }

    #[tokio::test]
    async fn late_first_error_gets_safe_prelude_and_keeps_cause() {
        let response = Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(frame(
                json!({"type":"error","error":{"message":"late cause"}}),
            )))
            .unwrap();
        let response = protect_response(response, "trace_late", "model");
        let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(text.matches("event: message_start\n").count(), 1);
        assert_eq!(text.matches("event: content_block_stop\n").count(), 1);
        assert!(text.contains("late cause"));
        assert!(!text.contains("event: message_stop"));
    }

    #[tokio::test]
    async fn native_partial_eof_reports_failure_without_closing_tools() {
        let input = frame(json!({"type":"message_start"}))
            + &frame(
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call1","name":"Bash","input":{}}}),
            );
        let response = Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(input.clone()))
            .unwrap();
        let bytes = to_bytes(
            protect_response(response, "trace_eof", "model").into_body(),
            8192,
        )
        .await
        .unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.starts_with(&input));
        assert!(text.contains("upstream_transport_interrupted"));
        assert!(!text.contains("event: content_block_stop"));
        assert!(!text.contains("event: message_stop"));
    }
}
