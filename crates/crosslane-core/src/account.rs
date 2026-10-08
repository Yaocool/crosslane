//! 账号与凭证模型。

use std::fmt;

use serde::{Deserialize, Serialize};

/// 认证方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethod {
    /// IAM Identity Center。
    Idc,
    /// 手工导入的社交登录凭据；阶段 1 只持久化，不实现登录流。
    Social,
    /// Kiro CLI/headless API key (ksk_...), stored in access_token.
    #[serde(rename = "api_key", alias = "apikey")]
    ApiKey,
}

/// 订阅等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionKind {
    /// 免费版。
    Free,
    /// Pro。
    Pro,
    /// Pro Plus。
    ProPlus,
    /// Power。
    Power,
    /// Enterprise。
    Enterprise,
    /// Teams。
    Teams,
    /// 未识别的上游订阅类型。
    #[serde(other)]
    Unknown,
}

/// 账号凭证。
///
/// `Debug` 手写实现，token 与 secret 一律输出 `<redacted>`。
#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    /// 当前访问 token。
    pub access_token: String,
    /// 刷新 token。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// OAuth/OIDC client ID。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// OAuth/OIDC client secret。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// AWS 区域。
    pub region: String,
    /// 过期时间，Unix 秒。
    pub expires_at: i64,
    /// 认证方式。
    pub auth_method: AuthMethod,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("region", &self.region)
            .field("expires_at", &self.expires_at)
            .field("auth_method", &self.auth_method)
            .finish()
    }
}

/// 账号额度用量。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    /// 已使用 credits。
    pub current: f64,
    /// 总 credits。
    pub limit: f64,
    /// 已计入总额度的 Kiro 超额额度上限。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overage_cap: Option<f64>,
    /// 使用百分比。
    pub percent_used: f64,
    /// 上游提供的下次重置日期。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_reset_date: Option<String>,
    /// 更新时间，Unix 秒。
    pub updated_at: i64,
}

impl Usage {
    /// 不使用 Kiro 超额额度时可用的总额度。
    pub fn limit_without_overage(&self) -> f64 {
        (self.limit - self.overage_cap.unwrap_or(0.0)).max(0.0)
    }
}

/// 账号订阅信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    /// 标准化订阅等级。
    pub kind: SubscriptionKind,
    /// 展示标题。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 上游原始类型。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_type: Option<String>,
    /// 过期时间，Unix 秒。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// 剩余天数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days_remaining: Option<i64>,
}

fn default_true() -> bool {
    true
}

/// 账号实体。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    /// `acc_<8hex>`。
    pub id: String,
    /// 登录邮箱。
    pub email: String,
    /// 用户备注。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// 是否参与服务。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 64 位 hex，仅用于构造上游 UA。
    pub machine_id: String,
    /// Kiro IDC/Enterprise 请求所需的 profile ARN。
    #[serde(default, alias = "profileArn", skip_serializing_if = "Option::is_none")]
    pub profile_arn: Option<String>,
    /// Kiro 返回的稳定用户 ID，用于防止同一真实身份重复入库。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_user_id: Option<String>,
    /// 认证凭据。
    pub credentials: Credentials,
    /// 最近一次额度数据。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// 订阅数据。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription: Option<Subscription>,
    /// 扁平标签。
    #[serde(default)]
    pub tags: Vec<String>,
    /// 创建时间，Unix 秒。
    pub created_at: i64,
    /// 额度耗尽硬标记，需要持久化。
    #[serde(default)]
    pub credit_exhausted: bool,
}

impl Account {
    /// Return the operator-facing account name, preferring a non-empty label.
    pub fn display_name(&self) -> &str {
        self.label
            .as_deref()
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .unwrap_or(&self.email)
    }

