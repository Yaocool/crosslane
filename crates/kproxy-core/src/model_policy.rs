//! Model identity for global deny rules. Deliberately does not perform family
//! fallback: disabling Sonnet 4.6 must not disable Sonnet 4.5.

pub fn model_key(model: &str) -> String {
    let lower = model.trim().to_ascii_lowercase();
    let lower = lower.strip_prefix("anthropic.").unwrap_or(&lower);
    let normalized = lower.replace(['.', '_'], "-");
    let alias = normalized
        .strip_prefix("claude-")
        .or_else(|| normalized.strip_prefix("claude"))
        .unwrap_or(&normalized);
    for family in ["opus", "sonnet", "haiku"] {
        let mut parts = alias
            .split('-')
            .filter(|part| !part.is_empty() && *part != "latest")
            .collect::<Vec<_>>();
        if let Some(version) = parts
            .first()
            .and_then(|part| part.strip_prefix(family))
            .filter(|version| {
                !version.is_empty() && version.bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            parts[0] = version;
        } else if let Some(index) = parts.iter().position(|part| *part == family) {
            parts.remove(index);
        } else {
            continue;
        }
        if parts.last().is_some_and(|part| {
            part.len() == 8
                && part.starts_with("20")
                && part.bytes().all(|byte| byte.is_ascii_digit())
        }) {
            parts.pop();
        }
        if !parts.is_empty()
            && parts
                .iter()
                .all(|part| part.bytes().all(|byte| byte.is_ascii_digit()))
        {
            return format!("claude-{family}-{}", parts.join("-"));
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_model_aliases_share_a_deny_rule_without_family_fallback() {
        let key = model_key("claude-sonnet-4.6");
        for alias in [
            "SONNET4.6",
            "claude-4-6-sonnet",
            "anthropic.claude-sonnet-4-6",
            "claude-sonnet-4-6-20260217",
            "claude-sonnet-4.6-latest",
        ] {
            assert_eq!(model_key(alias), key, "{alias}");
        }
        assert_ne!(model_key("claude-sonnet-4.5"), key);
        assert_ne!(model_key("claude-opus-4.6"), key);
        assert_eq!(model_key("GPT-4.1"), model_key("gpt-4.1"));
    }
}
