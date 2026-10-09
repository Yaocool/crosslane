//! Bounded client telemetry parsing before provider routing/normalization.

use axum::http::HeaderMap;
use crosslane_core::client_context::{ClientContext, ClientIdentitySource, ClientKind};
use serde::Deserialize;
use serde_json::{value::RawValue, Value};
use sha2::{Digest, Sha256};

const MAX_METADATA_BYTES: usize = 8 * 1024;
const MAX_USER_ID_BYTES: usize = 4 * 1024;
const MAX_IDENTIFIER_BYTES: usize = 256;

/// Unknown fields (including messages/input/tools) are skipped without
/// allocating copies of the prompt. Bad telemetry never rejects a request.
#[derive(Default, Deserialize)]
struct Envelope<'a> {
    #[serde(borrow)]
    metadata: Option<&'a RawValue>,
    #[serde(borrow)]
    client_metadata: Option<&'a RawValue>,
    #[serde(borrow)]
    previous_response_id: Option<&'a RawValue>,
    #[serde(borrow)]
    store: Option<&'a RawValue>,
}

fn identifier(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()
        && value.len() <= MAX_IDENTIFIER_BYTES
        && !value.chars().any(char::is_control))
    .then_some(value)
}

fn field<'a>(value: Option<&'a Value>, name: &str) -> Option<&'a str> {
    identifier(value?.get(name)?.as_str()?)
}

fn header<'a>(headers: &'a HeaderMap, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .and_then(identifier)
    })
}

fn bounded_metadata(raw: Option<&RawValue>) -> Option<Value> {
    let raw = raw?.get();
    (raw.len() <= MAX_METADATA_BYTES)
        .then(|| serde_json::from_str(raw).ok())
        .flatten()
}

fn reference(namespace: &str, value: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"crosslane-client-context-v1\0");
    digest.update(namespace.as_bytes());
    digest.update(b"\0");
    digest.update(value.as_bytes());
    let hash = digest.finalize();
    use std::fmt::Write as _;
    hash[..16].iter().fold(String::new(), |mut output, byte| {
        let _ = write!(output, "{byte:02x}");
        output
    })
}

fn claude_identity(metadata: Option<&Value>) -> Option<(Option<String>, Option<String>)> {
    let raw = metadata?.get("user_id")?.as_str()?.trim();
    if raw.len() > MAX_USER_ID_BYTES {
        return None;
    }
    if raw.starts_with('{') {
        let value: Value = serde_json::from_str(raw).ok()?;
        let device = field(Some(&value), "device_id").map(str::to_owned);
        let session = field(Some(&value), "session_id").map(str::to_owned);
        return (device.is_some() || session.is_some()).then_some((device, session));
    }
    let (device, rest) = raw.strip_prefix("user_")?.split_once("_account_")?;
    if device.len() != 64 || !device.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let (_, session) = rest.split_once("_session_")?;
    Some((
        Some(device.to_owned()),
        identifier(session).map(str::to_owned),
    ))
}

pub(super) fn extract(headers: &HeaderMap, body: &[u8]) -> ClientContext {
    let envelope: Envelope<'_> = serde_json::from_slice(body).unwrap_or_default();
    let metadata = bounded_metadata(envelope.metadata);
    let client_metadata = bounded_metadata(envelope.client_metadata);
    let claude = claude_identity(metadata.as_ref());
    let installation = field(client_metadata.as_ref(), "x-codex-installation-id")
        .or_else(|| header(headers, &["x-codex-installation-id"]));
    let user_agent = headers
        .get("user-agent")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let mut context = ClientContext::default();
    if installation.is_some() || user_agent.starts_with("codex_") {
        context.client_kind = ClientKind::Codex;
        if let Some(installation) = installation {
            context.user_ref = Some(reference("codex.installation", installation));
            context.identity_source =
                if field(client_metadata.as_ref(), "x-codex-installation-id").is_some() {
                    ClientIdentitySource::CodexClientMetadata
                } else {
                    ClientIdentitySource::CodexHeader
                };
        }
    } else if claude.is_some() || user_agent.starts_with("claude-cli/") {
        context.client_kind = ClientKind::ClaudeCode;
        if let Some(device) = claude.as_ref().and_then(|(device, _)| device.as_deref()) {
            context.user_ref = Some(reference("claude_code.device", device));
            context.identity_source = ClientIdentitySource::ClaudeMetadata;
        }
    }
    // Body telemetry survives CRS Console's header allowlist. Codex body
    // telemetry is also preferred when a proxy forwards both representations.
    let session = if context.client_kind == ClientKind::Codex {
        field(client_metadata.as_ref(), "session_id")
    } else {
        claude.as_ref().and_then(|(_, session)| session.as_deref())
    }
    .or_else(|| field(metadata.as_ref(), "session_id"))
    .or_else(|| {
        header(
            headers,
            &[
                "x-claude-code-session-id",
                "session-id",
                "session_id",
                "x-session-id",
                "x-opencode-session",
            ],
        )
    });
    let thread = field(client_metadata.as_ref(), "thread_id")
        .or_else(|| field(metadata.as_ref(), "thread_id"))
        .or_else(|| header(headers, &["thread-id", "thread_id"]));
    context.session_ref = session.map(|value| reference("session", value));
    context.thread_ref = thread.map(|value| reference("thread", value));
    if context.client_kind == ClientKind::Unknown && !context.is_unknown() {
        context.client_kind = ClientKind::Generic;
    }
    context
}

pub(super) fn previous_response_id(body: &[u8]) -> Option<String> {
    let envelope: Envelope<'_> = serde_json::from_slice(body).ok()?;
    let raw = envelope.previous_response_id?.get();
    if raw.len() > MAX_IDENTIFIER_BYTES + 2 {
        return None;
    }
    let value: String = serde_json::from_str(raw).ok()?;
    identifier(&value).map(str::to_owned)
}

pub(super) fn stores_response(body: &[u8]) -> bool {
    serde_json::from_slice::<Envelope<'_>>(body)
        .ok()
        .and_then(|envelope| envelope.store)
        .is_none_or(|raw| raw.get() != "false")
}

pub(super) fn log(trace_id: &str, context: &ClientContext) {
    tracing::info!(
        event = "proxy.client.context",
        trace_id,
        client_kind = ?context.client_kind,
        user_ref = context.user_ref.as_deref().unwrap_or("unknown"),
        session_ref = context.session_ref.as_deref().unwrap_or("unknown"),
        thread_ref = context.thread_ref.as_deref().unwrap_or("unknown"),
        identity_source = ?context.identity_source,
        "client context extracted"
    );
}

#[cfg(test)]
mod tests;
