//! Keep native Responses identities stable without translating their payloads.

use std::collections::HashMap;

use bytes::Bytes;
use crosslane_provider::{
    ProviderError, ProviderErrorKind, ProviderResponse, ProviderResponseBody,
};
use futures::StreamExt;
use http::StatusCode;
use serde_json::Value;

const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

pub(super) fn normalize_response(
    mut response: ProviderResponse,
    streaming: bool,
) -> ProviderResponse {
    if !streaming
        || !response.status.is_success()
        || !response
            .headers
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"))
    {
        return response;
    }
    let mut upstream = match response.body {
        ProviderResponseBody::Stream(stream) => stream,
        ProviderResponseBody::Full(bytes) => Box::pin(futures::stream::once(async { Ok(bytes) })),
    };
    response.headers.remove("content-length");
    response.headers.remove("content-encoding");
    let stream = async_stream::try_stream! {
        let mut identities = StreamIdentities::default();
        while let Some(chunk) = upstream.next().await {
            for frame in identities.push(&chunk?)? {
                yield frame;
            }
        }
        if !identities.pending.is_empty() {
            // Preserve an unterminated final frame; never manufacture success
            // when the upstream ends or fails before its terminal event.
            let frame = std::mem::take(&mut identities.pending);
            yield identities.frame(frame);
        }
    };
    response.body = ProviderResponseBody::Stream(Box::pin(stream));
    response
}

#[derive(Default)]
struct StreamIdentities {
    pending: Vec<u8>,
    line_start: usize,
    response_id: Option<String>,
    item_ids: HashMap<u64, String>,
}

impl StreamIdentities {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<Bytes>, ProviderError> {
        let mut frames = Vec::new();
        for &byte in bytes {
            self.pending.push(byte);
            if self.pending.len() > MAX_FRAME_BYTES {
                return Err(ProviderError::new(
                    ProviderErrorKind::Upstream,
                    StatusCode::BAD_GATEWAY,
                    "Copilot Responses SSE event exceeds the adapter limit",
                ));
            }
            if byte != b'\n' {
                continue;
            }
            let mut end = self.pending.len() - 1;
            if end > self.line_start && self.pending[end - 1] == b'\r' {
                end -= 1;
            }
            if end == self.line_start {
                let frame = std::mem::take(&mut self.pending);
                self.line_start = 0;
                frames.push(self.frame(frame));
            } else {
                self.line_start = self.pending.len();
            }
        }
        Ok(frames)
    }

    fn frame(&mut self, frame: Vec<u8>) -> Bytes {
        let Ok(text) = std::str::from_utf8(&frame) else {
            return Bytes::from(frame);
        };
        let data = text
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(|line| line.strip_prefix(' ').unwrap_or(line))
            .collect::<Vec<_>>()
            .join("\n");
        let Ok(mut value) = serde_json::from_str::<Value>(&data) else {
            return Bytes::from(frame);
        };
        let kind = value["type"].as_str().or_else(|| {
            text.lines()
                .find_map(|line| line.strip_prefix("event:").map(str::trim))
        });
        if !kind.is_some_and(|kind| kind.starts_with("response.")) || !self.normalize(&mut value) {
            return Bytes::from(frame);
        }
        // Keep event names, comments, id/retry fields and framing. Coalesce only
        // the JSON data lines, so CRLF and multiline SSE remain well formed.
        let data = value.to_string();
        let mut output = Vec::with_capacity(frame.len());
        let mut emitted = false;
        for line in frame.split_inclusive(|byte| *byte == b'\n') {
            if line.starts_with(b"data:") {
                if !emitted {
                    output.extend_from_slice(b"data: ");
                    output.extend_from_slice(data.as_bytes());
                    if line.ends_with(b"\r\n") {
                        output.extend_from_slice(b"\r\n");
                    } else if line.ends_with(b"\n") {
                        output.push(b'\n');
                    }
                    emitted = true;
                }
            } else {
                output.extend_from_slice(line);
            }
        }
        Bytes::from(output)
    }

