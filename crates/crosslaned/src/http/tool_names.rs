use std::collections::HashMap;

use crosslane_kiro::KiroEvent;

use super::response::{DecodedResponse, OpenAiToolIdentity};

pub(super) const TOOL_NAME_MISMATCH_MESSAGE: &str =
    "upstream tool call name does not match the tools declared in this request";
pub(super) const TOOL_NAME_MISMATCH_CODE: &str = "upstream_tool_name_mismatch";

pub(super) struct ToolNameTrace<'a> {
    pub trace_id: &'a str,
    pub request_id: &'a str,
    pub path: &'a str,
    pub streaming: bool,
}

/// Keep validation tied to the originating request. A hash-looking name is
/// legal if the client actually declared it; never guess another tool by prefix.
pub(super) struct ClientToolNames {
    aliases: HashMap<String, String>,
    originals: HashMap<String, String>,
    restore_original: bool,
}

impl ClientToolNames {
    #[cfg(feature = "crs")]
    pub fn is_claude(&self) -> bool {
        self.restore_original
    }

    pub fn claude(names: &HashMap<String, String>) -> Self {
        Self::new(names.clone(), true)
    }

    pub fn openai(names: &HashMap<String, OpenAiToolIdentity>) -> Self {
        Self::new(
            names
                .iter()
                .map(|(alias, identity)| (alias.clone(), identity.name.clone()))
                .collect(),
            false,
        )
    }

    fn new(aliases: HashMap<String, String>, restore_original: bool) -> Self {
        let originals = aliases
            .iter()
            .map(|(alias, original)| (original.clone(), alias.clone()))
            .collect();
        Self {
            aliases,
            originals,
            restore_original,
        }
    }

    fn resolve<'a>(
        &'a self,
        name: &str,
        tool_use_id: &str,
        trace: &ToolNameTrace<'_>,
    ) -> Result<&'a str, &'static str> {
        let resolved = if let Some(pair) = self.aliases.get_key_value(name) {
            pair
        } else if let Some(alias) = self.originals.get(name) {
            tracing::debug!(
                event = "proxy.tool_name.mapping_miss",
                provider_id = "kiro",
                trace_id = trace.trace_id,
                request_id = trace.request_id,
                path = trace.path,
                streaming = trace.streaming,
                tool_use_id = %log_name(tool_use_id),
                upstream_tool_name = %log_name(name),
                declared_tool_count = self.originals.len(),
                resolution = "declared_original",
                "upstream returned a declared original name instead of its alias"
            );
            self.aliases.get_key_value(alias).expect("registered alias")
        } else {
            tracing::warn!(
                event = "proxy.tool_name.mapping_miss",
                provider_id = "kiro",
                trace_id = trace.trace_id,
                request_id = trace.request_id,
                path = trace.path,
                streaming = trace.streaming,
                tool_use_id = %log_name(tool_use_id),
                upstream_tool_name = %log_name(name),
                declared_tool_count = self.originals.len(),
                resolution = "rejected",
                "rejecting an upstream tool name absent from this request's tool catalog"
            );
            return Err(TOOL_NAME_MISMATCH_MESSAGE);
        };
        // OpenAI formatting still needs the upstream key to recover the tool's
        // function/custom shape. Claude formatting receives the original name.
        Ok(if self.restore_original {
            resolved.1
        } else {
            resolved.0
        })
    }

    pub fn restore_event(
        &self,
        event: &mut KiroEvent,
        trace: &ToolNameTrace<'_>,
    ) -> Result<(), &'static str> {
        if let KiroEvent::ToolUse { id, name, .. } = event {
            *name = self.resolve(name, id, trace)?.to_owned();
        }
        Ok(())
    }

    pub fn restore_decoded(
        &self,
        decoded: &mut DecodedResponse,
        trace: &ToolNameTrace<'_>,
    ) -> Result<(), &'static str> {
        // Validate the complete nonstreaming response before changing any name.
        let restored = decoded
            .tools
            .values()
            .map(|tool| self.resolve(&tool.name, &tool.id, trace).map(str::to_owned))
            .collect::<Result<Vec<_>, _>>()?;
        for (tool, name) in decoded.tools.values_mut().zip(restored) {
            tool.name = name;
        }
        Ok(())
    }

    pub fn validate_decoded(
        &self,
        decoded: &DecodedResponse,
        trace: &ToolNameTrace<'_>,
    ) -> Result<(), &'static str> {
        for tool in decoded.tools.values() {
            self.resolve(&tool.name, &tool.id, trace)?;
        }
        Ok(())
    }
}

