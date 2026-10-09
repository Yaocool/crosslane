use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Safe account metadata. Token deadlines are not subscription expiry dates.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CopilotAccountInfo {
    pub github_token_expires_at: Option<i64>,
    pub github_refresh_token_expires_at: Option<i64>,
    pub api_token_expires_at: Option<i64>,
    pub api_token_refresh_in_secs: Option<u64>,
    pub sku: Option<String>,
    pub plan: Option<String>,
    pub token_based_billing: Option<bool>,
    pub quota_reset_date: Option<String>,
    pub quota_snapshots: BTreeMap<String, CopilotQuotaSnapshot>,
    pub quota_checked_at: Option<i64>,
    pub quota_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CopilotQuotaSnapshot {
    pub entitlement: Option<f64>,
    pub remaining: Option<f64>,
    pub quota_remaining: Option<f64>,
    pub percent_remaining: Option<f64>,
    pub unlimited: Option<bool>,
    pub token_based_billing: Option<bool>,
    pub credits_used: Option<f64>,
    pub overage_count: Option<f64>,
    pub overage_permitted: Option<bool>,
    pub overage_entitlement: Option<f64>,
    pub has_quota: Option<bool>,
    pub quota_reset_at: Option<i64>,
    pub timestamp_utc: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct CopilotCredentials {
    pub github_access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_refresh_token_expires_at: Option<i64>,
    #[serde(default = "default_token_type")]
    pub token_type: String,
}

fn default_token_type() -> String {
    "bearer".into()
}

impl fmt::Debug for CopilotCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CopilotCredentials")
            .field("github_access_token", &"[REDACTED]")
            .field(
                "github_refresh_token",
                &self.github_refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("github_expires_at", &self.github_expires_at)
            .field(
                "github_refresh_token_expires_at",
                &self.github_refresh_token_expires_at,
            )
            .field("token_type", &self.token_type)
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct CopilotAccount {
    pub id: String,
    pub github_host: String,
    pub github_user_id: u64,
    pub login: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub credentials: CopilotCredentials,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
    #[serde(default)]
    pub supported_models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default)]
    pub info: CopilotAccountInfo,
}

fn default_true() -> bool {
    true
}

impl fmt::Debug for CopilotAccount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CopilotAccount")
            .field("id", &self.id)
            .field("github_host", &self.github_host)
            .field("github_user_id", &self.github_user_id)
            .field("login", &self.login)
            .field("email", &self.email)
            .field("label", &self.label)
            .field("tags", &self.tags)
            .field("enabled", &self.enabled)
            .field("credentials", &self.credentials)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .field("supported_models", &self.supported_models)
            .field("endpoint", &self.endpoint)
            .field("last_error", &self.last_error)
            .field("info", &self.info)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopilotAccountSummary {
    pub provider_id: String,
    pub provider_kind: String,
    pub id: String,
    pub login: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub enabled: bool,
    pub auth_state: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub supported_models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default)]
    pub info: CopilotAccountInfo,
}

impl CopilotAccountSummary {
    pub fn is_available(&self) -> bool {
        self.enabled
            && self.auth_state == "ready"
            && self.last_error.is_none()
            && !self.supported_models.is_empty()
    }
}
