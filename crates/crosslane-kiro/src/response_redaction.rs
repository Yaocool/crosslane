//! Restore request-local references in tools and explicitly marked checkpoints.

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::pin::Pin;

use bytes::{Bytes, BytesMut};
use crosslane_translate::redaction::RedactionSession;
use futures::{Stream, StreamExt};
use reqwest::{Response, ResponseBuilderExt};
use serde_json::Value;
use tokio_util::codec::Decoder;

use crate::{EventStreamDecoder, KiroEvent};

const MIN_FRAME_BYTES: usize = 16;
const MAX_FRAME_BYTES: usize = 50 * 1024 * 1024;
const MAX_TOOL_INPUT_BYTES: usize = 16 * 1024 * 1024;

/// Wrap at the transport boundary so every existing decoder, including retries
/// and summary consumers, sees the same restored tool parameters. Ordinary
/// answers, reasoning, metadata and unknown frames are never rewritten.
pub(crate) fn restore_tool_response(
    response: Response,
    session: RedactionSession,
    restore_summary: bool,
) -> Response {
    if session.is_empty() {
        return response;
    }
    let status = response.status();
    let version = response.version();
    let url = response.url().clone();
    let mut headers = response.headers().clone();
    headers.remove(reqwest::header::CONTENT_LENGTH);
    let extensions = response.extensions().clone();
    let state = RestoreState {
        source: Box::pin(response.bytes_stream()),
        session,
        buffer: BytesMut::new(),
        pending: Vec::new(),
        tools: BTreeMap::new(),
        ready: VecDeque::new(),
        last_tool_id: None,
        input_bytes: 0,
        pending_bytes: 0,
        restore_summary,
        summary_pending: String::new(),
        summary_template: None,
        ended: false,
    };
    let stream = futures::stream::try_unfold(state, |mut state| async move {
        loop {
            if let Some(bytes) = state.ready.pop_front() {
                return Ok::<_, io::Error>(Some((bytes, state)));
            }
            if state.ended {
                return Ok(None);
            }
            if let Some(frame) = state.next_frame()? {
                state.observe(frame)?;
                continue;
            }
            match state.source.next().await {
                Some(Ok(bytes)) => state.buffer.extend_from_slice(&bytes),
                Some(Err(error)) => return Err(io::Error::other(error)),
                None => {
                    if !state.buffer.is_empty() {
                        return Err(invalid("stream ended with an incomplete event message"));
                    }
                    state.flush_summary()?;
                    state.flush()?;
                    state.ended = true;
                }
            }
        }
    });
    let mut rebuilt = http::Response::builder()
        .status(status)
        .version(version)
        .url(url)
        .body(reqwest::Body::wrap_stream(stream))
        .expect("original HTTP response parts remain valid");
    *rebuilt.headers_mut() = headers;
    rebuilt.extensions_mut().extend(extensions);
    Response::from(rebuilt)
}

struct RestoreState {
    source: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    session: RedactionSession,
    buffer: BytesMut,
    pending: Vec<PendingFrame>,
    tools: BTreeMap<String, ToolInput>,
    ready: VecDeque<Bytes>,
    last_tool_id: Option<String>,
    input_bytes: usize,
    pending_bytes: usize,
    restore_summary: bool,
    summary_pending: String,
    summary_template: Option<Bytes>,
    ended: bool,
}

struct PendingFrame {
    bytes: Bytes,
    tool: Option<String>,
}

#[derive(Default)]
struct ToolInput {
    id: String,
    name: String,
    input: String,
    last_frame: usize,
    stopped: bool,
}

impl RestoreState {
    fn next_frame(&mut self) -> io::Result<Option<Bytes>> {
        if self.buffer.len() < MIN_FRAME_BYTES {
            return Ok(None);
        }
        let length = u32::from_be_bytes(self.buffer[..4].try_into().expect("four bytes")) as usize;
        if !(MIN_FRAME_BYTES..=MAX_FRAME_BYTES).contains(&length) {
            return Err(invalid("invalid event stream frame length"));
        }
        if self.buffer.len() < length {
            return Ok(None);
        }
        Ok(Some(self.buffer.split_to(length).freeze()))
    }

