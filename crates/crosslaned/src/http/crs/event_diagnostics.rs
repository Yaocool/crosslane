//! Content-free observations of the same Kiro response used by the client.
//!
//! The shared decoder intentionally merges some native aliases. Keep decoded
//! kinds distinct from `Other` native event types instead of claiming that a
//! reconstructed name came from a wire header. No request or event content is
//! logged, and unrecognized upstream strings are represented only by hashes.

use std::cell::Cell;
use std::collections::BTreeMap;

use bytes::BytesMut;
use crosslane_kiro::{EventStreamDecoder, KiroError, KiroEvent};
use futures::StreamExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio_util::codec::Decoder;

use super::super::response::DecodedResponse;

const MAX_DISTINCT_VALUES: usize = 32;

#[derive(Default)]
struct Counts {
    values: BTreeMap<String, u64>,
    overflow_events: u64,
}

impl Counts {
    fn add(&mut self, value: String) {
        if let Some(count) = self.values.get_mut(&value) {
            *count = count.saturating_add(1);
        } else if self.values.len() < MAX_DISTINCT_VALUES {
            self.values.insert(value, 1);
        } else {
            self.overflow_events = self.overflow_events.saturating_add(1);
        }
    }

    fn snapshot(&self) -> Value {
        json!({"counts": self.values, "overflow_events": self.overflow_events})
    }
}

fn classified(value: &str, allowlist: &[&str]) -> String {
    if allowlist.contains(&value) {
        value.to_owned()
    } else {
        format!("sha256:{:x}", Sha256::digest(value.as_bytes()))
    }
}

fn native_other_type(value: &str) -> String {
    classified(
        value,
        &[
            "contextUsageEvent",
            "initial-response",
            "invalidStateEvent",
            "assistantResponseEvent",
            "toolUseEvent",
            "reasoningContentEvent",
            "metadataEvent",
            "messageMetadataEvent",
            "usageEvent",
            "usage",
            "meteringEvent",
            "supplementaryWebLinksEvent",
            "codeReferenceEvent",
            "citationEvent",
            "followupPromptEvent",
            "error",
            "throttlingError",
            "validationError",
            "serviceUnavailableError",
        ],
    )
}

fn stop_reason(value: &str) -> String {
    classified(
        value,
        &[
            "CONTENT_FILTERED",
            "END_TURN",
            "MAX_TOKENS",
            "STOP_SEQUENCE",
            "TOOL_USE",
            "GUARDRAIL_INTERVENED",
        ],
    )
}

fn refusal_category(value: &str) -> String {
    classified(value, &["CYBER", "OTHER", "NONE"])
}

fn exception_kind(value: &str) -> String {
    classified(
        value,
        &[
            "ThrottlingException",
            "ValidationException",
            "InternalServerException",
            "ServiceUnavailableException",
            "AccessDeniedException",
            "UnauthorizedException",
            "ModelNotReadyException",
            "error",
            "throttlingError",
            "validationError",
            "serviceUnavailableError",
            "upstream_error",
            "INVALID_STATE",
        ],
    )
}

