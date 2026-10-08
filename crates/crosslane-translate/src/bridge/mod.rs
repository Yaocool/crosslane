//! Bidirectional public-protocol adapters. Native requests bypass this module.
//!
//! Requests use Chat-shaped history as an intermediate representation; streaming
//! responses use content blocks with stable identities and explicit completion.
//! No adapter performs model routing, account selection, or billing estimation.

mod request;
mod response;
mod stream;

pub use request::translate_request;
pub use response::{translate_error, translate_response};
pub use stream::StreamBridge;

use crosslane_core::provider::ProviderProtocol;
use serde_json::{json, Value};

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct BridgeError(pub String);

fn invalid(message: impl Into<String>) -> BridgeError {
    BridgeError(message.into())
}

fn array(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or_default()
}

fn string(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

#[cfg(test)]
mod tests;
