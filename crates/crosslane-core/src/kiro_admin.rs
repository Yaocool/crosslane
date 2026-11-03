//! Kiro administration credentials are deliberately outside the TOML Config.
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KiroAdminCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    #[serde(default)]
    pub session_token: Option<String>,
    #[serde(default = "default_region")]
    pub region: String,
    #[serde(default)]
    pub profile_arn: Option<String>,
    #[serde(default)]
    pub application_arn: Option<String>,
    #[serde(default)]
    pub identity_center_region: Option<String>,
}

fn default_region() -> String {
    "us-east-1".into()
}

impl std::fmt::Debug for KiroAdminCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KiroAdminCredentials(<redacted>)")
    }
}

impl Drop for KiroAdminCredentials {
    fn drop(&mut self) {
        self.access_key_id.zeroize();
        self.secret_access_key.zeroize();
        self.session_token.zeroize();
    }
}