    fn normalize(&mut self, event: &mut Value) -> bool {
        let mut changed = false;
        if let Some(response) = event.get_mut("response") {
            changed |= stabilize(response.get_mut("id"), &mut self.response_id);
            if let Some(items) = response.get_mut("output").and_then(Value::as_array_mut) {
                for (index, item) in items.iter_mut().enumerate() {
                    changed |= self.item_id(index as u64, item.get_mut("id"));
                }
            }
        }
        changed |= stabilize(event.get_mut("response_id"), &mut self.response_id);
        if let Some(index) = event["output_index"].as_u64() {
            changed |= self.item_id(index, event.pointer_mut("/item/id"));
            changed |= self.item_id(index, event.get_mut("item_id"));
        }
        changed
    }

    fn item_id(&mut self, index: u64, field: Option<&mut Value>) -> bool {
        let Some(field) = field else {
            return false;
        };
        let Some(id) = field.as_str().filter(|id| !id.is_empty()) else {
            return false;
        };
        // Each output index owns one item. Copilot's opaque encrypted IDs can
        // change on every serialization, including the completed snapshot.
        // Retain the first upstream handle instead of inventing a new one.
        let canonical = self.item_ids.entry(index).or_insert_with(|| id.to_owned());
        if id == canonical.as_str() {
            return false;
        }
        *field = Value::String(canonical.clone());
        true
    }
}

