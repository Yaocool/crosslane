//! Shared preparation for Kiro transport and its input-budget estimates.

use crate::KiroPayload;

pub(crate) const CREDENTIAL_REFERENCE_HINT: &str = "[Gateway credential references]\nStrings beginning with __CROSSLANE_SECRET_ are opaque references to values held by the caller. Copy each reference exactly when using that value in tool arguments or checkpoints; the gateway restores it locally. Do not guess, rewrite, decode or validate the hidden value yourself. Environment variable references remain expressions for the tool to evaluate.\n\n";

/// Prepare an outgoing copy without changing the caller's source context.
/// Budgeting and dispatch must use this same transformation: short secrets,
/// schema alternatives and the reference hint can all expand the request.
pub fn prepare_redacted_kiro_payload(payload: &KiroPayload, known_secrets: &[&str]) -> KiroPayload {
    let mut prepared = payload.clone();
    let session = prepared.redaction_session.clone().unwrap_or_default();
    prepared.redaction_session = Some(session.clone());
    let content = &mut prepared
        .conversation_state
        .current_message
        .user_input_message
        .content;
    // Remove our previous hint before matching remembered values. Otherwise a
    // credential equal to a word in the hint could mask the hint itself and
    // cause repeated preparation to prepend it again.
    if let Some(original) = content.strip_prefix(CREDENTIAL_REFERENCE_HINT) {
        *content = original.to_owned();
    }
    session.redact_payload(&mut prepared, known_secrets);
    if !session.is_empty() {
        prepared
            .conversation_state
            .current_message
            .user_input_message
            .content
            .insert_str(0, CREDENTIAL_REFERENCE_HINT);
    }
    prepared
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RedactionSession;
    use serde_json::json;

    #[test]
    fn preparation_is_idempotent_and_preserves_private_source() {
        let mut source: KiroPayload = serde_json::from_value(json!({
            "conversationState": {
                "chatTriggerType":"MANUAL", "conversationId":"fixture",
                "currentMessage":{"userInputMessage":{
                    "content":"password=Strings", "modelId":"model", "origin":"CLI"
                }}
            }
        }))
        .unwrap();
        source.redaction_session = Some(RedactionSession::new());
        let once = prepare_redacted_kiro_payload(&source, &[]);
        let twice = prepare_redacted_kiro_payload(&once, &[]);
        assert_eq!(
            serde_json::to_value(&once).unwrap(),
            serde_json::to_value(&twice).unwrap()
        );
        let content = &once
            .conversation_state
            .current_message
            .user_input_message
            .content;
        assert_eq!(content.matches(CREDENTIAL_REFERENCE_HINT).count(), 1);
        assert!(!content.ends_with("password=Strings"));
        assert_eq!(
            source
                .conversation_state
                .current_message
                .user_input_message
                .content,
            "password=Strings"
        );
    }

    #[test]
    fn preparation_leaves_ordinary_content_unchanged() {
        let source: KiroPayload = serde_json::from_value(json!({
            "conversationState": {
                "chatTriggerType":"MANUAL", "conversationId":"fixture",
                "currentMessage":{"userInputMessage":{
                    "content":"Read file.txt", "modelId":"model", "origin":"CLI"
                }}
            }
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(prepare_redacted_kiro_payload(&source, &[])).unwrap(),
            serde_json::to_value(source).unwrap()
        );
    }
}
