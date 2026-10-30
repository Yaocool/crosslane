//! Explicit public projections: upstream credentials and raw provider details never leave REST.

use std::collections::BTreeMap;

use crosslane_ipc::protocol::ProviderAccountSummary;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum GroupBy {
    #[default]
    Provider,
    Tag,
}

pub(super) fn account(account: ProviderAccountSummary) -> Value {
    let quota = if account.provider_kind == "kiro" {
        let quantity =
            |value: Option<f64>| value.filter(|value| value.is_finite() && *value >= 0.0);
        let used = quantity(account.quota_current);
        let total = quantity(account.quota_limit);
        json!({
            "unit":"kiro_credits", "source":"kiro_usage_cache", "cached":true,
            "used":used, "total":total, "remaining":used.zip(total).map(|(used,total)| (total-used).max(0.0)),
            "updated_at":account.details["usage_updated_at"]
        })
    } else {
        let mut quota = serde_json::to_value(
            crosslane_ipc::copilot::CopilotQuotaSummary::from_info(&account.details["info"]),
        )
        .expect("finite quota projection");
        quota["updated_at"] = account.details["info"]["quota_checked_at"].clone();
        quota
    };
    let local_aiu = serde_json::from_value::<crate::meter::AccountAiuUsage>(
        account.details["local_aiu_usage"].clone(),
    )
    .ok();
    let token_expires_at = if account.provider_kind == "copilot" {
        &account.details["info"]["api_token_expires_at"]
    } else {
        &account.details["token_expires_at"]
    }
    .as_i64()
    .filter(|value| *value > 0);
    json!({
        "id":account.id, "provider_id":account.provider_id, "provider_kind":account.provider_kind,
        "reference":format!("{}/{}",account.provider_id,account.id),
        "display_name":account.display_name, "email":account.email, "label":account.label,
        "enabled":account.enabled, "health":account.health, "auth_state":account.auth_state,
        "tags":account.tags, "supported_models":account.supported_models,
        "token_expires_at":token_expires_at,
        "quota":quota,
        "local_aiu_usage":local_aiu
    })
}

pub(super) fn groups(accounts: Vec<Value>, by: GroupBy) -> Vec<Value> {
    let mut groups = BTreeMap::<Option<String>, Vec<Value>>::new();
    for account in accounts {
        let keys = match by {
            GroupBy::Provider => vec![account["provider_id"].as_str().map(str::to_owned)],
            GroupBy::Tag => {
                let tags = account["tags"].as_array().expect("projected account tags");
                if tags.is_empty() {
                    vec![None]
                } else {
                    tags.iter()
                        .map(|tag| tag.as_str().map(str::to_owned))
                        .collect()
                }
            }
        };
        for key in keys {
            groups.entry(key).or_default().push(account.clone());
        }
    }
    groups
        .into_iter()
        .map(|(key, accounts)| match by {
            GroupBy::Provider => {
                json!({"provider_id":key,"count":accounts.len(),"accounts":accounts})
            }
            GroupBy::Tag => json!({"tag":key,"count":accounts.len(),"accounts":accounts}),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(kind: &str, details: Value) -> ProviderAccountSummary {
        ProviderAccountSummary {
            provider_id: kind.into(),
            provider_kind: kind.into(),
            id: "acc_12345678".into(),
            display_name: "fixture".into(),
            email: None,
            label: None,
            enabled: true,
            health: "available".into(),
            auth_state: "valid".into(),
            tags: vec![],
            quota_current: None,
            quota_limit: None,
            quota_unit: None,
            supported_models: vec![],
            details,
        }
    }

    #[test]
    fn kiro_remaining_is_computed_from_usage_and_unknown_stays_unknown() {
        let mut input = fixture(
            "kiro",
            json!({"access_token":"private-token", "usage_updated_at":123}),
        );
        input.quota_current = Some(26.75);
        input.quota_limit = Some(100.0);
        let projected = account(input.clone());
        assert_eq!(projected["quota"]["remaining"], 73.25);
        assert_eq!(projected["quota"]["unit"], "kiro_credits");
        assert_eq!(projected["quota"]["updated_at"], 123);
        assert!(projected.get("details").is_none());
        assert!(!projected.to_string().contains("private-token"));
        input.quota_current = None;
        assert!(account(input.clone())["quota"]["remaining"].is_null());
        input.quota_current = Some(101.0);
        assert_eq!(account(input.clone())["quota"]["remaining"], 0.0);
        input.quota_limit = Some(f64::NAN);
        assert!(account(input)["quota"]["total"].is_null());
    }

    #[test]
    fn copilot_native_quota_and_exact_local_aiu_are_separate_safe_projections() {
        let mut input = fixture(
            "copilot",
            json!({
                "refresh_token":"private-refresh", "info":{
                    "secret":"private-upstream", "quota_snapshots":{"premium_interactions":{
                        "entitlement":300,"remaining":279,"quota_remaining":279.875,
                        "token_based_billing":true,"unlimited":false
                    }}
                },
                "local_aiu_usage":{"total_nano_aiu":17_000_001,"billed_requests":2,
                    "first_recorded_at":123,"last_recorded_at":124,"secret":"private-extra"}
            }),
        );
        let projected = account(input.clone());
        assert_eq!(projected["quota"]["remaining"], 279.875);
        assert_eq!(projected["quota"]["unit"], "ai_credits");
        assert_eq!(projected["local_aiu_usage"]["total_nano_aiu"], 17_000_001);
        assert_eq!(projected["local_aiu_usage"]["billed_requests"], 2);
        assert!(!projected.to_string().contains("private-"));
        input.details["info"]["quota_snapshots"]["premium_interactions"] = json!({
            "unlimited":true,"entitlement":0,"remaining":0,"token_based_billing":true,"credits_used":1
        });
        let unlimited = account(input);
        assert!(unlimited["quota"]["remaining"].is_null());
        assert!(unlimited["quota"]["total"].is_null());
        assert_eq!(unlimited["quota"]["used"], 1.0);
        assert_eq!(unlimited["local_aiu_usage"]["total_nano_aiu"], 17_000_001);
    }

    #[test]
    fn tag_groups_retain_untagged_and_all_memberships() {
        let untagged = account(fixture("kiro", json!({})));
        let mut tagged = fixture("copilot", json!({}));
        tagged.tags = vec!["a".into(), "b".into()];
        let tagged = account(tagged);
        let by_provider = groups(vec![untagged.clone(), tagged.clone()], GroupBy::Provider);
        assert_eq!(by_provider.len(), 2);
        let by_tag = groups(vec![untagged, tagged], GroupBy::Tag);
        assert_eq!(by_tag.len(), 3);
        assert!(by_tag[0]["tag"].is_null());
        assert_eq!(by_tag[1]["tag"], "a");
        assert_eq!(by_tag[2]["tag"], "b");
        assert_eq!(
            by_tag[1]["accounts"][0]["reference"],
            by_tag[2]["accounts"][0]["reference"]
        );
    }
}