fn log_name(name: &str) -> String {
    name.chars().take(128).collect()
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use super::*;

    fn trace() -> ToolNameTrace<'static> {
        ToolNameTrace {
            trace_id: "trace_tool_names",
            request_id: "request_tool_names",
            path: "/v1/messages",
            streaming: true,
        }
    }

    fn event(name: &str) -> KiroEvent {
        KiroEvent::ToolUse {
            id: "call_tool_names".into(),
            name: name.into(),
            input_delta: "{\"private_argument\":\"do-not-log\"}".into(),
            stop: true,
        }
    }

    #[test]
    fn restores_exact_aliases_and_accepts_only_declared_originals() {
        let original = "mcp__mcp-gateway__orch_mcp_server__orch_dingtalk_notification_send";
        let alias = crosslane_translate::tool_name(original);
        let names = ClientToolNames::claude(&HashMap::from([(alias.clone(), original.into())]));
        for name in [alias.as_str(), original] {
            let mut tool = event(name);
            names.restore_event(&mut tool, &trace()).unwrap();
            assert!(matches!(tool, KiroEvent::ToolUse { name, .. } if name == original));
        }
        for name in [
            "",
            "undeclared_tool",
            "mcp_mcp-gateway__orch_mcp_server__orch_dingtalk_no_447129d6e2a1",
        ] {
            assert_eq!(
                names.restore_event(&mut event(name), &trace()),
                Err(TOOL_NAME_MISMATCH_MESSAGE)
            );
        }
    }

    #[test]
    fn preserves_declared_hash_names_and_openai_custom_identities() {
        let declared_hash = "declared_123456789abc";
        let names = ClientToolNames::claude(&HashMap::from([(
            declared_hash.into(),
            declared_hash.into(),
        )]));
        let mut tool = event(declared_hash);
        names.restore_event(&mut tool, &trace()).unwrap();
        assert!(matches!(tool, KiroEvent::ToolUse { name, .. } if name == declared_hash));

        let original = "custom.tool";
        let alias = crosslane_translate::tool_name(original);
        let identities = HashMap::from([(
            alias.clone(),
            OpenAiToolIdentity {
                kind: "custom".into(),
                name: original.into(),
            },
        )]);
        let names = ClientToolNames::openai(&identities);
        let mut tool = event(original);
        names.restore_event(&mut tool, &trace()).unwrap();
        assert!(matches!(tool, KiroEvent::ToolUse { name, .. } if name == alias));
    }

    #[test]
    fn invalid_parallel_call_cannot_partially_restore_a_nonstreaming_response() {
        let original = "mcp__mcp-gateway__orch_mcp_server__orch_dingtalk_notification_send";
        let alias = crosslane_translate::tool_name(original);
        let names = ClientToolNames::claude(&HashMap::from([(alias.clone(), original.into())]));
        let mut decoded = DecodedResponse::default();
        decoded.push(event(&alias)).unwrap();
        let mut unknown = event("unknown_hash_123456789abc");
        if let KiroEvent::ToolUse { id, .. } = &mut unknown {
            *id = "second_call".into();
        }
        decoded.push(unknown).unwrap();
        assert_eq!(
            names.restore_decoded(&mut decoded, &trace()),
            Err(TOOL_NAME_MISMATCH_MESSAGE)
        );
        assert_eq!(decoded.tools["call_tool_names"].name, alias);
    }

    #[derive(Clone)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    impl Write for LogCapture {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn mapping_miss_logs_correlatable_bounded_names_without_arguments() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let capture = LogCapture(Arc::clone(&output));
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_ansi(false)
            .with_writer(move || capture.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let names = ClientToolNames::claude(&HashMap::new());
            assert!(names
                .restore_event(&mut event(&"x".repeat(1_024)), &trace())
                .is_err());
        });
        let log: serde_json::Value = serde_json::from_slice(&output.lock().unwrap()).unwrap();
        let fields = &log["fields"];
        assert_eq!(fields["event"], "proxy.tool_name.mapping_miss");
        assert_eq!(fields["trace_id"], "trace_tool_names");
        assert_eq!(fields["request_id"], "request_tool_names");
        assert_eq!(fields["tool_use_id"], "call_tool_names");
        assert_eq!(fields["resolution"], "rejected");
        assert_eq!(fields["upstream_tool_name"].as_str().unwrap().len(), 128);
        assert!(!log.to_string().contains("private_argument"));
        assert!(!log.to_string().contains("do-not-log"));
    }
}
