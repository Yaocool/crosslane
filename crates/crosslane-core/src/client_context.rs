//! Anonymous client correlation, independent of authentication and routing.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    ClaudeCode,
    Codex,
    Generic,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientIdentitySource {
    ClaudeMetadata,
    CodexClientMetadata,
    CodexHeader,
    #[default]
    Unknown,
}

/// All references are namespaced hashes of client-supplied identifiers. They
/// describe a device/installation and its conversations, never an authenticated
/// person, API key, or billing principal. Missing user identity stays unknown.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientContext {
    pub client_kind: ClientKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_ref: Option<String>,
    pub identity_source: ClientIdentitySource,
}

impl ClientContext {
    pub fn conversation_ref(&self) -> Option<&str> {
        self.thread_ref.as_deref().or(self.session_ref.as_deref())
    }

    pub fn is_unknown(&self) -> bool {
        self.user_ref.is_none() && self.session_ref.is_none() && self.thread_ref.is_none()
    }

    /// An explicit different thread/session can fork a stored response's
    /// history, but must receive its own upstream conversation state.
    pub fn forks(&self, previous: &Self) -> bool {
        [
            (&self.session_ref, &previous.session_ref),
            (&self.thread_ref, &previous.thread_ref),
        ]
        .into_iter()
        .any(|(current, previous)| matches!((current, previous), (Some(a), Some(b)) if a != b))
    }

    /// Fill omitted telemetry on a verified continuation. A conflicting
    /// known device is an attribution error; client references do not grant
    /// access to response history (service/key ownership is checked separately).
    pub fn inherit(&mut self, previous: &Self) -> Result<(), &'static str> {
        if matches!((&self.user_ref, &previous.user_ref), (Some(a), Some(b)) if a != b) {
            return Err("previous_response_id belongs to a different client device");
        }
        let new_session =
            matches!((&self.session_ref, &previous.session_ref), (Some(a), Some(b)) if a != b);
        if self.user_ref.is_none() {
            self.user_ref.clone_from(&previous.user_ref);
            self.identity_source = previous.identity_source;
        }
        if matches!(self.client_kind, ClientKind::Unknown | ClientKind::Generic)
            && previous.client_kind != ClientKind::Unknown
        {
            self.client_kind = previous.client_kind;
        }
        // A new session must not inherit a thread belonging to the old one.
        if self.session_ref.is_none() {
            self.session_ref.clone_from(&previous.session_ref);
        }
        if self.thread_ref.is_none() && !new_session {
            self.thread_ref.clone_from(&previous.thread_ref);
        }
        Ok(())
    }
}
