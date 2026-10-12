//! GitHub Copilot provider adapter.

mod account;
mod auth;
mod bridge;
mod client;
mod responses;

pub use account::{
    CopilotAccount, CopilotAccountInfo, CopilotAccountSummary, CopilotCredentials,
    CopilotQuotaSnapshot,
};
pub use auth::{DeviceLoginState, DeviceLoginStatus};
pub use client::{CopilotProvider, CopilotSettings};
