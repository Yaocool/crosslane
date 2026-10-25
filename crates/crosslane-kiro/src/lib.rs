//! Kiro/AWS upstream transport and event-stream decoding.

pub mod catalog;
pub mod client;
pub mod endpoint;
pub mod event_stream;
pub mod identity;
mod response_redaction;

pub use catalog::{
    static_models, static_models_for_account, static_models_for_subscription,
    static_subscription_can_serve, StaticModel, STATIC_MODEL_CATALOG,
};
pub use client::{
    KiroClient, KiroError, KiroResponse, ModelInfo, UsageInfo, UsageLimits, UsageUserInfo,
};
pub use crosslane_translate::{WebSearchResult, WebSearchResults};
pub use endpoint::{EndpointCache, EndpointDefinition, EndpointKey, EndpointPurpose};
pub use event_stream::{EventStreamDecoder, KiroCitation, KiroEvent};
