//! Built-in model catalog used only while per-account discovery is unavailable.

use crosslane_core::account::SubscriptionKind;

use crate::ModelInfo;

/// A conservative snapshot of Kiro's public catalog.
///
/// Dynamic `ListAvailableModels` responses remain authoritative because model
/// availability can vary by account and region. This table only prevents cold
/// starts from advertising a single arbitrary model or routing premium models
/// through Free accounts. Keep it synchronized with <https://kiro.dev/docs/models/>.
#[derive(Debug, Clone, Copy)]
pub struct StaticModel {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub rate_multiplier: f64,
    pub free_tier: bool,
}

pub const STATIC_MODEL_CATALOG: &[StaticModel] = &[
    static_model("gpt-5.6-sol", "GPT-5.6 Sol", 2.4, false),
    static_model("gpt-5.6-terra", "GPT-5.6 Terra", 1.0, false),
    static_model("gpt-5.6-luna", "GPT-5.6 Luna", 0.1, false),
    static_model("claude-opus-5", "Claude Opus 5", 2.2, false),
    static_model("claude-opus-4.8", "Claude Opus 4.8", 2.2, false),
    static_model("claude-opus-4.7", "Claude Opus 4.7", 2.2, false),
    static_model("claude-opus-4.6", "Claude Opus 4.6", 2.2, false),
    static_model("claude-opus-4.5", "Claude Opus 4.5", 2.2, false),
    static_model("claude-sonnet-5", "Claude Sonnet 5", 1.3, false),
    static_model("claude-sonnet-4.6", "Claude Sonnet 4.6", 1.3, false),
    static_model("claude-sonnet-4.5", "Claude Sonnet 4.5", 1.3, true),
    static_model("claude-sonnet-4", "Claude Sonnet 4", 1.3, true),
    static_model("auto", "Auto", 1.0, true),
    static_model("claude-haiku-4.5", "Claude Haiku 4.5", 0.4, false),
    static_model("deepseek-3.2", "DeepSeek 3.2", 0.25, true),
    static_model("minimax-m2.5", "MiniMax M2.5", 0.25, true),
    static_model("glm-5", "GLM-5", 0.5, true),
    static_model("minimax-m2.1", "MiniMax M2.1", 0.15, true),
    static_model("qwen3-coder-next", "Qwen3 Coder Next", 0.05, true),
];

const fn static_model(
    id: &'static str,
    name: &'static str,
    rate_multiplier: f64,
    free_tier: bool,
) -> StaticModel {
    StaticModel {
        id,
        name,
        description: "Kiro upstream model (static fallback)",
        rate_multiplier,
        free_tier,
    }
}

pub fn static_models() -> Vec<ModelInfo> {
    STATIC_MODEL_CATALOG
        .iter()
        .map(|model| ModelInfo {
            model_id: model.id.into(),
            model_name: model.name.into(),
            description: model.description.into(),
            rate_multiplier: Some(model.rate_multiplier),
            token_limits: None,
            additional_model_request_fields_schema: None,
        })
        .collect()
}

/// Returns the conservative fallback catalog that the subscription may use.
/// This is used when Kiro's model-discovery API is unavailable or rejects an
/// otherwise valid enterprise account.
pub fn static_models_for_subscription(subscription: Option<SubscriptionKind>) -> Vec<ModelInfo> {
    static_models()
        .into_iter()
        .filter(|model| static_subscription_can_serve(subscription, &model.model_id))
        .collect()
}

pub fn static_models_for_account(account: &crosslane_core::account::Account) -> Vec<ModelInfo> {
    // Kiro only issues headless API keys to paid accounts. This selects the
    // fallback catalog without fabricating a subscription or credit balance.
    if account.credentials.auth_method == crosslane_core::account::AuthMethod::ApiKey {
        static_models()
    } else {
        static_models_for_subscription(
            account
                .subscription
                .as_ref()
                .map(|subscription| subscription.kind),
        )
    }
}

/// Applies the public tier matrix only when dynamic discovery has no answer.
/// Unknown models are permitted for paid/managed subscriptions for forward
/// compatibility, but not for Free/unknown subscriptions where doing so would
/// recreate the cold-start misrouting this fallback is intended to prevent.
pub fn static_subscription_can_serve(
    subscription: Option<SubscriptionKind>,
    requested_model: &str,
) -> bool {
    let paid = matches!(
        subscription,
        Some(
            SubscriptionKind::Pro
                | SubscriptionKind::ProPlus
                | SubscriptionKind::Power
                | SubscriptionKind::Enterprise
                | SubscriptionKind::Teams
        )
    );
    if paid {
        return true;
    }

    let requested = normalize_model(requested_model);
    STATIC_MODEL_CATALOG
        .iter()
        .any(|model| model.free_tier && model_matches(&requested, &normalize_model(model.id)))
}

fn model_matches(requested: &str, candidate: &str) -> bool {
    requested == candidate || dated_version_suffix(requested, candidate)
}

fn dated_version_suffix(requested: &str, catalog_id: &str) -> bool {
    requested
        .strip_prefix(catalog_id)
        .and_then(|suffix| suffix.strip_prefix('-'))
        .is_some_and(|suffix| {
            suffix.starts_with('v')
                || (suffix.len() >= 8 && suffix[..8].chars().all(|ch| ch.is_ascii_digit()))
        })
}

fn normalize_model(model: &str) -> String {
    model
        .trim()
        .to_ascii_lowercase()
        .replace(['.', '_'], "-")
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_tier_matrix_covers_premium_non_opus_models() {
        for model in [
            "claude-opus-4.8",
            "claude-sonnet-4.6",
            "claude-sonnet-5",
            "claude-haiku-4.5",
            "gpt-5.6-terra",
        ] {
            assert!(!static_subscription_can_serve(
                Some(SubscriptionKind::Free),
                model
            ));
        }
        for model in [
            "auto",
            "claude-sonnet-4.5",
            "claude-sonnet-4-20250514",
            "minimax-m2.5",
            "qwen3-coder-next",
        ] {
            assert!(static_subscription_can_serve(
                Some(SubscriptionKind::Free),
                model
            ));
        }
    }

    #[test]
    fn paid_tiers_remain_forward_compatible() {
        assert!(static_subscription_can_serve(
            Some(SubscriptionKind::Pro),
            "future-premium-model"
        ));
        assert!(!static_subscription_can_serve(
            Some(SubscriptionKind::Unknown),
            "future-premium-model"
        ));
    }

    #[test]
    fn fallback_catalog_is_filtered_by_subscription() {
        let free = static_models_for_subscription(Some(SubscriptionKind::Free));
        assert!(free.iter().any(|model| model.model_id == "auto"));
        assert!(!free.iter().any(|model| model.model_id == "claude-opus-4.6"));

        let power = static_models_for_subscription(Some(SubscriptionKind::Power));
        assert!(power
            .iter()
            .any(|model| model.model_id == "claude-opus-4.6"));
    }
}