fn stabilize(field: Option<&mut Value>, canonical: &mut Option<String>) -> bool {
    let Some(field) = field else {
        return false;
    };
    let Some(id) = field.as_str().filter(|id| !id.is_empty()) else {
        return false;
    };
    let canonical = canonical.get_or_insert_with(|| id.to_owned());
    if id == canonical.as_str() {
        return false;
    }
    *field = Value::String(canonical.clone());
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn events(frames: &[Bytes]) -> Vec<Value> {
        frames
            .iter()
            .filter_map(|frame| std::str::from_utf8(frame).ok())
            .flat_map(str::lines)
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str(data).ok())
            .collect()
    }

    #[test]
    fn fragmented_native_events_reconcile_to_one_message_and_keep_extensions() {
        let part = json!({"type":"output_text","text":"你好你好","annotations":[{"type":"url_citation","url":"https://example.com"}]});
        let upstream = [
            json!({"type":"response.created","response":{"id":"response-first","output":[]},"sequence_number":0}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"id":"message-first","type":"message","phase":"final_answer","content":[]},"sequence_number":1}),
            json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"message-delta-1","delta":"你好","sequence_number":2}),
            json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"message-delta-2","delta":"你好","sequence_number":3}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"id":"message-done","type":"message","phase":"final_answer","content":[part]},"sequence_number":4}),
            json!({"type":"response.completed","response":{"id":"response-last","output":[{"id":"message-final","type":"message","phase":"final_answer","content":[part]}],"future_extension":{"keep":true}},"copilot_usage":{"total_nano_aiu":7},"sequence_number":5}),
        ];
        let wire = upstream
            .iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap()
                )
            })
            .collect::<String>();
        let mut identities = StreamIdentities::default();
        let mut frames = Vec::new();
        for byte in wire.as_bytes() {
            frames.extend(identities.push(&[*byte]).unwrap());
        }
        let values = events(&frames);
        // This consumer reconciles deltas with completed messages by item ID.
        // The intentionally repeated text must remain, without a second item.
        let mut messages = HashMap::<String, String>::new();
        for value in &values {
            if value["type"] == "response.output_text.delta" {
                messages
                    .entry(value["item_id"].as_str().unwrap().into())
                    .or_default()
                    .push_str(value["delta"].as_str().unwrap());
            } else if value["type"] == "response.output_item.done" {
                messages.insert(
                    value["item"]["id"].as_str().unwrap().into(),
                    value["item"]["content"][0]["text"].as_str().unwrap().into(),
                );
            }
        }
        assert_eq!(messages.len(), 1);
        assert_eq!(messages["message-first"], "你好你好");
        for (index, (actual, expected)) in values.iter().zip(upstream).enumerate() {
            let mut expected = expected;
            if let Some(field) = expected.get_mut("item_id") {
                *field = json!("message-first");
            }
            if let Some(item) = expected.get_mut("item") {
                item["id"] = json!("message-first");
            }
            if let Some(response) = expected.get_mut("response") {
                response["id"] = json!("response-first");
                if let Some(item) = response["output"].as_array_mut().unwrap().first_mut() {
                    item["id"] = json!("message-first");
                }
            }
            assert_eq!(actual, &expected, "event {index}");
        }
    }

    #[test]
    fn output_indices_keep_reasoning_messages_and_parallel_calls_distinct() {
        let mut identities = StreamIdentities::default();
        for (index, kind) in [
            (0, "reasoning"),
            (1, "message"),
            (2, "function_call"),
            (3, "function_call"),
        ] {
            let id = format!("first-{index}");
            let event = json!({"type":"response.output_item.added","output_index":index,"item":{"id":id,"type":kind,"call_id":format!("call-{index}"),"name":"echo","phase":"commentary"}});
            identities.normalize(&mut event.clone());
            let mut delta = json!({"type":"response.function_call_arguments.delta","output_index":index,"item_id":format!("delta-{index}"),"delta":"{}","call_id":format!("call-{index}")});
            assert!(identities.normalize(&mut delta));
            assert_eq!(delta["item_id"], id);
            assert_eq!(delta["call_id"], format!("call-{index}"));
        }
        let mut completed = json!({"type":"response.incomplete","response":{"id":"response","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[
            {"id":"last-0","type":"reasoning","encrypted_content":"private"},
            {"id":"last-1","type":"message","phase":"commentary","content":[{"type":"refusal","refusal":"no"}]},
            {"id":"last-2","type":"function_call","call_id":"call-2","arguments":"{}"},
            {"id":"last-3","type":"function_call","call_id":"call-3","arguments":"{"}
        ]}});
        assert!(identities.normalize(&mut completed));
        for index in 0..4 {
            assert_eq!(
                completed["response"]["output"][index]["id"],
                format!("first-{index}")
            );
        }
        assert_eq!(
            completed["response"]["output"][0]["encrypted_content"],
            "private"
        );
        assert_eq!(completed["response"]["output"][1]["phase"], "commentary");
        assert_eq!(completed["response"]["output"][2]["call_id"], "call-2");
        assert_eq!(completed["response"]["output"][3]["arguments"], "{");
        assert_eq!(
            completed["response"]["incomplete_details"]["reason"],
            "max_output_tokens"
        );
    }

    #[test]
    fn crlf_multiline_and_non_json_frames_preserve_sse_fields() {
        let mut identities = StreamIdentities::default();
        let untouched = b": keepalive\r\n\r\nevent: copilot_usage\ndata: {\"copilot_usage\":{\"total_nano_aiu\":7}}\n\ndata: [DONE]\n\ndata: not-json\n\n";
        assert_eq!(identities.push(untouched).unwrap().concat(), untouched);
        identities.item_ids.insert(0, "first".into());
        let wire = b": comment\r\nid: sse-id\r\nretry: 1000\r\nevent: response.output_text.delta\r\ndata: {\"output_index\":0,\r\ndata: \"item_id\":\"later\",\"delta\":\"hi\"}\r\n\r\n";
        let frames = identities.push(wire).unwrap();
        let text = std::str::from_utf8(&frames[0]).unwrap();
        assert!(text.starts_with(
            ": comment\r\nid: sse-id\r\nretry: 1000\r\nevent: response.output_text.delta\r\n"
        ));
        assert!(text.ends_with("\r\n\r\n"));
        assert_eq!(events(&frames)[0]["item_id"], "first");
        assert_eq!(events(&frames)[0]["delta"], "hi");
    }

    #[tokio::test]
    async fn interrupted_streams_preserve_transport_errors_without_a_success_event() {
        let failure = ProviderError::new(
            ProviderErrorKind::Transport,
            StatusCode::BAD_GATEWAY,
            "disconnected",
        );
        let body = futures::stream::iter([
            Ok(Bytes::from_static(
                b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"first\"}}\n\n",
            )),
            Err(failure),
        ]);
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "content-type",
            http::HeaderValue::from_static("text/event-stream"),
        );
        let response = normalize_response(
            ProviderResponse {
                status: StatusCode::OK,
                headers,
                body: ProviderResponseBody::Stream(Box::pin(body)),
                account_id: None,
                upstream_request_id: None,
            },
            true,
        );
        let ProviderResponseBody::Stream(mut stream) = response.body else {
            panic!("stream expected");
        };
        assert_eq!(
            events(&[stream.next().await.unwrap().unwrap()])[0]["type"],
            "response.created"
        );
        assert_eq!(
            stream.next().await.unwrap().unwrap_err().message,
            "disconnected"
        );
        assert!(stream.next().await.is_none());
    }
}
