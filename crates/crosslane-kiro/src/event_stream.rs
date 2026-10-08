//! AWS Event Stream decoder with half-frame and sticky-frame support.

use std::collections::BTreeMap;
use std::io;

use bytes::{Buf, BytesMut};
use crc32fast::hash;
use serde_json::Value;
use tokio_util::codec::Decoder;

const MIN_FRAME_LENGTH: usize = 16;
const MAX_FRAME_LENGTH: usize = 50 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum KiroEvent {
    AssistantResponse {
        content: String,
    },
    ToolUse {
        id: String,
        name: String,
        input_delta: String,
        stop: bool,
    },
    Reasoning {
        content: String,
        signature: Option<String>,
        redacted_content: Option<String>,
    },
    Citations {
        citations: Vec<KiroCitation>,
    },
    MessageMetadata {
        usage: Value,
    },
    Usage {
        usage: Value,
    },
    Error {
        kind: String,
        message: String,
    },
    Other {
        event_type: String,
        payload: Value,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct KiroCitation {
    pub text: Option<String>,
    pub link: String,
    pub target: Value,
    pub kind: String,
}

#[derive(Debug, Default)]
pub struct EventStreamDecoder;

impl Decoder for EventStreamDecoder {
    type Item = KiroEvent;
    type Error = io::Error;

    fn decode(&mut self, source: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if source.len() < MIN_FRAME_LENGTH {
            return Ok(None);
        }
        let total_length =
            u32::from_be_bytes([source[0], source[1], source[2], source[3]]) as usize;
        let headers_length =
            u32::from_be_bytes([source[4], source[5], source[6], source[7]]) as usize;
        if !(MIN_FRAME_LENGTH..=MAX_FRAME_LENGTH).contains(&total_length) {
            return Err(invalid(format!(
                "invalid event stream length {total_length}"
            )));
        }
        if headers_length > total_length.saturating_sub(MIN_FRAME_LENGTH) {
            return Err(invalid(format!(
                "invalid headers length {headers_length} for frame {total_length}"
            )));
        }
        if source.len() < total_length {
            source.reserve(total_length - source.len());
            return Ok(None);
        }

        validate_crc(&source[..total_length])?;
        let headers = parse_headers(&source[12..12 + headers_length])?;
        let payload = &source[12 + headers_length..total_length - 4];
        let value: Value = serde_json::from_slice(payload)
            .map_err(|error| invalid(format!("invalid Kiro event JSON: {error}")))?;
        source.advance(total_length);
        Ok(Some(to_event(headers, value)))
    }

    fn decode_eof(&mut self, source: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if let Some(event) = self.decode(source)? {
            return Ok(Some(event));
        }
        if source.is_empty() {
            Ok(None)
        } else {
            Err(invalid(format!(
                "stream ended with incomplete event message ({} bytes remaining)",
                source.len()
            )))
        }
    }
}

fn validate_crc(frame: &[u8]) -> io::Result<()> {
    let prelude_expected = u32::from_be_bytes([frame[8], frame[9], frame[10], frame[11]]);
    if prelude_expected != 0 && hash(&frame[..8]) != prelude_expected {
        return Err(invalid("event stream prelude CRC mismatch"));
    }
    let crc_offset = frame.len() - 4;
    let message_expected = u32::from_be_bytes([
        frame[crc_offset],
        frame[crc_offset + 1],
        frame[crc_offset + 2],
        frame[crc_offset + 3],
    ]);
    if message_expected != 0 && hash(&frame[..frame.len() - 4]) != message_expected {
        return Err(invalid("event stream message CRC mismatch"));
    }
    Ok(())
}

fn parse_headers(bytes: &[u8]) -> io::Result<BTreeMap<String, String>> {
    let mut headers = BTreeMap::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let name_len = bytes[offset] as usize;
        offset += 1;
        if offset + name_len + 1 > bytes.len() {
            return Err(invalid("truncated event stream header name"));
        }
        let name = std::str::from_utf8(&bytes[offset..offset + name_len])
            .map_err(|_| invalid("event stream header name is not UTF-8"))?
            .to_string();
        offset += name_len;
        let kind = bytes[offset];
        offset += 1;
        match kind {
            7 => {
                if offset + 2 > bytes.len() {
                    return Err(invalid("truncated event stream string header"));
                }
                let length = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]) as usize;
                offset += 2;
                if offset + length > bytes.len() {
                    return Err(invalid("truncated event stream string value"));
                }
                let value = std::str::from_utf8(&bytes[offset..offset + length])
                    .map_err(|_| invalid("event stream header is not UTF-8"))?
                    .to_string();
                offset += length;
                headers.insert(name, value);
            }
            0 | 1 => {}
            2 => offset += 1,
            3 => offset += 2,
            4 => offset += 4,
            5 | 8 => offset += 8,
            6 => {
                if offset + 2 > bytes.len() {
                    return Err(invalid("truncated byte-array header"));
                }
                let length = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]) as usize;
                offset += 2 + length;
            }
            9 => offset += 16,
            _ => return Err(invalid(format!("unknown event stream header type {kind}"))),
        }
        if offset > bytes.len() {
            return Err(invalid("truncated event stream header value"));
        }
    }
    Ok(headers)
}