    fn observe(&mut self, mut bytes: Bytes) -> io::Result<()> {
        // Reuse the authoritative decoder for CRC, header, JSON and event-type
        // validation. All original bytes are retained for unmodified frames.
        let mut checked = BytesMut::from(bytes.as_ref());
        let event = EventStreamDecoder
            .decode(&mut checked)?
            .ok_or_else(|| invalid("incomplete event stream frame"))?;
        if self.restore_summary {
            if matches!(event, KiroEvent::AssistantResponse { .. }) {
                if let Some(content) = assistant_content(&bytes)? {
                    self.summary_template = Some(bytes.clone());
                    let mut combined = std::mem::take(&mut self.summary_pending);
                    combined.push_str(&content);
                    let cut = summary_safe_cut(&combined);
                    self.summary_pending = combined[cut..].to_owned();
                    let restored = self.checkpoint_content(&combined[..cut])?;
                    // Even a fully held marker emits an empty frame, retaining
                    // transport activity without exposing a partial reference.
                    bytes = rewrite_assistant_content(&bytes, &restored)?;
                }
            } else if matches!(
                event,
                KiroEvent::ToolUse { .. }
                    | KiroEvent::MessageMetadata { .. }
                    | KiroEvent::Usage { .. }
                    | KiroEvent::Error { .. }
            ) {
                self.flush_summary()?;
            }
        }
        if let KiroEvent::ToolUse {
            id,
            name,
            input_delta,
            stop,
        } = event
        {
            let key = if id.is_empty() {
                self.last_tool_id.clone().unwrap_or_default()
            } else {
                self.last_tool_id = Some(id.clone());
                id.clone()
            };
            self.input_bytes = self.input_bytes.saturating_add(input_delta.len());
            if self.input_bytes > MAX_TOOL_INPUT_BYTES {
                return Err(invalid("tool input exceeded the 16 MiB response limit"));
            }
            let tool = self.tools.entry(key.clone()).or_default();
            if tool.id.is_empty() {
                tool.id = key.clone();
            }
            if tool.name.is_empty() {
                tool.name = name;
            }
            tool.input.push_str(&input_delta);
            tool.last_frame = self.pending.len();
            tool.stopped |= stop;
            self.enqueue(bytes, Some(key))?;
            if self.tools.values().all(|tool| tool.stopped) {
                self.flush()?;
            } else {
                self.ready.push_back(activity_frame());
            }
        } else if self.pending.is_empty() {
            self.ready.push_back(bytes);
        } else {
            let terminal = matches!(event, KiroEvent::Error { .. })
                || matches!(&event, KiroEvent::MessageMetadata { usage } if terminal_metadata(usage));
            // A valid JSON call does not require an explicit stop frame in the
            // existing protocol. Release it before following text or metadata.
            // Invalid arguments stay pending until a terminal boundary, where
            // they are released unchanged for existing refusal/error handling.
            if terminal || self.tools.values().all(|tool| valid_input(&tool.input)) {
                self.flush()?;
                self.ready.push_back(bytes);
            } else {
                self.enqueue(bytes, None)?;
                self.ready.push_back(activity_frame());
            }
        }
        Ok(())
    }

    fn enqueue(&mut self, bytes: Bytes, tool: Option<String>) -> io::Result<()> {
        self.pending_bytes = self.pending_bytes.saturating_add(bytes.len());
        if self.pending_bytes > MAX_FRAME_BYTES {
            return Err(invalid(
                "pending tool response exceeded its buffering limit",
            ));
        }
        self.pending.push(PendingFrame { bytes, tool });
        Ok(())
    }

    fn flush_summary(&mut self) -> io::Result<()> {
        if self.summary_pending.is_empty() {
            return Ok(());
        }
        if let Some(template) = &self.summary_template {
            let content = self.checkpoint_content(&self.summary_pending)?;
            let frame = rewrite_assistant_content(template, &content)?;
            if self.pending.is_empty() {
                self.ready.push_back(frame);
            } else {
                self.enqueue(frame, None)?;
            }
        }
        self.summary_pending.clear();
        Ok(())
    }

