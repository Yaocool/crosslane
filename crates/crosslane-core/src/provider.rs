//! Provider-neutral identities, capabilities, models, and routing references.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// Reserved selector used by management commands to address every provider.
pub const ALL_PROVIDERS: &str = "all";

/// A validated, stable provider instance identifier.
///
/// IDs are deliberately independent from a driver's kind so multiple instances
/// of one driver can coexist (for example `copilot-team-a` and
/// `copilot-team-b`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProviderId(String);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid provider id `{value}`: {reason}")]
pub struct ProviderIdError {
    value: String,
    reason: &'static str,
}

impl ProviderId {
    /// Parses an instance ID matching `^[a-z][a-z0-9-]{0,63}$`.
    pub fn parse(value: impl Into<String>) -> Result<Self, ProviderIdError> {
        let value = value.into();
        if value == ALL_PROVIDERS {
            return Err(ProviderIdError {
                value,
                reason: "`all` is reserved for selectors",
            });
        }
        if value.is_empty() || value.len() > 64 {
            return Err(ProviderIdError {
                value,
                reason: "length must be between 1 and 64 bytes",
            });
        }
        let mut bytes = value.bytes();
        if !bytes.next().is_some_and(|byte| byte.is_ascii_lowercase()) {
            return Err(ProviderIdError {
                value,
                reason: "the first character must be a lowercase ASCII letter",
            });
        }
        if !bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-') {
            return Err(ProviderIdError {
                value,
                reason: "only lowercase ASCII letters, digits, and hyphens are allowed",
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for ProviderId {
    type Err = ProviderIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for ProviderId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ProviderId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

/// Public wire protocols understood by provider adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProtocol {
    ClaudeMessages,
    OpenAiChat,
    OpenAiResponses,
}

impl ProviderProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeMessages => "claude_messages",
            Self::OpenAiChat => "openai_chat",
            Self::OpenAiResponses => "openai_responses",
        }
    }
}

/// How a provider exposes a capability.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySupport {
    Native,
    Adapted,
    Unsupported,
    #[default]
    Unknown,
}

/// Provider-level capabilities advertised through management APIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    #[serde(default)]
    pub protocols: Vec<ProviderProtocol>,
    #[serde(default)]
    pub device_flow: bool,
    #[serde(default)]
    pub token_import: bool,
    #[serde(default)]
    pub model_discovery: bool,
    #[serde(default)]
    pub token_counting: CapabilitySupport,
    #[serde(default)]
    pub usage: CapabilitySupport,
}

impl Default for ProviderCapabilities {
    fn default() -> Self {
        Self {
            protocols: Vec::new(),
            device_flow: false,
            token_import: false,
            model_discovery: false,
            token_counting: CapabilitySupport::Unknown,
            usage: CapabilitySupport::Unknown,
        }
    }
}

/// Provider runtime metadata returned by the registry and the CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDescriptor {
    pub id: ProviderId,
    pub kind: String,
    pub enabled: bool,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub capabilities: ProviderCapabilities,
}

/// Provider-neutral model metadata. Numeric limits stay unabridged in JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderModel {
    pub provider_id: ProviderId,
    pub id: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub protocols: Vec<ProviderProtocol>,
    #[serde(default)]
    pub capabilities: serde_json::Value,
}

/// A provider-qualified account reference used by v2 management APIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAccountRef {
    pub provider_id: ProviderId,
    pub account_id: String,
}

impl fmt::Display for ProviderAccountRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.provider_id, self.account_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_ids_are_path_safe_and_reserve_all() {
        for valid in ["kiro", "copilot", "copilot-team-a", "a1"] {
            assert_eq!(ProviderId::parse(valid).unwrap().as_str(), valid);
        }
        for invalid in [
            "",
            "all",
            "Copilot",
            "1copilot",
            "copilot/team",
            "../x",
            "a_b",
        ] {
            assert!(ProviderId::parse(invalid).is_err(), "{invalid}");
        }
        assert!(ProviderId::parse(format!("a{}", "x".repeat(64))).is_err());
    }

    #[test]
    fn serde_rejects_invalid_provider_ids() {
        assert!(serde_json::from_str::<ProviderId>("\"../copilot\"").is_err());
        let id = ProviderId::parse("copilot-team-a").unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"copilot-team-a\"");
    }
}