fn to_event(headers: BTreeMap<String, String>, value: Value) -> KiroEvent {
    let message_type = headers.get(":message-type").map(String::as_str);
    let exception_type = headers.get(":exception-type").cloned();
    let event_type = headers
        .get(":event-type")
        .cloned()
        .filter(|event_type| !event_type.is_empty())
        .or_else(|| detected_event_type(&value).map(str::to_string))
        .unwrap_or_default();
    if message_type == Some("exception") || exception_type.is_some() {
        return KiroEvent::Error {
            kind: exception_type.unwrap_or_else(|| event_type.clone()),
            message: value
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| value.get("Message").and_then(Value::as_str))
                .unwrap_or("Unknown stream error")
                .into(),
        };
    }
    if let Some(kind) = value
        .get("__type")
        .and_then(Value::as_str)
        .or_else(|| value.get("_type").and_then(Value::as_str))
        .filter(|kind| !kind.is_empty())
    {
        return KiroEvent::Error {
            kind: kind.into(),
            message: value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Unknown stream error")
                .into(),
        };
    }
    if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
        return KiroEvent::Error {
            kind: "upstream_error".into(),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| error.as_str())
                .unwrap_or("Unknown stream error")
                .into(),
        };
    }
    match event_type.as_str() {
        "assistantResponseEvent" => KiroEvent::AssistantResponse {
            content: nested_str(&value, "assistantResponseEvent", "content")
                .or_else(|| value.get("content").and_then(Value::as_str))
                .unwrap_or_default()
                .into(),
        },
        "toolUseEvent" => {
            let body = value.get("toolUseEvent").unwrap_or(&value);
            KiroEvent::ToolUse {
                id: body
                    .get("toolUseId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
                name: body
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
                input_delta: body
                    .get("input")
                    .or_else(|| body.get("inputDelta"))
                    .map(value_text)
                    .unwrap_or_default(),
                stop: body.get("stop").and_then(Value::as_bool).unwrap_or(false),
            }
        }
        "reasoningContentEvent" => {
            let body = value.get("reasoningContentEvent").unwrap_or(&value);
            KiroEvent::Reasoning {
                content: body
                    .get("text")
                    .and_then(Value::as_str)
                    .or_else(|| body.get("content").and_then(Value::as_str))
                    .unwrap_or_default()
                    .into(),
                signature: body
                    .get("signature")
                    .and_then(Value::as_str)
                    .filter(|signature| !signature.is_empty())
                    .map(str::to_owned),
                redacted_content: body
                    .get("redactedContent")
                    .and_then(Value::as_str)
                    .or_else(|| body.get("redacted_content").and_then(Value::as_str))
                    .filter(|content| !content.is_empty())
                    .map(str::to_owned),
            }
        }
        "messageMetadataEvent" | "metadataEvent" => KiroEvent::MessageMetadata { usage: value },
        "usageEvent" | "usage" | "meteringEvent" => KiroEvent::Usage { usage: value },
        "supplementaryWebLinksEvent" => citation_event(
            &value,
            "supplementaryWebLinksEvent",
            "supplementaryWebLinks",
            "web",
        ),
        "codeReferenceEvent" => citation_event(&value, "codeReferenceEvent", "references", "code"),
        "citationEvent" => citation_event(&value, "citationEvent", "citations", "document"),
        "invalidStateEvent" => {
            let body = value.get("invalidStateEvent").unwrap_or(&value);
            let reason = ["reason", "code", "state"]
                .into_iter()
                .find_map(|field| {
                    body.get(field)
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                })
                .unwrap_or("INVALID_STATE");
            let message = ["message", "detail"]
                .into_iter()
                .find_map(|field| {
                    body.get(field)
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                })
                .unwrap_or("Kiro reported an invalid conversation state");
            KiroEvent::Error {
                kind: reason.into(),
                message: message.into(),
            }
        }
        "followupPromptEvent" => {
            let body = value.get("followupPromptEvent").unwrap_or(&value);
            let prompt = body.get("followupPrompt").unwrap_or(body);
            let suggestion = prompt
                .get("content")
                .or_else(|| prompt.get("userIntent"))
                .and_then(Value::as_str)
                .map(str::to_string);
            match suggestion {
                Some(suggestion) => KiroEvent::AssistantResponse {
                    content: format!("\n\n**Suggested follow-up:** {suggestion}"),
                },
                None => KiroEvent::Other {
                    event_type,
                    payload: value,
                },
            }
        }
        _ => KiroEvent::Other {
            event_type,
            payload: value,
        },
    }
}