    fn checkpoint_content(&self, text: &str) -> io::Result<String> {
        // Capsules expand to Base64 JSON. Check their encoded allocation bound
        // before constructing them; a tiny stream can repeat a huge credential
        // many times and otherwise allocate far beyond the frame-size limit.
        if self.session.checkpoint_expansion_bound(text) > MAX_FRAME_BYTES {
            return Err(invalid("summary checkpoint exceeded its expansion limit"));
        }
        Ok(self.session.checkpoint_text(text))
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut restored = BTreeMap::new();
        for (key, tool) in &self.tools {
            let mut value = match serde_json::from_str::<Value>(&tool.input) {
                Ok(value) => value,
                // Empty, incomplete and invalid calls must retain the original
                // event sequence; never repair them into executable calls.
                Err(_) => continue,
            };
            if self.session.tool_input_expansion_bound(&value) > MAX_TOOL_INPUT_BYTES {
                return Err(invalid("restored tool input exceeded its expansion limit"));
            }
            self.session
                .restore_tool_input_for_tool(&tool.name, &mut value);
            let input = serde_json::to_string(&value)
                .map_err(|_| invalid("restored tool input could not be serialized"))?;
            if input.len() > MAX_TOOL_INPUT_BYTES {
                return Err(invalid(
                    "restored tool input exceeded the 16 MiB response limit",
                ));
            }
            restored.insert(key.clone(), input);
        }
        for (index, frame) in self.pending.drain(..).enumerate() {
            let bytes = match frame
                .tool
                .as_ref()
                .and_then(|key| self.tools.get(key).zip(restored.get(key)))
            {
                Some((tool, input)) => rewrite_input(
                    &frame.bytes,
                    Some(if index == tool.last_frame { input } else { "" }),
                    &tool.id,
                    &tool.name,
                )?,
                None => match frame.tool.as_ref().and_then(|key| self.tools.get(key)) {
                    // A valid call may already have been emitted at a preceding
                    // text/metadata boundary. A later empty-ID stop fragment
                    // still belongs to that call, rather than a new empty-ID
                    // tool. Preserve its original input and only fill identity.
                    Some(tool) => rewrite_input(&frame.bytes, None, &tool.id, &tool.name)?,
                    None => frame.bytes,
                },
            };
            self.ready.push_back(bytes);
        }
        self.tools.clear();
        self.pending_bytes = 0;
        Ok(())
    }
}

fn valid_input(input: &str) -> bool {
    serde_json::from_str::<Value>(input).is_ok()
}

/// Preserve upstream read activity while arguments await complete JSON. This
/// empty text event carries no input, tool identity, completion or secret data.
fn activity_frame() -> Bytes {
    let mut headers = Vec::new();
    for (name, value) in [
        (":message-type", "event"),
        (":event-type", "assistantResponseEvent"),
        (":content-type", "application/json"),
    ] {
        headers.push(name.len() as u8);
        headers.extend_from_slice(name.as_bytes());
        headers.push(7);
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    }
    let payload = br#"{"content":""}"#;
    let length = MIN_FRAME_BYTES + headers.len() + payload.len();
    let mut bytes = Vec::with_capacity(length);
    bytes.extend_from_slice(&(length as u32).to_be_bytes());
    bytes.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    let crc = crc32fast::hash(&bytes);
    bytes.extend_from_slice(&crc.to_be_bytes());
    bytes.extend(headers);
    bytes.extend_from_slice(payload);
    let crc = crc32fast::hash(&bytes);
    bytes.extend_from_slice(&crc.to_be_bytes());
    Bytes::from(bytes)
}

fn terminal_metadata(value: &Value) -> bool {
    match value {
        Value::Object(fields) => fields.iter().any(|(key, value)| {
            matches!(key.as_str(), "stopReason" | "stop_reason" | "refusal") && !value.is_null()
                || terminal_metadata(value)
        }),
        Value::Array(values) => values.iter().any(terminal_metadata),
        _ => false,
    }
}

fn rewrite_input(frame: &Bytes, input: Option<&str>, id: &str, name: &str) -> io::Result<Bytes> {
    let headers_len = u32::from_be_bytes(frame[4..8].try_into().expect("four bytes")) as usize;
    let payload_start = 12 + headers_len;
    let mut value: Value = serde_json::from_slice(&frame[payload_start..frame.len() - 4])
        .map_err(|_| invalid("invalid tool event JSON"))?;
    let body = if value.get("toolUseEvent").is_some() {
        value.get_mut("toolUseEvent").expect("known field")
    } else {
        &mut value
    };
    let body = body
        .as_object_mut()
        .ok_or_else(|| invalid("tool event body must be an object"))?;
    let mut changed = false;
    if let Some(input) = input {
        let field = if body.contains_key("input") || !body.contains_key("inputDelta") {
            "input"
        } else {
            "inputDelta"
        };
        body.insert(field.into(), Value::String(input.into()));
        changed = true;
    }
    if !id.is_empty()
        && body
            .get("toolUseId")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        body.insert("toolUseId".into(), Value::String(id.into()));
        changed = true;
    }
    if !name.is_empty()
        && body
            .get("name")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        body.insert("name".into(), Value::String(name.into()));
        changed = true;
    }
    if changed {
        encode_frame(frame, &value)
    } else {
        Ok(frame.clone())
    }
}