    /// 判断 token 是否已进入刷新窗口。
    pub fn is_token_expiring(&self, now: i64, before_secs: i64) -> bool {
        self.credentials.auth_method != AuthMethod::ApiKey
            && self.credentials.expires_at.saturating_sub(now) <= before_secs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_credentials() -> Credentials {
        Credentials {
            access_token: "at-secret-value".into(),
            refresh_token: Some("rt-secret-value".into()),
            client_id: Some("cid".into()),
            client_secret: Some("cs-secret-value".into()),
            region: "us-east-1".into(),
            expires_at: 1_000_000,
            auth_method: AuthMethod::Idc,
        }
    }

    fn sample_account() -> Account {
        Account {
            id: "acc_00000001".into(),
            email: "a@example.com".into(),
            label: None,
            enabled: true,
            machine_id: "0".repeat(64),
            profile_arn: None,
            upstream_user_id: None,
            credentials: sample_credentials(),
            usage: None,
            subscription: None,
            tags: vec![],
            created_at: 0,
            credit_exhausted: false,
        }
    }

    #[test]
    fn debug_output_redacts_every_secret_field() {
        let debug = format!("{:?}", sample_credentials());
        assert!(!debug.contains("at-secret-value"), "{debug}");
        assert!(!debug.contains("rt-secret-value"), "{debug}");
        assert!(!debug.contains("cs-secret-value"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
        assert!(debug.contains("us-east-1"), "{debug}");
    }

    #[test]
    fn account_debug_does_not_leak_credentials() {
        let debug = format!("{:?}", sample_account());
        assert!(!debug.contains("at-secret-value"), "{debug}");
    }

    #[test]
    fn token_is_expiring_within_the_lead_window() {
        let mut account = sample_account();
        account.credentials.expires_at = 1_000;
        assert!(!account.is_token_expiring(650, 300));
        assert!(account.is_token_expiring(750, 300));
        assert!(account.is_token_expiring(1_500, 300));
    }

    #[test]
    fn display_name_prefers_non_empty_label_and_falls_back_to_email() {
        let mut account = sample_account();
        assert_eq!(account.display_name(), "a@example.com");
        account.label = Some("  Enterprise team  ".into());
        assert_eq!(account.display_name(), "Enterprise team");
        account.label = Some("   ".into());
        assert_eq!(account.display_name(), "a@example.com");
    }

    #[test]
    fn account_roundtrips_through_json() {
        let mut account = sample_account();
        account.id = "acc_deadbeef".into();
        account.label = Some("备注".into());
        account.machine_id = "f".repeat(64);
        account.profile_arn = Some("arn:aws:codewhisperer:us-east-1:123:profile/test".into());
        account.tags = vec!["prod".into()];
        account.usage = Some(Usage {
            current: 120.0,
            limit: 500.0,
            overage_cap: None,
            percent_used: 24.0,
            next_reset_date: Some("2026-09-01".into()),
            updated_at: 42,
        });
        account.subscription = Some(Subscription {
            kind: SubscriptionKind::Pro,
            title: Some("KIRO PRO".into()),
            raw_type: Some("Q_DEVELOPER_STANDALONE_PRO".into()),
            expires_at: Some(2_000_000),
            days_remaining: Some(18),
        });
        let json = serde_json::to_string(&account).expect("serialize");
        let back: Account = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.id, account.id);
        assert_eq!(
            back.credentials.access_token,
            account.credentials.access_token
        );
        assert_eq!(back.tags, account.tags);
        assert_eq!(back.profile_arn, account.profile_arn);
        assert_eq!(
            back.subscription.map(|subscription| subscription.kind),
            Some(SubscriptionKind::Pro)
        );
    }

    #[test]
    fn missing_optional_fields_deserialize_to_defaults() {
        let json = r#"{
            "id": "acc_00000002",
            "email": "b@example.com",
            "machine_id": "1111111111111111111111111111111111111111111111111111111111111111",
            "credentials": {
                "access_token": "at",
                "region": "us-east-1",
                "expires_at": 0,
                "auth_method": "idc"
            },
            "created_at": 0
        }"#;
        let account: Account = serde_json::from_str(json).expect("deserialize");
        assert!(account.enabled);
        assert!(account.tags.is_empty());
        assert!(account.label.is_none());
        assert!(account.profile_arn.is_none());
        assert!(!account.credit_exhausted);
    }

    #[test]
    fn legacy_usage_without_overage_cap_keeps_its_limit() {
        let usage: Usage =
            serde_json::from_str(r#"{"current":10,"limit":100,"percent_used":10,"updated_at":1}"#)
                .expect("legacy usage");
        assert_eq!(usage.overage_cap, None);
        assert_eq!(usage.limit_without_overage(), 100.0);
    }
}
