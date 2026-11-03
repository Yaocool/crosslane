use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroize::Zeroizing;

/// Explicit, request-scoped AWS credentials. Never use the default credential
/// chain, and deliberately do not implement Debug or Serialize.
pub struct AwsCredentials {
    pub(super) access_key_id: Zeroizing<String>,
    pub(super) secret_access_key: Zeroizing<String>,
    pub(super) session_token: Option<Zeroizing<String>>,
}

impl AwsCredentials {
    pub fn new(
        access_key_id: String,
        secret_access_key: String,
        session_token: Option<String>,
    ) -> Result<Self, EnterpriseError> {
        let access_key_id = Zeroizing::new(access_key_id);
        let secret_access_key = Zeroizing::new(secret_access_key);
        let session_token = session_token.map(Zeroizing::new);
        if !(16..=128).contains(&access_key_id.len())
            || !access_key_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric())
            || secret_access_key.is_empty()
            || secret_access_key.len() > 4096
            || !secret_access_key
                .bytes()
                .all(|byte| byte.is_ascii_graphic())
            || session_token.as_ref().is_some_and(|value| {
                value.is_empty()
                    || value.len() > 16384
                    || !value.bytes().all(|byte| byte.is_ascii_graphic())
            })
            || (access_key_id.starts_with("ASIA") && session_token.is_none())
        {
            return Err(EnterpriseError::invalid("complete AWS access key, secret key and, for STS credentials, session token are required"));
        }
        Ok(Self {
            access_key_id,
            secret_access_key,
            session_token,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Plan {
    Pro,
    ProPlus,
    ProMax,
    Power,
}

impl Plan {
    pub const ALL: [Self; 4] = [Self::Pro, Self::ProPlus, Self::ProMax, Self::Power];

    pub fn aws_type(self) -> &'static str {
        match self {
            Self::Pro => "Q_DEVELOPER_STANDALONE_PRO",
            Self::ProPlus => "Q_DEVELOPER_STANDALONE_PRO_PLUS",
            Self::ProMax => "Q_DEVELOPER_STANDALONE_PRO_MAX",
            Self::Power => "Q_DEVELOPER_STANDALONE_POWER",
        }
    }

    pub(super) fn from_aws(value: &str) -> Option<Self> {
        let suffix = value
            .strip_prefix("KIRO_ENTERPRISE_")
            .or_else(|| value.strip_prefix("Q_DEVELOPER_STANDALONE_"))?;
        match suffix {
            "PRO" => Some(Self::Pro),
            "PRO_PLUS" => Some(Self::ProPlus),
            "PRO_MAX" => Some(Self::ProMax),
            "POWER" => Some(Self::Power),
            _ => None,
        }
    }

    pub(super) fn rank(self) -> usize {
        Self::ALL
            .iter()
            .position(|plan| *plan == self)
            .expect("known plan")
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeletionMode {
    Immediate,
    #[default]
    EndOfPeriod,
}

impl DeletionMode {
    pub(super) fn aws_type(self) -> &'static str {
        match self {
            Self::Immediate => "IMMEDIATE",
            Self::EndOfPeriod => "END_OF_PERIOD",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mutation {
    Create(Plan),
    Upgrade(Plan),
    Downgrade(Plan),
    Cancel(DeletionMode),
}

impl Mutation {
    pub fn operation(self) -> &'static str {
        match self {
            Self::Create(_) => "CreateAssignment",
            Self::Upgrade(_) | Self::Downgrade(_) => "UpdateAssignment",
            Self::Cancel(_) => "DeleteAssignment",
        }
    }
    pub(super) fn plan(self) -> Option<Plan> {
        match self {
            Self::Create(plan) | Self::Upgrade(plan) | Self::Downgrade(plan) => Some(plan),
            Self::Cancel(_) => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ScopeSelector {
    pub profile_arn: Option<String>,
    pub application_arn: Option<String>,
    pub identity_center_region: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnterpriseProfile {
    pub arn: String,
    pub instance_arn: Option<String>,
    pub identity_center_region: Option<String>,
    pub application_arns: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Profiles {
    pub account_id: String,
    pub kiro_region: String,
    pub profiles: Vec<EnterpriseProfile>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnterpriseScope {
    pub account_id: String,
    pub kiro_region: String,
    pub identity_center_region: String,
    pub identity_store_id: String,
    pub instance_arn: String,
    pub application_arn: String,
    pub profile_arn: String,
    pub legacy: bool,
    // Identity Center ownership can differ from the Kiro billing account.
    #[serde(skip)]
    pub(super) identity_store_account: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubscriptionClaim {
    pub principal_type: &'static str,
    pub principal_id: String,
    pub subscription_type: String,
    pub plan: Option<Plan>,
    pub pending_subscription_type: Option<String>,
    pub pending_plan: Option<Plan>,
    pub status: Option<String>,
}

impl SubscriptionClaim {
    pub(super) fn parse(value: &Value) -> Result<Self, EnterpriseError> {
        let user = value.pointer("/principal/user").and_then(Value::as_str);
        let group = value.pointer("/principal/group").and_then(Value::as_str);
        let (principal_type, principal_id) = match (user, group) {
            (Some(id), None) => (
                "USER",
                super::identity::user_id(id).ok_or_else(|| {
                    EnterpriseError::protocol("invalid subscription user reference")
                })?,
            ),
            (None, Some(id)) => (
                "GROUP",
                super::identity::group_reference(id).ok_or_else(|| {
                    EnterpriseError::protocol("invalid subscription group reference")
                })?,
            ),
            _ => return Err(EnterpriseError::protocol("invalid subscription principal")),
        };
        let subscription_type = value
            .pointer("/type/amazonQ")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| EnterpriseError::protocol("missing subscription type"))?
            .to_owned();
        let pending_subscription_type = match value.get("newType") {
            None | Some(Value::Null) => None,
            Some(Value::Object(map)) if map.is_empty() => None,
            Some(pending) => Some(
                pending
                    .get("amazonQ")
                    .and_then(Value::as_str)
                    .ok_or_else(|| EnterpriseError::protocol("invalid pending subscription type"))?
                    .to_owned(),
            ),
        };
        Ok(Self {
            principal_type,
            principal_id,
            plan: Plan::from_aws(&subscription_type),
            pending_plan: pending_subscription_type
                .as_deref()
                .and_then(Plan::from_aws),
            subscription_type,
            pending_subscription_type,
            status: value
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EnterpriseUser {
    pub user_id: String,
    pub user_name: Option<String>,
    pub display_name: Option<String>,
    pub emails: Vec<String>,
    pub direct_subscriptions: Vec<SubscriptionClaim>,
}

impl EnterpriseUser {
    pub(super) fn parse(
        value: &Value,
        claims: &[SubscriptionClaim],
    ) -> Result<Self, EnterpriseError> {
        let user_id = value
            .get("UserId")
            .and_then(Value::as_str)
            .and_then(super::identity::user_id)
            .ok_or_else(|| EnterpriseError::protocol("missing or invalid user ID"))?;
        Ok(Self {
            user_name: value
                .get("UserName")
                .and_then(Value::as_str)
                .map(str::to_owned),
            display_name: value
                .get("DisplayName")
                .and_then(Value::as_str)
                .map(str::to_owned),
            emails: value
                .get("Emails")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|email| {
                    email
                        .get("Value")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .collect(),
            direct_subscriptions: claims
                .iter()
                .filter(|claim| claim.principal_type == "USER" && claim.principal_id == user_id)
                .cloned()
                .collect(),
            user_id,
        })
    }
}

#[derive(Debug, Serialize)]
pub struct UserList {
    pub scope: EnterpriseScope,
    pub users: Vec<EnterpriseUser>,
    /// Listing users does not expand each user's group memberships.
    pub group_subscriptions: Vec<SubscriptionClaim>,
}

#[derive(Debug, Serialize)]
pub struct UserSubscription {
    pub scope: EnterpriseScope,
    pub user: EnterpriseUser,
    pub group_subscriptions: Vec<SubscriptionClaim>,
}

#[derive(Debug, Serialize)]
pub struct MutationResult {
    pub status: &'static str,
    pub operation: &'static str,
    pub scope: EnterpriseScope,
    pub user_id: String,
    pub previous_plan: Option<Plan>,
    pub requested_plan: Option<Plan>,
    pub deletion_mode: Option<DeletionMode>,
    pub expected_effect: &'static str,
    pub upstream_request_id: Option<String>,
    pub application_assignment_requested: bool,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WriteOutcome {
    NotSubmitted,
    Rejected,
    Unknown,
}

impl WriteOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotSubmitted => "not_submitted",
            Self::Rejected => "rejected",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, thiserror::Error, Serialize)]
#[error("{message}")]
pub struct EnterpriseError {
    pub code: &'static str,
    pub message: &'static str,
    pub http_status: u16,
    pub operation: Option<String>,
    pub upstream_status: Option<u16>,
    pub upstream_code: Option<String>,
    pub upstream_request_id: Option<String>,
    pub write_outcome: WriteOutcome,
}

impl EnterpriseError {
    pub(super) fn new(code: &'static str, http_status: u16, message: &'static str) -> Self {
        Self {
            code,
            message,
            http_status,
            operation: None,
            upstream_status: None,
            upstream_code: None,
            upstream_request_id: None,
            write_outcome: WriteOutcome::NotSubmitted,
        }
    }
    pub fn invalid(message: &'static str) -> Self {
        Self::new("invalid_request", 400, message)
    }
    pub(super) fn protocol(message: &'static str) -> Self {
        Self::new("upstream_protocol_error", 502, message)
    }
    pub(super) fn conflict(message: &'static str) -> Self {
        Self::new("conflict", 409, message)
    }
    pub(super) fn not_found(message: &'static str) -> Self {
        Self::new("not_found", 404, message)
    }
}

/// Validate a supported commercial region without constructing an HTTP client.
pub fn validate_region(region: &str) -> Result<(), EnterpriseError> {
    let segments: Vec<_> = region.split('-').collect();
    if region.len() > 32
        || segments.len() < 3
        || segments[0].len() != 2
        || !segments[..segments.len() - 1]
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_lowercase()))
        || segments.last().is_none_or(|part| {
            part.is_empty() || part.len() > 2 || !part.bytes().all(|byte| byte.is_ascii_digit())
        })
        || region.starts_with("cn-")
        || region.starts_with("us-gov-")
    {
        return Err(EnterpriseError::invalid(
            "a commercial AWS region is required",
        ));
    }
    Ok(())
}
