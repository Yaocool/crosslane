//! Non-secret selectors for daemon-managed Kiro Enterprise operations.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EnterpriseScope {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_arn: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub application_arn: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity_center_region: Option<String>,
    pub legacy: bool,
}
