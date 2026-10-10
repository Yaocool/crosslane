//! Account-list projections of Copilot's native quota, separate from local AIU.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ProviderAccountSummary;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopilotQuotaUnit {
    AiCredits,
    PremiumRequests,
}

/// Personal upstream quota only. Unknown quantities stay null, including the
/// zero entitlement/remaining placeholders returned by unlimited Business seats.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CopilotQuotaSummary {
    pub source: String,
    pub scope: String,
    pub unit: Option<CopilotQuotaUnit>,
    pub used: Option<f64>,
    pub total: Option<f64>,
    pub remaining: Option<f64>,
    pub percent_remaining: Option<f64>,
    pub unlimited: Option<bool>,
    pub cached: bool,
}

impl CopilotQuotaSummary {
    pub fn from_info(info: &Value) -> Self {
        let quota = &info["quota_snapshots"]["premium_interactions"];
        let credits = quota["token_based_billing"] == true || info["token_based_billing"] == true;
        let unit = quota.as_object().map(|_| {
            if credits {
                CopilotQuotaUnit::AiCredits
            } else {
                CopilotQuotaUnit::PremiumRequests
            }
        });
        let unlimited = quota["unlimited"].as_bool();
        let entitlement = quantity(&quota["entitlement"]);
        let total = entitlement.filter(|value| *value > 0.0 && unlimited != Some(true));
        // quota_remaining retains fractional credits; remaining may be rounded.
        let remaining = total.and_then(|total| {
            quantity(&quota["quota_remaining"])
                .or_else(|| quantity(&quota["remaining"]))
                .filter(|value| *value <= total)
        });
        let used = if credits {
            quantity(&quota["credits_used"]).or_else(|| {
                total
                    .zip(remaining)
                    .map(|(total, remaining)| total - remaining)
            })
        } else {
            total
                .zip(remaining)
                .map(|(total, remaining)| total - remaining)
        };
        // An explicit zero entitlement is not a usable percentage denominator.
        let percent_remaining = if unlimited == Some(true) || entitlement == Some(0.0) {
            None
        } else {
            quantity(&quota["percent_remaining"])
                .filter(|value| *value <= 100.0)
                .or_else(|| {
                    total
                        .zip(remaining)
                        .map(|(total, remaining)| remaining / total * 100.0)
                })
        };
        Self {
            source: "copilot_internal/user".into(),
            scope: "personal".into(),
            unit,
            used,
            total,
            remaining,
            percent_remaining,
            unlimited,
            cached: info["quota_error"].as_str().is_some(),
        }
    }
}

fn quantity(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .filter(|value| value.is_finite() && *value >= 0.0)
}

/// Also usable by a new CLI connected to a daemon predating this projection.
pub fn enrich_account(account: &mut ProviderAccountSummary) {
    if account.provider_kind == "copilot" {
        let summary = CopilotQuotaSummary::from_info(&account.details["info"]);
        if !account.details.is_object() {
            account.details = serde_json::json!({});
        }
        account.details["quota_summary"] =
            serde_json::to_value(summary).expect("finite quota values");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn business_placeholders_preserve_usage_without_inventing_a_balance() {
        let info = json!({"token_based_billing":true,"quota_snapshots":{
            "premium_interactions":{"unlimited":true,"entitlement":0,"remaining":0,
                "quota_remaining":0,"percent_remaining":100,"credits_used":1}
        }});
        let summary = CopilotQuotaSummary::from_info(&info);
        assert_eq!(summary.unit, Some(CopilotQuotaUnit::AiCredits));
        assert_eq!(summary.used, Some(1.0));
        assert_eq!(summary.total, None);
        assert_eq!(summary.remaining, None);
        assert_eq!(summary.percent_remaining, None);
        assert_eq!(summary.unlimited, Some(true));
        let value = serde_json::to_value(summary).unwrap();
        assert!(value["total"].is_null());
        assert!(value["remaining"].is_null());
        assert_eq!(
            info["quota_snapshots"]["premium_interactions"]["remaining"],
            0
        );
    }

    #[test]
    fn credits_preserve_fractional_remaining_and_authoritative_usage() {
        let summary = CopilotQuotaSummary::from_info(&json!({"quota_error":"HTTP 429",
            "quota_snapshots":{"premium_interactions":{"token_based_billing":true,
                "entitlement":1500,"remaining":1498,"quota_remaining":1498.75,
                "credits_used":1,"percent_remaining":99.9}}
        }));
        assert_eq!(summary.used, Some(1.0));
        assert_eq!(summary.total, Some(1500.0));
        assert_eq!(summary.remaining, Some(1498.75));
        assert_eq!(summary.percent_remaining, Some(99.9));
        assert!(summary.cached);
    }

    #[test]
    fn legacy_requests_and_missing_data_remain_distinct() {
        let summary = CopilotQuotaSummary::from_info(&json!({"quota_snapshots":{
            "premium_interactions":{"entitlement":300,"remaining":240}
        }}));
        assert_eq!(summary.unit, Some(CopilotQuotaUnit::PremiumRequests));
        assert_eq!(summary.used, Some(60.0));
        assert_eq!(summary.remaining, Some(240.0));
        assert_eq!(summary.percent_remaining, Some(80.0));
        let missing = CopilotQuotaSummary::from_info(&Value::Null);
        assert_eq!(missing.used, None);
        assert_eq!(missing.total, None);
        assert_eq!(missing.unlimited, None);
        assert_eq!(missing.unit, None);
    }

    #[test]
    fn exhaustion_is_distinct_from_missing_or_invalid_quantities() {
        for remaining in [json!(0), json!(-1), json!(200)] {
            let summary = CopilotQuotaSummary::from_info(&json!({"token_based_billing":true,
                "quota_snapshots":{"premium_interactions":{
                    "unlimited":false,"entitlement":100,"remaining":remaining,
                    "credits_used":120,"percent_remaining":101}}
            }));
            assert_eq!(summary.used, Some(120.0));
            assert_eq!(summary.total, Some(100.0));
            if remaining == 0 {
                assert_eq!(summary.remaining, Some(0.0));
                assert_eq!(summary.percent_remaining, Some(0.0));
            } else {
                assert_eq!(summary.remaining, None);
                assert_eq!(summary.percent_remaining, None);
            }
        }
    }
}