fn assistant_content(frame: &Bytes) -> io::Result<Option<String>> {
    let value = frame_payload(frame)?;
    Ok(value
        .get("assistantResponseEvent")
        .unwrap_or(&value)
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_owned))
}

fn rewrite_assistant_content(frame: &Bytes, content: &str) -> io::Result<Bytes> {
    let mut value = frame_payload(frame)?;
    let body = if value.get("assistantResponseEvent").is_some() {
        value
            .get_mut("assistantResponseEvent")
            .expect("known field")
    } else {
        &mut value
    };
    body.as_object_mut()
        .ok_or_else(|| invalid("assistant event body must be an object"))?
        .insert("content".into(), Value::String(content.into()));
    encode_frame(frame, &value)
}

fn frame_payload(frame: &Bytes) -> io::Result<Value> {
    let headers_len = u32::from_be_bytes(frame[4..8].try_into().expect("four bytes")) as usize;
    serde_json::from_slice(&frame[12 + headers_len..frame.len() - 4])
        .map_err(|_| invalid("invalid event JSON"))
}

fn encode_frame(frame: &Bytes, value: &Value) -> io::Result<Bytes> {
    let headers_len = u32::from_be_bytes(frame[4..8].try_into().expect("four bytes")) as usize;
    let payload_start = 12 + headers_len;
    let payload = serde_json::to_vec(value)
        .map_err(|_| invalid("restored tool event could not be serialized"))?;
    let length = MIN_FRAME_BYTES + headers_len + payload.len();
    if length > MAX_FRAME_BYTES {
        return Err(invalid(
            "restored event stream frame exceeded its size limit",
        ));
    }
    let mut output = Vec::with_capacity(length);
    output.extend_from_slice(&(length as u32).to_be_bytes());
    output.extend_from_slice(&(headers_len as u32).to_be_bytes());
    let prelude_crc = crc32fast::hash(&output);
    output.extend_from_slice(&prelude_crc.to_be_bytes());
    output.extend_from_slice(&frame[12..payload_start]);
    output.extend_from_slice(&payload);
    let message_crc = crc32fast::hash(&output);
    output.extend_from_slice(&message_crc.to_be_bytes());
    Ok(Bytes::from(output))
}