pub(crate) struct KiroEventDiagnostics<'a> {
    trace_id: &'a str,
    request_id: &'a str,
    endpoint: &'static str,
    streaming: bool,
    round: u64,
    events: u64,
    decoded_kinds: Counts,
    native_other_types: Counts,
    metadata_stop_reasons: Counts,
    refusal_categories: Counts,
    exception_kinds: Counts,
    metadata_refusals: u64,
    assistant_bytes: u64,
    tool_input_bytes: u64,
    reasoning_bytes: u64,
    other_object_fields: u64,
    finished: Cell<bool>,
    #[cfg(test)]
    summary_emissions: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl<'a> KiroEventDiagnostics<'a> {
    pub(crate) fn new(
        trace_id: &'a str,
        request_id: &'a str,
        endpoint: &str,
        streaming: bool,
        round: u64,
    ) -> Self {
        Self {
            trace_id,
            request_id,
            endpoint: match endpoint {
                "AmazonQ" => "AmazonQ",
                "CodeWhisperer" => "CodeWhisperer",
                "KiroRuntime" => "KiroRuntime",
                _ => "other",
            },
            streaming,
            round,
            events: 0,
            decoded_kinds: Counts::default(),
            native_other_types: Counts::default(),
            metadata_stop_reasons: Counts::default(),
            refusal_categories: Counts::default(),
            exception_kinds: Counts::default(),
            metadata_refusals: 0,
            assistant_bytes: 0,
            tool_input_bytes: 0,
            reasoning_bytes: 0,
            other_object_fields: 0,
            finished: Cell::new(false),
            #[cfg(test)]
            summary_emissions: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Observe before filters while retaining the original event unchanged.
    pub(crate) fn observe(&mut self, event: &KiroEvent) {
        self.events = self.events.saturating_add(1);
        let kind = match event {
            KiroEvent::AssistantResponse { content } => {
                self.assistant_bytes = self.assistant_bytes.saturating_add(content.len() as u64);
                "assistant_response"
            }
            KiroEvent::ToolUse { input_delta, .. } => {
                self.tool_input_bytes = self
                    .tool_input_bytes
                    .saturating_add(input_delta.len() as u64);
                "tool_use"
            }
            KiroEvent::Reasoning { content, .. } => {
                self.reasoning_bytes = self.reasoning_bytes.saturating_add(content.len() as u64);
                "reasoning"
            }
            KiroEvent::Citations { .. } => "citations",
            KiroEvent::MessageMetadata { usage } => {
                let metadata = usage
                    .get("metadataEvent")
                    .or_else(|| usage.get("messageMetadataEvent"))
                    .unwrap_or(usage);
                if let Some(reason) = metadata.get("stopReason").and_then(Value::as_str) {
                    self.metadata_stop_reasons.add(stop_reason(reason));
                }
                if let Some(refusal) = metadata.pointer("/stopDetails/refusal") {
                    if refusal.is_object() {
                        self.metadata_refusals = self.metadata_refusals.saturating_add(1);
                        if let Some(category) = refusal.get("category").and_then(Value::as_str) {
                            self.refusal_categories.add(refusal_category(category));
                        }
                    }
                }
                "message_metadata"
            }
            KiroEvent::Usage { .. } => "usage",
            KiroEvent::Error { kind, .. } => {
                self.exception_kinds.add(exception_kind(kind));
                "error"
            }
            KiroEvent::Other {
                event_type,
                payload,
            } => {
                self.native_other_types.add(native_other_type(event_type));
                self.other_object_fields = self
                    .other_object_fields
                    .saturating_add(payload.as_object().map_or(0, |object| object.len()) as u64);
                "other"
            }
        };
        self.decoded_kinds.add(kind.to_owned());
        tracing::trace!(
            event = "crs.kiro.event.observed",
            trace_id = self.trace_id,
            request_id = self.request_id,
            endpoint = self.endpoint,
            streaming = self.streaming,
            round = self.round,
            event_index = self.events,
            decoded_kind = kind,
            "observed Kiro event structure before response filters"
        );
        if matches!(event, KiroEvent::MessageMetadata { .. })
            && (self.metadata_refusals > 0
                || self
                    .metadata_stop_reasons
                    .values
                    .contains_key("CONTENT_FILTERED"))
        {
            self.log_summary(None, "metadata_stop");
        }
    }

    fn snapshot(&self, decoded: Option<&DecodedResponse>, outcome: &'static str) -> Value {
        json!({
            "outcome": outcome,
            "events": self.events,
            "decoded_kinds": self.decoded_kinds.snapshot(),
            "native_other_types": self.native_other_types.snapshot(),
            "metadata_stop_reasons": self.metadata_stop_reasons.snapshot(),
            "refusal_categories": self.refusal_categories.snapshot(),
            "exception_kinds": self.exception_kinds.snapshot(),
            "metadata_refusals": self.metadata_refusals,
            "assistant_bytes": self.assistant_bytes,
            "tool_input_bytes": self.tool_input_bytes,
            "reasoning_bytes": self.reasoning_bytes,
            "other_object_fields": self.other_object_fields,
            "filtered_output": decoded.map(|decoded| json!({
                "text_bytes": decoded.text.len(),
                "tool_count": decoded.tools.len(),
                "reasoning_bytes": decoded.reasoning.len(),
                "citation_count": decoded.citations.len(),
                "has_stop_reason": decoded.stop_reason.is_some(),
                "has_stop_sequence": decoded.stop_sequence.is_some(),
            })),
        })
    }

    pub(crate) fn log_summary(&self, decoded: Option<&DecodedResponse>, outcome: &'static str) {
        if self.finished.get() {
            return;
        }
        // A metadata stop is an intermediate observation: later filters can
        // still fail, and cancellation can drop the response without EOF.
        if outcome != "metadata_stop" {
            self.finished.set(true);
        }
        #[cfg(test)]
        self.summary_emissions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let summary = self.snapshot(decoded, outcome);
        if outcome == "complete" {
            tracing::info!(
                event = "crs.kiro.round.observed",
                trace_id = self.trace_id,
                request_id = self.request_id,
                endpoint = self.endpoint,
                streaming = self.streaming,
                round = self.round,
                diagnostics = %summary,
                "Kiro response round structure observed"
            );
        } else {
            tracing::warn!(
                event = "crs.kiro.round.observed",
                trace_id = self.trace_id,
                request_id = self.request_id,
                endpoint = self.endpoint,
                streaming = self.streaming,
                round = self.round,
                diagnostics = %summary,
                "Kiro response round structure observed"
            );
        }
    }
}

impl Drop for KiroEventDiagnostics<'_> {
    fn drop(&mut self) {
        if !self.finished.get() {
            // A dropped generator or an early validation return has no
            // reliable cancellation/EOF reason. Report only what was observed.
            self.log_summary(None, "unfinished");
        }
    }
}

/// Same decoder/error contract as KiroClient::collect_events, with observations
/// before errors abort collection. No response or request value is rewritten.
pub(crate) async fn collect_events(
    response: reqwest::Response,
    diagnostics: &mut KiroEventDiagnostics<'_>,
) -> Result<Vec<KiroEvent>, KiroError> {
    let mut source = response.bytes_stream();
    let mut buffer = BytesMut::new();
    let mut decoder = EventStreamDecoder;
    let mut output = Vec::new();
    while let Some(chunk) = source.next().await {
        let chunk = chunk.map_err(|error| {
            diagnostics.log_summary(None, "http_body_error");
            transport_error(error)
        })?;
        buffer.extend_from_slice(&chunk);
        while let Some(event) = decoder.decode(&mut buffer).map_err(|error| {
            diagnostics.log_summary(None, "event_stream_decode_error");
            transport_error(error)
        })? {
            collect_event(event, diagnostics, &mut output)?;
        }
    }
    while let Some(event) = decoder.decode_eof(&mut buffer).map_err(|error| {
        diagnostics.log_summary(None, "event_stream_eof_error");
        transport_error(error)
    })? {
        collect_event(event, diagnostics, &mut output)?;
    }
    Ok(output)
}

fn collect_event(
    event: KiroEvent,
    diagnostics: &mut KiroEventDiagnostics<'_>,
    output: &mut Vec<KiroEvent>,
) -> Result<(), KiroError> {
    diagnostics.observe(&event);
    if let KiroEvent::Error { kind, message } = &event {
        diagnostics.log_summary(None, "upstream_exception");
        return Err(KiroError {
            status: None,
            endpoint: "event-stream".into(),
            message: format!("{kind}: {message}"),
        });
    }
    output.push(event);
    Ok(())
}

fn transport_error(error: impl std::fmt::Display) -> KiroError {
    KiroError {
        status: None,
        endpoint: "transport".into(),
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use crosslane_core::config::UpstreamConfig;
    use crosslane_kiro::endpoint::EndpointOverrides;
    use crosslane_kiro::KiroClient;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn diagnostics() -> KiroEventDiagnostics<'static> {
        KiroEventDiagnostics::new("trace_owned", "request_owned", "AmazonQ", true, 1)
    }

    #[test]
    fn observations_keep_events_and_record_only_counts_or_allowlisted_enums() {
        let mut diagnostics = diagnostics();
        let events = vec![
            KiroEvent::AssistantResponse {
                content: "private response content".into(),
            },
            KiroEvent::ToolUse {
                id: "private-call-id".into(),
                name: "private-tool-name".into(),
                input_delta: r#"{"path":"/private/中文😀.txt"}"#.into(),
                stop: true,
            },
            KiroEvent::MessageMetadata {
                usage: json!({
                    "stopReason": "CONTENT_FILTERED",
                    "stopDetails": {"refusal": {
                        "category": "CYBER", "explanation": "private refusal explanation"
                    }}
                }),
            },
            KiroEvent::Other {
                event_type: "contextUsageEvent".into(),
                payload: json!({"contextUsagePercentage": 0.5}),
            },
        ];
        let unchanged = events.clone();
        for event in &events {
            diagnostics.observe(event);
        }
        assert_eq!(events, unchanged);
        let snapshot = diagnostics.snapshot(None, "empty_retry");
        assert_eq!(snapshot["events"], 4);
        assert_eq!(snapshot["decoded_kinds"]["counts"]["assistant_response"], 1);
        assert_eq!(snapshot["decoded_kinds"]["counts"]["tool_use"], 1);
        assert_eq!(
            snapshot["native_other_types"]["counts"]["contextUsageEvent"],
            1
        );
        assert_eq!(
            snapshot["metadata_stop_reasons"]["counts"]["CONTENT_FILTERED"],
            1
        );
        assert_eq!(snapshot["refusal_categories"]["counts"]["CYBER"], 1);
        let serialized = snapshot.to_string();
        for secret in [
            "private response content",
            "private-call-id",
            "private-tool-name",
            "/private/",
            "private refusal explanation",
        ] {
            assert!(!serialized.contains(secret), "unexpected content: {secret}");
        }
    }

    #[test]
    fn arbitrary_types_metadata_and_exception_values_are_only_bounded_hashes() {
        let mut diagnostics = diagnostics();
        for index in 0..100 {
            diagnostics.observe(&KiroEvent::Other {
                event_type: format!("private-native-type-{index}"),
                payload: json!({"private payload key": "private payload text"}),
            });
        }
        diagnostics.observe(&KiroEvent::MessageMetadata {
            usage: json!({"metadataEvent": {
                "stopReason": "private-stop-reason",
                "stopDetails": {"refusal": {
                    "category": "private-category", "explanation": "private-explanation"
                }}
            }}),
        });
        diagnostics.observe(&KiroEvent::Error {
            kind: "private-exception-kind".into(),
            message: "private-exception-message".into(),
        });
        let snapshot = diagnostics.snapshot(None, "failed");
        assert_eq!(snapshot["events"], 102);
        assert_eq!(snapshot["native_other_types"]["overflow_events"], 68);
        let native_types = snapshot["native_other_types"]["counts"]
            .as_object()
            .unwrap();
        assert_eq!(native_types.len(), MAX_DISTINCT_VALUES);
        assert!(native_types
            .keys()
            .all(|key| key.starts_with("sha256:") && key.len() == 71));
        assert!(!snapshot.to_string().contains("private"));
    }

    #[test]
    fn filtered_sizes_do_not_capture_content_or_terminal_text() {
        let diagnostics = diagnostics();
        let decoded = DecodedResponse {
            text: "private filtered text".into(),
            reasoning: "private reasoning".into(),
            stop_reason: Some("private terminal reason".into()),
            ..Default::default()
        };
        let snapshot = diagnostics.snapshot(Some(&decoded), "complete");
        assert_eq!(
            snapshot["filtered_output"]["text_bytes"],
            decoded.text.len()
        );
        assert_eq!(snapshot["filtered_output"]["has_stop_reason"], true);
        assert!(!snapshot.to_string().contains("private"));
    }

    #[test]
    fn drop_and_early_return_emit_unfinished_once() {
        use std::sync::atomic::Ordering;

        let mut observed = diagnostics();
        observed.observe(&KiroEvent::Other {
            event_type: "contextUsageEvent".into(),
            payload: json!({"contextUsagePercentage": 0.5}),
        });
        let emissions = observed.summary_emissions.clone();
        drop(observed);
        assert_eq!(emissions.load(Ordering::Relaxed), 1);

        let mut early_emissions = None;
        let result: Result<(), ()> = (|| {
            let observed = diagnostics();
            early_emissions = Some(observed.summary_emissions.clone());
            Err(())?;
            Ok(())
        })();
        assert!(result.is_err());
        assert_eq!(early_emissions.unwrap().load(Ordering::Relaxed), 1);
    }

    #[test]
    fn completed_and_decode_error_summaries_suppress_drop_duplicates() {
        use std::sync::atomic::Ordering;

        for outcome in [
            "complete",
            "empty_retry",
            "empty_fail",
            "event_stream_decode_error",
        ] {
            let observed = diagnostics();
            let emissions = observed.summary_emissions.clone();
            observed.log_summary(None, outcome);
            observed.log_summary(None, outcome);
            drop(observed);
            assert_eq!(emissions.load(Ordering::Relaxed), 1, "{outcome}");
        }
    }

    #[test]
    fn metadata_refusal_is_intermediate_and_does_not_hide_unfinished_round() {
        use std::sync::atomic::Ordering;

        let mut observed = diagnostics();
        observed.observe(&KiroEvent::MessageMetadata {
            usage: json!({"stopReason":"CONTENT_FILTERED", "stopDetails":{"refusal":{"category":"CYBER"}}}),
        });
        let emissions = observed.summary_emissions.clone();
        assert!(!observed.finished.get());
        assert_eq!(emissions.load(Ordering::Relaxed), 1);
        drop(observed);
        assert_eq!(emissions.load(Ordering::Relaxed), 2);
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
            }
        }
        !crc
    }

    fn frame(event_type: &str, payload: Value, exception: Option<&str>) -> Vec<u8> {
        let mut headers = Vec::new();
        let mut add_header = |name: &str, value: &str| {
            headers.push(name.len() as u8);
            headers.extend_from_slice(name.as_bytes());
            headers.push(7);
            headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
            headers.extend_from_slice(value.as_bytes());
        };
        add_header(":event-type", event_type);
        add_header(
            ":message-type",
            if exception.is_some() {
                "exception"
            } else {
                "event"
            },
        );
        if let Some(exception) = exception {
            add_header(":exception-type", exception);
        }
        let payload = serde_json::to_vec(&payload).unwrap();
        let mut frame = Vec::new();
        frame.extend_from_slice(&((16 + headers.len() + payload.len()) as u32).to_be_bytes());
        frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        frame.extend_from_slice(&crc32(&frame).to_be_bytes());
        frame.extend_from_slice(&headers);
        frame.extend_from_slice(&payload);
        frame.extend_from_slice(&crc32(&frame).to_be_bytes());
        frame
    }

    #[tokio::test]
    async fn nonstream_observer_matches_original_collection_for_eof_crc_and_exceptions() {
        let context = frame(
            "contextUsageEvent",
            json!({"contextUsagePercentage":0.5}),
            None,
        );
        let assistant = frame(
            "assistantResponseEvent",
            json!({"content":"owned answer"}),
            None,
        );
        let mut valid = context.clone();
        valid.extend_from_slice(&assistant);
        let mut truncated = context.clone();
        truncated.extend_from_slice(&assistant[..assistant.len() - 3]);
        let mut bad_crc = context.clone();
        let mut corrupt_frame = assistant.clone();
        let crc_index = corrupt_frame.len() - 1;
        corrupt_frame[crc_index] ^= 1;
        bad_crc.extend_from_slice(&corrupt_frame);
        let mut exception = context.clone();
        exception.extend_from_slice(&frame(
            "ThrottlingException",
            json!({"message":"owned rejection", "reason":"USER_REQUEST_RATE_EXCEEDED"}),
            Some("ThrottlingException"),
        ));
        let metadata = frame(
            "metadataEvent",
            json!({"stopReason":"CONTENT_FILTERED","stopDetails":{"refusal":{"category":"CYBER"}}}),
            None,
        );
        let client =
            KiroClient::new(UpstreamConfig::default(), EndpointOverrides::default()).unwrap();
        let server = MockServer::start().await;
        for (index, (body, expected_events)) in [
            (Vec::new(), 0),
            (valid, 2),
            (context, 1),
            (truncated, 1),
            (bad_crc, 1),
            (exception, 2),
            (metadata, 1),
        ]
        .into_iter()
        .enumerate()
        {
            let resource = format!("/case-{index}");
            Mock::given(method("GET"))
                .and(path(resource.clone()))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
                .mount(&server)
                .await;
            let url = format!("{}{resource}", server.uri());
            let original = client
                .collect_events(reqwest::get(&url).await.unwrap())
                .await;
            let mut diagnostics = diagnostics();
            let observed =
                collect_events(reqwest::get(&url).await.unwrap(), &mut diagnostics).await;
            match (original, observed) {
                (Ok(original), Ok(observed)) => assert_eq!(observed, original),
                (Err(original), Err(observed)) => {
                    assert_eq!(observed.status, original.status);
                    assert_eq!(observed.endpoint, original.endpoint);
                    assert_eq!(observed.message, original.message);
                }
                _ => panic!("observer changed collection outcome for case {index}"),
            }
            assert_eq!(diagnostics.events, expected_events);
        }
    }
}