fn detected_event_type(value: &Value) -> Option<&'static str> {
    [
        "assistantResponseEvent",
        "toolUseEvent",
        "reasoningContentEvent",
        "messageMetadataEvent",
        "metadataEvent",
        "usageEvent",
        "meteringEvent",
        "supplementaryWebLinksEvent",
        "codeReferenceEvent",
        "followupPromptEvent",
        "citationEvent",
        "contextUsageEvent",
        "invalidStateEvent",
    ]
    .into_iter()
    .find(|key| value.get(key).is_some())
    .or_else(|| {
        value
            .get("citationLink")
            .is_some()
            .then_some("citationEvent")
    })
}

fn nested_str<'a>(value: &'a Value, wrapper: &str, key: &str) -> Option<&'a str> {
    value
        .get(wrapper)
        .unwrap_or(value)
        .get(key)
        .and_then(Value::as_str)
}

fn citation_event(value: &Value, wrapper: &str, list: &str, kind: &str) -> KiroEvent {
    let body = value.get(wrapper).unwrap_or(value);
    let listed = body
        .get(list)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let items = if listed.is_empty() {
        vec![body]
    } else {
        listed
    };
    let citations = items
        .into_iter()
        .filter_map(|item| {
            let link = ["citationLink", "url", "repository"]
                .into_iter()
                .find_map(|field| {
                    item.get(field)
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                })
                .map(str::to_owned);
            let text = ["citationText", "title", "licenseName", "snippet", "content"]
                .into_iter()
                .find_map(|field| {
                    item.get(field)
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                })
                .map(str::to_owned);
            if link.is_none() && text.is_none() {
                return None;
            }
            let target = item
                .get("target")
                .or_else(|| item.get("recommendationContentSpan"))
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            Some(KiroCitation {
                text,
                link: link.unwrap_or_default(),
                target,
                kind: kind.into(),
            })
        })
        .collect::<Vec<_>>();
    if citations.is_empty() {
        KiroEvent::Other {
            event_type: wrapper.into(),
            payload: value.clone(),
        }
    } else {
        KiroEvent::Citations { citations }
    }
}

