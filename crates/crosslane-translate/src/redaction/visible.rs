//! Restores credential references in client-visible assistant text.
//!
//! The client already owns every value it sent, so references the model
//! repeats in its answer are expanded before the text leaves the gateway.
//! Streaming callers hold back a trailing partial reference with
//! [`partial_marker_start`] until the next chunk completes it.

use serde_json::Value;

use super::{marker_pattern, RedactionSession};

const MARKER_PREFIX: &[u8] = b"__CROSSLANE_SECRET_";
const NONCE_LEN: usize = 32;
/// Prefix, nonce, separator, a 20-digit counter and the closing `__`.
const MAX_MARKER_LEN: usize = MARKER_PREFIX.len() + NONCE_LEN + 1 + 20 + 2;

impl RedactionSession {
    /// Expands this session's references in display text. Unknown references
    /// stay literal, and restored values are never expanded a second time.
    /// Unlike [`RedactionSession::restore_text`], the text is not reparsed as
    /// JSON, so an arbitrary streaming chunk keeps its exact formatting.
    pub fn restore_visible_text(&self, text: &str) -> String {
        if !self.enabled || !text.contains("__CROSSLANE_SECRET_") {
            return text.to_owned();
        }
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        marker_pattern()
            .replace_all(text, |captures: &regex::Captures<'_>| {
                let marker = captures.get(0).expect("marker").as_str();
                match state.by_marker.get(marker) {
                    Some(Value::String(value)) => value.clone(),
                    Some(value) => value.to_string(),
                    None => marker.to_owned(),
                }
            })
            .into_owned()
    }
}

/// Byte offset where a trailing, still incomplete reference begins, or
/// `text.len()` when the whole text can be restored now.
pub fn partial_marker_start(text: &str) -> usize {
    let bytes = text.as_bytes();
    // A complete reference is restorable; never hold its closing `__`.
    let floor = marker_pattern()
        .find_iter(text)
        .last()
        .map_or(0, |marker| marker.end());
    let start = floor.max(bytes.len().saturating_sub(MAX_MARKER_LEN));
    (start..bytes.len())
        .find(|&index| bytes[index] == b'_' && is_partial_marker(&bytes[index..]))
        .unwrap_or(bytes.len())
}

/// True when `rest` is a strict prefix of some valid reference.
fn is_partial_marker(rest: &[u8]) -> bool {
    let head = rest.len().min(MARKER_PREFIX.len());
    if rest[..head] != MARKER_PREFIX[..head] {
        return false;
    }
    let rest = &rest[head..];
    let hex = rest
        .iter()
        .take(NONCE_LEN)
        .take_while(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        .count();
    if hex < rest.len().min(NONCE_LEN) {
        return false;
    }
    let rest = &rest[hex..];
    let Some((separator, rest)) = rest.split_first() else {
        return true;
    };
    if *separator != b'_' {
        return false;
    }
    let digits = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
    match &rest[digits..] {
        [] => true,
        // One closing underscore is still incomplete; two complete it.
        [b'_'] => digits > 0,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARKER: &str = "__CROSSLANE_SECRET_0123456789abcdef0123456789abcdef_13__";

    #[test]
    fn complete_text_is_not_held() {
        assert_eq!(partial_marker_start("plain text"), 10);
        let text = format!("value {MARKER} done");
        assert_eq!(partial_marker_start(&text), text.len());
        // The closing `__` of a complete reference is not a new prefix.
        let text = format!("value {MARKER}");
        assert_eq!(partial_marker_start(&text), text.len());
    }

    #[test]
    fn every_split_point_of_a_reference_is_held() {
        for split in 1..MARKER.len() {
            let text = format!("see {}", &MARKER[..split]);
            assert_eq!(partial_marker_start(&text), 4, "split at {split}: {text}");
        }
    }

    #[test]
    fn lookalikes_are_released() {
        for text in [
            "a__CROSSLANE_X",
            "a__CROSSLANE_SECRET_0123456789ABCDEF",
            "a__CROSSLANE_SECRET_0123456789abcdef0123456789abcdef-",
            "a__CROSSLANE_SECRET_0123456789abcdef0123456789abcdef__x",
        ] {
            assert_eq!(partial_marker_start(text), text.len(), "{text}");
        }
        // A trailing `__` may begin the next reference, so it is held.
        let text = "a__CROSSLANE_SECRET_0123456789abcdef0123456789abcdef__";
        assert_eq!(partial_marker_start(text), text.len() - 2);
    }

    #[test]
    fn visible_text_restores_known_references_only() {
        let session = RedactionSession::new();
        let marker = session.marker(Value::String("s3cr3t-value".into()));
        let forged = "__CROSSLANE_SECRET_ffffffffffffffffffffffffffffffff_1__";
        let text = format!("{{\"key\": \"{marker}\"}} and {forged}");
        assert_eq!(
            session.restore_visible_text(&text),
            format!("{{\"key\": \"s3cr3t-value\"}} and {forged}")
        );
        assert_eq!(
            RedactionSession::with_enabled(false).restore_visible_text(&text),
            text
        );
    }
}