/// Retain only a suffix that can still become a session reference. Completed
/// references are emitted together; their closing underscores are not mistaken
/// for the start of the next reference.
fn summary_safe_cut(text: &str) -> usize {
    const PREFIX: &str = "__CROSSLANE_SECRET_";
    const MAX_INDEX_DIGITS: usize = 20;
    let mut suffix_start = 0;
    if let Some(start) = text.rfind(PREFIX) {
        let suffix = &text[start + PREFIX.len()..];
        let nonce_len = suffix.len().min(32);
        if suffix.as_bytes()[..nonce_len]
            .iter()
            .all(u8::is_ascii_hexdigit)
        {
            if suffix.len() < 32 {
                return start;
            }
            if let Some(index) = suffix[32..].strip_prefix('_') {
                let digits = index.bytes().take_while(u8::is_ascii_digit).count();
                // Real indices fit usize. A malformed reference must not keep
                // an arbitrarily long suffix buffered across model chunks.
                if digits > MAX_INDEX_DIGITS {
                    return text.len();
                }
                let end = &index[digits..];
                if end.is_empty() || (digits > 0 && end == "_") {
                    return start;
                }
                if digits > 0 && end.starts_with("__") {
                    suffix_start = start + PREFIX.len() + 32 + 1 + digits + 2;
                }
            } else if suffix.len() == 32 {
                return start;
            }
        }
    }
    let tail = &text[suffix_start..];
    for length in (1..=PREFIX.len().min(tail.len())).rev() {
        if tail.ends_with(&PREFIX[..length]) {
            return text.len() - length;
        }
    }
    text.len()
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosslane_translate::KiroPayload;
    use serde_json::json;

    #[derive(Clone, Debug, PartialEq)]
    struct RequestExtension(u32);

    #[test]
    fn malformed_summary_reference_has_bounded_buffering() {
        let text = format!(
            "__CROSSLANE_SECRET_{}_{}",
            "a".repeat(32),
            "1".repeat(10_000)
        );
        assert_eq!(summary_safe_cut(&text), text.len());
        let valid_partial = format!("__CROSSLANE_SECRET_{}_12_", "a".repeat(32));
        assert_eq!(summary_safe_cut(&valid_partial), 0);
    }

    fn marker(session: &RedactionSession, original: Value) -> String {
        let mut payload: KiroPayload = serde_json::from_value(json!({
            "conversationState":{
                "chatTriggerType":"MANUAL", "conversationId":"test",
                "currentMessage":{"userInputMessage":{
                    "content":"continue", "modelId":"test", "origin":"CLI"
                }},
                "history":[{"assistantResponseMessage":{
                    "content":"", "toolUses":[{"toolUseId":"source", "name":"http", "input":{"token":original}}]
                }}]
            }
        })).unwrap();
        session.redact_payload(&mut payload, &[]);
        payload.conversation_state.history[0]
            .assistant_response_message
            .as_ref()
            .unwrap()
            .tool_uses[0]
            .input["token"]
            .as_str()
            .expect("secret marker")
            .to_owned()
    }

    fn frame(event_type: &str, value: Value) -> Bytes {
        let mut headers = Vec::new();
        for (name, value) in [
            (":message-type", "event"),
            (":event-type", event_type),
            (":content-type", "application/json"),
        ] {
            headers.push(name.len() as u8);
            headers.extend_from_slice(name.as_bytes());
            headers.push(7);
            headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
            headers.extend_from_slice(value.as_bytes());
        }
        let payload = serde_json::to_vec(&value).unwrap();
        let length = MIN_FRAME_BYTES + headers.len() + payload.len();
        let mut bytes = Vec::with_capacity(length);
        bytes.extend_from_slice(&(length as u32).to_be_bytes());
        bytes.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        let crc = crc32fast::hash(&bytes);
        bytes.extend_from_slice(&crc.to_be_bytes());
        bytes.extend(headers);
        bytes.extend(payload);
        let crc = crc32fast::hash(&bytes);
        bytes.extend_from_slice(&crc.to_be_bytes());
        Bytes::from(bytes)
    }

    fn tool(id: &str, name: &str, input: &str, stop: bool) -> Bytes {
        frame(
            "toolUseEvent",
            json!({"toolUseId":id,"name":name,"input":input,"stop":stop}),
        )
    }

    fn response(frames: &[Bytes], fragment_size: usize) -> Response {
        let bytes: Vec<u8> = frames
            .iter()
            .flat_map(|frame| frame.iter().copied())
            .collect();
        let chunks: Vec<_> = bytes
            .chunks(fragment_size)
            .map(|chunk| Ok::<_, io::Error>(Bytes::copy_from_slice(chunk)))
            .collect();
        let mut response = http::Response::builder()
            .status(201)
            .url("https://example.invalid/runtime".parse().unwrap())
            .header("content-type", "application/vnd.amazon.eventstream")
            .header("content-length", bytes.len())
            .header("x-request-id", "fixture-request")
            .body(reqwest::Body::wrap_stream(futures::stream::iter(chunks)))
            .unwrap();
        response.extensions_mut().insert(RequestExtension(42));
        Response::from(response)
    }

    fn decode(bytes: &[u8]) -> Vec<KiroEvent> {
        let mut source = BytesMut::from(bytes);
        let mut decoder = EventStreamDecoder;
        let mut events = Vec::new();
        while let Some(event) = decoder.decode(&mut source).unwrap() {
            events.push(event);
        }
        assert!(source.is_empty());
        events
    }

    fn tool_inputs(events: &[KiroEvent]) -> BTreeMap<String, Value> {
        let mut inputs = BTreeMap::<String, String>::new();
        for event in events {
            if let KiroEvent::ToolUse {
                id, input_delta, ..
            } = event
            {
                inputs.entry(id.clone()).or_default().push_str(input_delta);
            }
        }
        inputs
            .into_iter()
            .map(|(id, input)| (id, serde_json::from_str(&input).unwrap()))
            .collect()
    }

    #[tokio::test]
    async fn repeated_large_references_fail_before_unbounded_expansion() {
        let session = RedactionSession::new();
        let reference = marker(&session, json!("x".repeat(1024 * 1024)));
        let input = json!({"values":vec![reference; 20]}).to_string();
        let source = response(&[tool("large", "http", &input, true)], 4096);
        assert!(restore_tool_response(source, session, false)
            .bytes()
            .await
            .is_err());
    }

    #[tokio::test]
    async fn repeated_large_summary_references_fail_before_capsule_expansion() {
        let session = RedactionSession::new();
        let reference = marker(&session, json!("x".repeat(1024 * 1024)));
        let source = response(
            &[frame(
                "assistantResponseEvent",
                json!({"content":reference.repeat(100)}),
            )],
            4096,
        );
        assert!(restore_tool_response(source, session, true)
            .bytes()
            .await
            .is_err());
    }

    #[tokio::test]
    async fn restores_fragmented_tool_json_without_touching_text_or_transport_metadata() {
        let session = RedactionSession::new();
        let secret = "opaque\"quoted\\secret\n中文😀";
        let reference = marker(&session, json!(secret));
        let input =
            json!({"token":reference,"headers":{"Authorization":format!("Bearer {reference}")}})
                .to_string();
        let split = input.find(&reference).unwrap() + reference.len() / 2;
        let before = frame("assistantResponseEvent", json!({"content":reference}));
        let after = frame("messageMetadataEvent", json!({"usage":{"outputTokens":7}}));
        let frames = vec![
            before.clone(),
            tool("call-1", "http", &input[..split], false),
            tool("", "", &input[split..], true),
            after.clone(),
        ];
        let restored = restore_tool_response(response(&frames, 3), session, false);
        assert_eq!(restored.status(), 201);
        assert_eq!(restored.url().as_str(), "https://example.invalid/runtime");
        assert_eq!(restored.headers()["x-request-id"], "fixture-request");
        assert!(restored.headers().get("content-length").is_none());
        assert_eq!(
            restored.extensions().get::<RequestExtension>(),
            Some(&RequestExtension(42))
        );
        let bytes = restored.bytes().await.unwrap();
        assert!(bytes.starts_with(&before));
        assert!(bytes.ends_with(&after));
        let events = decode(&bytes);
        assert!(
            matches!(&events[0],KiroEvent::AssistantResponse { content } if content == &reference)
        );
        assert_eq!(
            tool_inputs(&events)["call-1"],
            json!({"token":secret,"headers":{"Authorization":format!("Bearer {secret}")}})
        );
    }

    #[tokio::test]
    async fn restores_interleaved_calls_and_original_json_scalar_types() {
        let session = RedactionSession::new();
        let numeric = marker(&session, json!(123456));
        let boolean = marker(&session, json!(true));
        let a = json!({"password":numeric}).to_string();
        let b = json!({"token":boolean}).to_string();
        let frames = vec![
            tool("a", "verify", &a[..5], false),
            tool("b", "verify", &b[..7], false),
            tool("a", "", &a[5..], true),
            tool("b", "", &b[7..], true),
        ];
        let bytes = restore_tool_response(response(&frames, 5), session, false)
            .bytes()
            .await
            .unwrap();
        let inputs = tool_inputs(&decode(&bytes));
        assert_eq!(inputs["a"], json!({"password":123456}));
        assert_eq!(inputs["b"], json!({"token":true}));
    }

    #[tokio::test]
    async fn missing_stop_valid_json_is_restored_before_metadata_and_at_eof() {
        for metadata in [false, true] {
            let session = RedactionSession::new();
            let reference = marker(&session, json!("opaque-secret"));
            let mut frames = vec![tool(
                "a",
                "http",
                &json!({"token":reference}).to_string(),
                false,
            )];
            if metadata {
                frames.push(frame(
                    "messageMetadataEvent",
                    json!({"stopReason":"end_turn"}),
                ));
            }
            let bytes = restore_tool_response(response(&frames, 7), session, false)
                .bytes()
                .await
                .unwrap();
            let events = decode(&bytes);
            assert_eq!(tool_inputs(&events)["a"], json!({"token":"opaque-secret"}));
            assert!(matches!(
                events
                    .iter()
                    .find(|event| matches!(event, KiroEvent::ToolUse { .. })),
                Some(KiroEvent::ToolUse { stop: false, .. })
            ));
        }
    }

    #[tokio::test]
    async fn empty_id_stop_after_nonterminal_metadata_closes_the_original_call() {
        let session = RedactionSession::new();
        let reference = marker(&session, json!("opaque-secret"));
        let frames = vec![
            tool("a", "http", &json!({"token":reference}).to_string(), false),
            frame("messageMetadataEvent", json!({"usage":{"outputTokens":7}})),
            tool("", "", "", true),
        ];
        let bytes = restore_tool_response(response(&frames, 7), session, false)
            .bytes()
            .await
            .unwrap();
        let events = decode(&bytes);
        assert_eq!(
            tool_inputs(&events),
            BTreeMap::from([("a".into(), json!({"token":"opaque-secret"}))])
        );
        assert!(matches!(events.last(), Some(KiroEvent::ToolUse {id,stop:true,..}) if id == "a"));
    }

    #[tokio::test]
    async fn ordinary_arguments_and_alternate_native_envelopes_remain_usable() {
        let session = RedactionSession::new();
        let reference = marker(&session, json!("opaque-secret"));
        let plain = json!({
            "path":"/tmp/中文 file.txt", "count":3, "overwrite":false,
            "body":"{ \"ordinary\" : \"JSON string\" }",
            "command":"printf '%s' \"$HOME\"", "optional":null
        });
        let wrapped_input = json!({"token":reference,"count":2}).to_string();
        let frames = vec![
            frame(
                "toolUseEvent",
                json!({"toolUseId":"plain","name":"Bash","input":plain,"stop":true}),
            ),
            frame(
                "toolUseEvent",
                json!({"toolUseEvent":{
                    "toolUseId":"wrapped","name":"http","inputDelta":wrapped_input,"stop":true
                }}),
            ),
        ];
        let bytes = restore_tool_response(response(&frames, 1), session, false)
            .bytes()
            .await
            .unwrap();
        let inputs = tool_inputs(&decode(&bytes));
        assert_eq!(inputs["plain"], plain);
        assert_eq!(
            inputs["wrapped"],
            json!({"token":"opaque-secret","count":2})
        );
    }

    #[tokio::test]
    async fn non_tool_frames_are_byte_identical_outside_explicit_summaries() {
        let session = RedactionSession::new();
        let reference = marker(&session, json!("opaque-secret"));
        let frames = vec![
            frame("assistantResponseEvent", json!({"content":reference})),
            frame(
                "reasoningContentEvent",
                json!({"text":reference,"signature":"opaque-signature"}),
            ),
            frame(
                "citationEvent",
                json!({"citations":[{"link":"https://example.invalid/","text":reference}]}),
            ),
            frame("futureEvent", json!({"unrecognized":reference})),
            frame("usageEvent", json!({"usage":{"outputTokens":7}})),
            frame("messageMetadataEvent", json!({"stopReason":"end_turn"})),
        ];
        let expected: Vec<_> = frames
            .iter()
            .flat_map(|frame| frame.iter().copied())
            .collect();
        let bytes = restore_tool_response(response(&frames, 3), session, false)
            .bytes()
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), expected.as_slice());
    }

    #[tokio::test]
    async fn refusal_and_truncated_arguments_are_not_completed_or_reordered() {
        let session = RedactionSession::new();
        let reference = marker(&session, json!("opaque-secret"));
        let partial = tool(
            "a",
            "http",
            &format!("{{\"token\":\"{}", &reference[..reference.len() / 2]),
            false,
        );
        let refusal = frame(
            "messageMetadataEvent",
            json!({"stopReason":"CONTENT_FILTERED","stopDetails":{"refusal":{"category":"test"}}}),
        );
        let frames = vec![partial.clone(), refusal.clone()];
        let bytes = restore_tool_response(response(&frames, 2), session, false)
            .bytes()
            .await
            .unwrap();
        let expected: Vec<u8> = frames
            .iter()
            .flat_map(|frame| frame.iter().copied())
            .collect();
        assert!(bytes.ends_with(&expected));
        assert!(matches!(
            decode(&bytes).last(),
            Some(KiroEvent::MessageMetadata { .. })
        ));
    }

    #[tokio::test]
    async fn unknown_references_are_not_restored_and_crc_failures_remain_errors() {
        let session = RedactionSession::new();
        marker(&session, json!("our-secret"));
        let other = marker(&RedactionSession::new(), json!("another-secret"));
        let frames = vec![tool("a", "http", &json!({"token":other}).to_string(), true)];
        let bytes = restore_tool_response(response(&frames, 1000), session.clone(), false)
            .bytes()
            .await
            .unwrap();
        assert_eq!(tool_inputs(&decode(&bytes))["a"], json!({"token":other}));
        let mut broken = frames[0].to_vec();
        let last = broken.len() - 1;
        broken[last] ^= 1;
        assert!(
            restore_tool_response(response(&[Bytes::from(broken)], 1000), session, false)
                .bytes()
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn explicit_summaries_restore_references_at_every_chunk_boundary() {
        let session = RedactionSession::new();
        let secret = "summary\"secret\n</summary>中文😀";
        let reference = marker(&session, json!(secret));
        let checkpoint = session.checkpoint_text(&reference);
        for boundary in 0..=reference.len() {
            let frames = vec![
                frame(
                    "assistantResponseEvent",
                    json!({"content":format!("before {}",&reference[..boundary])}),
                ),
                frame(
                    "assistantResponseEvent",
                    json!({"assistantResponseEvent":{"content":format!("{} after",&reference[boundary..])}}),
                ),
                frame("messageMetadataEvent", json!({"stopReason":"end_turn"})),
            ];
            for restore_summary in [false, true] {
                let bytes =
                    restore_tool_response(response(&frames, 3), session.clone(), restore_summary)
                        .bytes()
                        .await
                        .unwrap();
                let events = decode(&bytes);
                let text: String = events
                    .iter()
                    .filter_map(|event| match event {
                        KiroEvent::AssistantResponse { content } => Some(content.as_str()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(
                    text,
                    format!(
                        "before {} after",
                        if restore_summary {
                            &checkpoint
                        } else {
                            &reference
                        }
                    )
                );
                assert_eq!(
                    events.len(),
                    frames.len(),
                    "empty activity frames remain present"
                );
            }
        }
    }

    #[tokio::test]
    async fn summary_eof_preserves_unfinished_reference_suffixes_and_unknown_references() {
        let session = RedactionSession::new();
        let reference = marker(&session, json!("secret-value"));
        let unknown = marker(&RedactionSession::new(), json!("other-secret"));
        for suffix in [
            "_",
            "__CROSSLANE_SEC",
            &reference[..reference.len() - 2],
            &unknown,
        ] {
            let content = format!("literal {suffix}");
            let frames = vec![frame("assistantResponseEvent", json!({"content":content}))];
            let bytes = restore_tool_response(response(&frames, 7), session.clone(), true)
                .bytes()
                .await
                .unwrap();
            let text: String = decode(&bytes)
                .into_iter()
                .filter_map(|event| match event {
                    KiroEvent::AssistantResponse { content } => Some(content),
                    _ => None,
                })
                .collect();
            assert_eq!(text, content);
        }
    }

    #[tokio::test]
    async fn held_tool_fragments_emit_activity_before_a_delayed_completion() {
        let session = RedactionSession::new();
        let reference = marker(&session, json!("secret-value"));
        let input = json!({"token":reference}).to_string();
        let split = input.find(&reference).unwrap() + reference.len() / 2;
        let first = tool("a", "http", &input[..split], false);
        let final_frame = tool("a", "", &input[split..], true);
        let (send, receive) = tokio::sync::oneshot::channel::<()>();
        let source = futures::stream::once(async move { Ok::<_, io::Error>(first) }).chain(
            futures::stream::once(async move {
                receive.await.expect("test releases final frame");
                Ok::<_, io::Error>(final_frame)
            }),
        );
        let response = Response::from(
            http::Response::builder()
                .status(200)
                .body(reqwest::Body::wrap_stream(source))
                .unwrap(),
        );
        let mut source = restore_tool_response(response, session, false).bytes_stream();
        let activity = tokio::time::timeout(std::time::Duration::from_millis(250), source.next())
            .await
            .expect("held input must not hide active upstream reads")
            .unwrap()
            .unwrap();
        assert_eq!(
            decode(&activity),
            vec![KiroEvent::AssistantResponse {
                content: String::new()
            }]
        );
        send.send(()).unwrap();
        let mut output = Vec::new();
        while let Some(chunk) = source.next().await {
            output.extend_from_slice(&chunk.unwrap());
        }
        let inputs = tool_inputs(&decode(&output));
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs["a"], json!({"token":"secret-value"}));
    }
}