fn value_text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(event_type: &str, payload: Value) -> Vec<u8> {
        let name = b":event-type";
        let value = event_type.as_bytes();
        let mut headers = vec![name.len() as u8];
        headers.extend_from_slice(name);
        headers.push(7);
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value);
        let payload = serde_json::to_vec(&payload).expect("payload");
        let total = 16 + headers.len() + payload.len();
        let mut output = Vec::with_capacity(total);
        output.extend_from_slice(&(total as u32).to_be_bytes());
        output.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        output.extend_from_slice(&0u32.to_be_bytes());
        output.extend_from_slice(&headers);
        output.extend_from_slice(&payload);
        output.extend_from_slice(&0u32.to_be_bytes());
        output
    }

    #[test]
    fn decodes_half_and_sticky_frames() {
        let first = frame(
            "assistantResponseEvent",
            serde_json::json!({"content":"hello"}),
        );
        let second = frame(
            "assistantResponseEvent",
            serde_json::json!({"content":" world"}),
        );
        let split = first.len() / 2;
        let mut buffer = BytesMut::from(&first[..split]);
        let mut decoder = EventStreamDecoder;
        assert_eq!(decoder.decode(&mut buffer).expect("half"), None);
        buffer.extend_from_slice(&first[split..]);
        buffer.extend_from_slice(&second);
        assert_eq!(
            decoder.decode(&mut buffer).expect("first"),
            Some(KiroEvent::AssistantResponse {
                content: "hello".into()
            })
        );
        assert_eq!(
            decoder.decode(&mut buffer).expect("second"),
            Some(KiroEvent::AssistantResponse {
                content: " world".into()
            })
        );
        assert!(buffer.is_empty());
    }

    #[test]
    fn detects_both_exception_shapes_and_error_field() {
        for payload in [
            serde_json::json!({"__type":"Throttle","message":"rate"}),
            serde_json::json!({"_type":"Legacy","message":"old"}),
            serde_json::json!({"error":{"message":"boom"}}),
        ] {
            let mut buffer = BytesMut::from(frame("assistantResponseEvent", payload).as_slice());
            assert!(matches!(
                EventStreamDecoder.decode(&mut buffer).expect("decode"),
                Some(KiroEvent::Error { .. })
            ));
        }
    }

    #[test]
    fn eof_rejects_incomplete_frame() {
        let bytes = frame(
            "assistantResponseEvent",
            serde_json::json!({"content":"hello"}),
        );
        let mut buffer = BytesMut::from(&bytes[..bytes.len() - 1]);
        assert!(EventStreamDecoder.decode_eof(&mut buffer).is_err());
    }

    #[test]
    fn detects_wrapper_events_without_headers_and_keeps_visible_supplements() {
        assert_eq!(
            to_event(
                BTreeMap::new(),
                serde_json::json!({"reasoningContentEvent":{"text":"thinking"}}),
            ),
            KiroEvent::Reasoning {
                content: "thinking".into(),
                signature: None,
                redacted_content: None,
            }
        );
        let links = to_event(
            BTreeMap::new(),
            serde_json::json!({"supplementaryWebLinksEvent":{"supplementaryWebLinks":[
                {"title":"Example","url":"https://example.com"}
            ]}}),
        );
        assert!(matches!(links, KiroEvent::Citations { citations }
            if citations.len() == 1
                && citations[0].text.as_deref() == Some("Example")
                && citations[0].link == "https://example.com"));

        let license = to_event(
            BTreeMap::new(),
            serde_json::json!({"codeReferenceEvent":{"references":[
                {"licenseName":"MIT"}
            ]}}),
        );
        assert!(matches!(license, KiroEvent::Citations { citations }
            if citations.len() == 1
                && citations[0].text.as_deref() == Some("MIT")
                && citations[0].link.is_empty()));

        let direct = to_event(
            BTreeMap::new(),
            serde_json::json!({
                "error":null,
                "citationLink":null,
                "url":"https://example.com/source",
                "citationText":null,
                "title":"Fallback title",
                "target":{"range":{"start":0,"end":6}}
            }),
        );
        assert!(matches!(direct, KiroEvent::Citations { citations }
            if citations.len() == 1
                && citations[0].text.as_deref() == Some("Fallback title")
                && citations[0].link == "https://example.com/source"));

        assert_eq!(
            to_event(
                BTreeMap::new(),
                serde_json::json!({"invalidStateEvent":{
                    "reason":"STALE_CONVERSATION",
                    "message":"Conversation state is no longer valid"
                }}),
            ),
            KiroEvent::Error {
                kind: "STALE_CONVERSATION".into(),
                message: "Conversation state is no longer valid".into(),
            }
        );
    }

    #[test]
    fn aws_exception_headers_become_classifiable_errors() {
        let headers = BTreeMap::from([
            (":message-type".into(), "exception".into()),
            (":exception-type".into(), "ThrottlingException".into()),
        ]);
        assert_eq!(
            to_event(headers, serde_json::json!({"message":"rate exceeded"})),
            KiroEvent::Error {
                kind: "ThrottlingException".into(),
                message: "rate exceeded".into()
            }
        );
    }
}
