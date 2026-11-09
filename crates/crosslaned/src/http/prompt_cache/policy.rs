//! Local cache thresholds per resolved Kiro model. Claude values follow the
//! Anthropic prompt-caching limits; GPT-5.6 reports exact boundaries. Models
//! without published cache semantics keep the legacy 1024-token default.

use super::allocate::Boundary;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModelPolicy {
    pub min_tokens: u64,
    pub boundary: Boundary,
    /// `false` when no table entry matched and `kiro-default` applied.
    pub listed: bool,
}

const KIRO_DEFAULT: ModelPolicy = ModelPolicy {
    min_tokens: 1024,
    boundary: Boundary::Exact,
    listed: false,
};

/// (family prefix after normalization, minimum tokens). Longest prefix wins.
const TABLE: &[(&str, u64)] = &[
    ("fable-5-1", 512),
    ("fable-5", 512),
    ("mythos-5-1", 512),
    ("mythos-5", 512),
    ("mythos-preview", 2048),
    ("opus-5-5", 512),
    ("opus-5", 512),
    ("sonnet-5-5", 512),
    ("sonnet-5", 1024),
    ("haiku-5-5", 512),
    ("opus-4-8", 1024),
    ("opus-4-7", 2048),
    ("opus-4-6", 4096),
    ("opus-4-5", 4096),
    ("opus-4-1", 1024),
    ("opus-4", 1024),
    ("sonnet-4-6", 1024),
    ("sonnet-4-5", 1024),
    ("sonnet-4", 1024),
    ("haiku-4-5", 4096),
    ("haiku-3-5", 2048),
    ("gpt-5-6", 1024),
];

/// Lowercase, `.`/`_` → `-`, drop `anthropic.` / `claude-` prefixes. Version
/// suffixes such as dates remain; prefix matching never merges two families
/// whose names only share a leading digit (`opus-4` does not match `opus-45`).
fn normalize(model: &str) -> String {
    let lower = model.trim().to_ascii_lowercase().replace(['.', '_'], "-");
    let lower = lower
        .strip_prefix("anthropic-")
        .unwrap_or(&lower)
        .to_owned();
    lower.strip_prefix("claude-").unwrap_or(&lower).to_owned()
}

fn matches_family(model: &str, family: &str) -> bool {
    model == family
        || model
            .strip_prefix(family)
            .is_some_and(|rest| rest.starts_with('-') && !rest[1..].starts_with(char::is_numeric))
        || model
            .strip_prefix(family)
            .is_some_and(|rest| rest.starts_with('-') && is_date_suffix(&rest[1..]))
}

fn is_date_suffix(rest: &str) -> bool {
    rest.len() >= 8
        && rest[..8]
            .chars()
            .all(|character| character.is_ascii_digit())
}

pub(crate) fn policy(resolved_model: &str) -> ModelPolicy {
    let model = normalize(resolved_model);
    TABLE
        .iter()
        .filter(|(family, _)| matches_family(&model, family))
        .max_by_key(|(family, _)| family.len())
        .map_or(KIRO_DEFAULT, |(_, min_tokens)| ModelPolicy {
            min_tokens: *min_tokens,
            boundary: Boundary::Exact,
            listed: true,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn min(model: &str) -> (u64, bool) {
        let policy = policy(model);
        (policy.min_tokens, policy.listed)
    }

    #[test]
    fn catalog_models_resolve_to_documented_thresholds() {
        assert_eq!(min("claude-opus-5.5"), (512, true));
        assert_eq!(min("claude-opus-5"), (512, true));
        assert_eq!(min("claude-sonnet-5"), (1024, true));
        assert_eq!(min("claude-sonnet-5.5"), (512, true));
        assert_eq!(min("claude-opus-4.8"), (1024, true));
        assert_eq!(min("claude-opus-4.7"), (2048, true));
        assert_eq!(min("claude-opus-4.6"), (4096, true));
        assert_eq!(min("claude-opus-4.5"), (4096, true));
        assert_eq!(min("claude-sonnet-4.6"), (1024, true));
        assert_eq!(min("claude-sonnet-4"), (1024, true));
        assert_eq!(min("claude-haiku-4.5"), (4096, true));
        assert_eq!(min("claude-fable-5-1"), (512, true));
        assert_eq!(min("gpt-5.6-sol"), (1024, true));
        assert_eq!(min("CLAUDE_SONNET_4_5_20250929_V1_0"), (1024, true));
    }

    #[test]
    fn unknown_models_use_the_kiro_default() {
        for model in [
            "deepseek-3.2",
            "glm-5",
            "minimax-m2.5",
            "qwen3-coder-next",
            "auto",
            "x",
        ] {
            assert_eq!(min(model), (1024, false), "{model}");
        }
        // A family prefix must end at a version boundary.
        assert_eq!(min("claude-opus-45"), (1024, false));
    }

    #[test]
    fn every_table_entry_is_exact() {
        // Adding a round128 entry requires an official source and a new test.
        for (family, _) in TABLE {
            assert_eq!(policy(family).boundary, Boundary::Exact, "{family}");
        }
    }
}
