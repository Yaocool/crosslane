use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceLoginStatus {
    Pending,
    Authorized,
    Denied,
    Expired,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceLoginState {
    pub id: String,
    pub provider_id: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_at: i64,
    pub interval_secs: u64,
    pub status: DeviceLoginStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct DeviceLoginTask {
    pub state: DeviceLoginState,
    pub device_code: String,
    pub next_poll_at: i64,
    pub label: Option<String>,
    pub expected_login: Option<String>,
}
