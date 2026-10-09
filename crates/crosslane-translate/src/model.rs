//! Model mapping and adaptive-thinking decisions.

use crosslane_core::config::{ModelMappingRule, ModelMappingSchedule};
use rand::distributions::{Distribution, WeightedIndex};

/// Resolve an exact ID or an equivalent client alias from a provider catalog.
/// Unlike dynamic Kiro resolution, this never chooses a different version.
/// Exact IDs take precedence; an ambiguous alias must be mapped explicitly.
pub fn resolve_model_alias(model: &str, available: &[String]) -> Option<String> {
    let model = model.trim();
    if let Some(exact) = available
        .iter()
        .find(|candidate| candidate.eq_ignore_ascii_case(model))
    {
        return Some(exact.clone());
    }
    let requested = crosslane_core::model_policy::model_key(model);
    let mut matches = available
        .iter()
        .filter(|candidate| crosslane_core::model_policy::model_key(candidate) == requested);
    let matched = matches.next()?;
    matches
        .all(|candidate| candidate == matched)
        .then(|| matched.clone())
}

/// Resolve a client model name against the exact IDs returned by one account.
/// Matching mirrors the TypeScript authority: exact, normalized/prefix, then
/// highest version in the same model family.
pub fn resolve_dynamic_model(model: &str, available: &[String]) -> Option<String> {
    if available.is_empty() {
        return None;
    }
    if let Some(exact) = available
        .iter()
        .find(|candidate| candidate.eq_ignore_ascii_case(model))
    {
        return Some(exact.clone());
    }
    let model = strip_discovery_prefix(model);
    let requested = normalize_model_name(model);
    if let Some(exact) = available
        .iter()
        .find(|candidate| normalize_model_name(candidate) == requested)
    {
        return Some(exact.clone());
    }
    let prefixed = available
        .iter()
        .filter(|candidate| {
            let normalized = normalize_model_name(candidate);
            prefix_version_match(&requested, &normalized)
                || prefix_version_match(&normalized, &requested)
        })
        .max_by(
            |left, right| match (model_family(left), model_family(right)) {
                (Some((left_family, left_version)), Some((right_family, right_version)))
                    if left_family == right_family =>
                {
                    compare_version(&left_version, &right_version)
                }
                _ => normalize_model_name(left).cmp(&normalize_model_name(right)),
            },
        );
    if let Some(candidate) = prefixed {
        return Some(candidate.clone());
    }
    // Some clients put the Claude family after the version
    // (`claude-4.6-sonnet`) while Kiro returns it before the version
    // (`claude-sonnet-4.6`). Treat separators and token order as aliases of
    // the same discovered model, but keep family isolation so Opus, Sonnet,
    // and Haiku can never resolve across one another.
    if let Some(candidate) = available
        .iter()
        .filter(|candidate| token_model_match(model, candidate))
        .max_by(|left, right| compare_token_matches(left, right))
    {
        return Some(candidate.clone());
    }
    let family = model_family(model)?;
    available
        .iter()
        .filter_map(|candidate| {
            let (candidate_family, version) = model_family(candidate)?;
            (candidate_family == family.0).then_some((version, candidate))
        })
        .max_by(|left, right| compare_version(&left.0, &right.0))
        .map(|(_, candidate)| candidate.clone())
}

fn model_tokens(model: &str) -> Vec<String> {
    model
        .to_ascii_lowercase()
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty() && *token != "latest" && *token != "model")
        .map(str::to_owned)
        .collect()
}

fn token_model_match(requested: &str, candidate: &str) -> bool {
    const CLAUDE_FAMILIES: [&str; 3] = ["opus", "sonnet", "haiku"];
    let requested = model_tokens(requested);
    let candidate = model_tokens(candidate);
    if requested.is_empty() || !requested.iter().all(|token| candidate.contains(token)) {
        return false;
    }
    CLAUDE_FAMILIES.iter().all(|family| {
        requested.iter().any(|token| token == family)
            == candidate.iter().any(|token| token == family)
    })
}

fn compare_token_matches(left: &str, right: &str) -> std::cmp::Ordering {
    match (model_family(left), model_family(right)) {
        (Some((left_family, left_version)), Some((right_family, right_version)))
            if left_family == right_family =>
        {
            compare_version(&left_version, &right_version)
                .then_with(|| right.len().cmp(&left.len()))
                .then_with(|| left.cmp(right))
        }
        _ => right.len().cmp(&left.len()).then_with(|| left.cmp(right)),
    }
}

pub fn can_resolve_dynamic_model(model: &str, available: &[String]) -> bool {
    resolve_dynamic_model(model, available).is_some()
}

/// Claude Code discovers only IDs containing `claude` or `anthropic`.
pub fn claude_discovery_model_id(model: &str) -> String {
    let lower = model.to_ascii_lowercase();
    if lower.contains("claude") || lower.contains("anthropic") {
        model.into()
    } else {
        format!("anthropic.{model}")
    }
}

fn strip_discovery_prefix(model: &str) -> &str {
    model
        .get(..10)
        .filter(|prefix| prefix.eq_ignore_ascii_case("anthropic."))
        .map(|_| &model[10..])
        .unwrap_or(model)
}

fn normalize_model_name(model: &str) -> String {
    let normalized = model
        .trim()
        .to_ascii_lowercase()
        .replace(['_', '.'], "-")
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    expand_compact_claude_alias(&normalized).unwrap_or(normalized)
}

/// Expands common client shorthands such as `opus5` and `sonnet4.6` into the
/// same normalized family/version shape as Kiro's canonical model IDs. Only
/// recognized Claude families followed exclusively by a numeric version are
/// expanded, so arbitrary model names remain untouched.
fn expand_compact_claude_alias(normalized: &str) -> Option<String> {
    let alias = normalized
        .strip_prefix("claude-")
        .or_else(|| normalized.strip_prefix("claude"))
        .unwrap_or(normalized);
    for family in ["opus", "sonnet", "haiku"] {
        let Some(version) = alias.strip_prefix(family) else {
            continue;
        };
        let version = version.strip_prefix('-').unwrap_or(version);
        if version.is_empty()
            || !version
                .chars()
                .all(|character| character.is_ascii_digit() || matches!(character, '-' | '.'))
            || !version.chars().any(|character| character.is_ascii_digit())
        {
            return None;
        }
        let version = version
            .replace('.', "-")
            .split('-')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        return Some(format!("claude-{family}-{version}"));
    }
    None
}

fn prefix_version_match(longer: &str, shorter: &str) -> bool {
    let Some(rest) = longer.strip_prefix(shorter) else {
        return false;
    };
    rest.is_empty()
        || rest
            .strip_prefix('-')
            .and_then(|rest| rest.chars().next())
            .is_some_and(|next| next.is_ascii_digit())
}

fn model_family(model: &str) -> Option<(String, Vec<u32>)> {
    let normalized = normalize_model_name(model);
    let parts = normalized.split('-').collect::<Vec<_>>();
    let first_version = parts
        .iter()
        .position(|part| part.chars().next().is_some_and(|ch| ch.is_ascii_digit()))?;
    if first_version == 0 {
        return None;
    }
    let family = parts[..first_version].join("-");
    let version = parts[first_version..]
        .iter()
        .take_while(|part| part.chars().all(|ch| ch.is_ascii_digit()))
        .filter_map(|part| part.parse().ok())
        .collect::<Vec<_>>();
    (!version.is_empty()).then_some((family, version))
}

fn compare_version(left: &[u32], right: &[u32]) -> std::cmp::Ordering {
    (0..left.len().max(right.len()))
        .map(|index| {
            left.get(index)
                .copied()
                .unwrap_or_default()
                .cmp(&right.get(index).copied().unwrap_or_default())
        })
        .find(|ordering| !ordering.is_eq())
        .unwrap_or(std::cmp::Ordering::Equal)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRoute {
    pub original: String,
    pub mapped: String,
    pub rule: Option<String>,
    /// Index of the matched rule in the configuration slice. This lets the
    /// provider gateway inspect the exact rule without relying on display
    /// names, which are not required to be unique in legacy configurations.
    pub rule_index: Option<usize>,
}

/// Provider-aware conditions used by the shared model mapping engine.
#[derive(Debug, Clone, Copy, Default)]
pub struct ModelMappingContext<'a> {
    pub provider_id: &'a str,
    pub service_id: Option<&'a str>,
    pub api_key_id: Option<&'a str>,
    pub remaining_percent: Option<f64>,
}

pub fn map_model(
    model: &str,
    rules: &[ModelMappingRule],
    api_key_id: Option<&str>,
    remaining_percent: Option<f64>,
    default_model: &str,
) -> ModelRoute {
    map_model_for_provider(
        model,
        rules,
        ModelMappingContext {
            provider_id: "kiro",
            service_id: None,
            api_key_id,
            remaining_percent,
        },
        default_model,
    )
}

/// Applies the same mapping rules for any provider while preserving the legacy
/// rule scope (`providers = []` means Kiro only).
pub fn map_model_for_provider(
    model: &str,
    rules: &[ModelMappingRule],
    context: ModelMappingContext<'_>,
    default_model: &str,
) -> ModelRoute {
    let mut ordered = rules
        .iter()
        .enumerate()
        .filter(|(_, rule)| rule.enabled)
        .collect::<Vec<_>>();
    ordered.sort_by_key(|(_, rule)| rule.priority);
    for (rule_index, rule) in ordered {
        let provider_matches = if rule.providers.is_empty() {
            context.provider_id == "kiro"
        } else {
            rule.providers
                .iter()
                .any(|provider| provider == context.provider_id)
        };
        if !provider_matches {
            continue;
        }
        if !rule.service_ids.is_empty()
            && !context
                .service_id
                .is_some_and(|service| rule.service_ids.iter().any(|item| item == service))
        {
            continue;
        }
        if !schedule_active(rule.schedule.as_ref()) {
            continue;
        }
        if !rule
            .source_models
            .iter()
            .any(|pattern| glob(pattern, model) || glob(pattern, strip_discovery_prefix(model)))
        {
            continue;
        }
        if let Some(ids) = &rule.api_key_ids {
            if !ids.is_empty()
                && !context
                    .api_key_id
                    .is_some_and(|id| ids.iter().any(|item| item == id))
            {
                continue;
            }
        }
        if let Some(maximum) = rule.max_remaining_credit_percent {
            if context
                .remaining_percent
                .is_none_or(|value| value >= maximum)
            {
                continue;
            }
        }
        if let Some(mapped) = choose_target(rule) {
            let mapped = mapped
                .split_once('/')
                .filter(|(provider, _)| *provider == context.provider_id)
                .map_or(mapped.as_str(), |(_, model)| model)
                .to_owned();
            return ModelRoute {
                original: model.into(),
                mapped,
                rule: Some(rule.name.clone()),
                rule_index: Some(rule_index),
            };
        }
    }
    ModelRoute {
        original: model.into(),
        mapped: if default_model.trim().is_empty() {
            strip_discovery_prefix(model).into()
        } else {
            default_model.into()
        },
        rule: None,
        rule_index: None,
    }
}

/// Matches a model permission pattern using the mapping engine's anchored glob
/// semantics.
pub fn model_glob_matches(pattern: &str, model: &str) -> bool {
    glob(pattern, model)
}

fn schedule_active(schedule: Option<&ModelMappingSchedule>) -> bool {
    let Some(schedule) = schedule else {
        return true;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0);
    let (day, minutes) = local_day_and_minutes(now / 1_000);
    schedule_active_at(schedule, now, day, minutes)
}

fn schedule_active_at(
    schedule: &ModelMappingSchedule,
    timestamp_ms: i64,
    day: u8,
    minutes: u16,
) -> bool {
    let mode = if schedule.mode == "always"
        && (schedule.start.is_some()
            || schedule.end.is_some()
            || schedule.start_minutes.is_some()
            || schedule.end_minutes.is_some()
            || schedule.days.is_some()
            || schedule.days_of_week.is_some())
    {
        "daily"
    } else if schedule.mode == "always"
        && (schedule.start_at.is_some() || schedule.end_at.is_some())
    {
        "range"
    } else {
        schedule.mode.as_str()
    };
    match mode {
        "range" => {
            schedule.start_at.is_none_or(|start| timestamp_ms >= start)
                && schedule.end_at.is_none_or(|end| timestamp_ms <= end)
        }
        "daily" => {
            let configured_days = schedule.days_of_week.clone().or_else(|| {
                schedule.days.as_ref().map(|days| {
                    days.iter()
                        .filter_map(|day| parse_weekday(day))
                        .collect::<Vec<_>>()
                })
            });
            if configured_days
                .as_ref()
                .is_some_and(|days| !days.is_empty() && !days.contains(&day))
            {
                return false;
            }
            let start = schedule
                .start_minutes
                .or_else(|| schedule.start.as_deref().and_then(parse_clock));
            let end = schedule
                .end_minutes
                .or_else(|| schedule.end.as_deref().and_then(parse_clock));
            match (start, end) {
                (Some(start), Some(end)) if start <= end => minutes >= start && minutes < end,
                (Some(start), Some(end)) => minutes >= start || minutes < end,
                _ => true,
            }
        }
        _ => true,
    }
}

fn parse_clock(value: &str) -> Option<u16> {
    let (hour, minute) = value.split_once(':')?;
    let hour = hour.parse::<u16>().ok()?;
    let minute = minute.parse::<u16>().ok()?;
    (hour < 24 && minute < 60).then_some(hour * 60 + minute)
}

fn parse_weekday(value: &str) -> Option<u8> {
    match value.trim().to_ascii_lowercase().as_str() {
        "sun" | "sunday" => Some(0),
        "mon" | "monday" => Some(1),
        "tue" | "tues" | "tuesday" => Some(2),
        "wed" | "wednesday" => Some(3),
        "thu" | "thur" | "thurs" | "thursday" => Some(4),
        "fri" | "friday" => Some(5),
        "sat" | "saturday" => Some(6),
        _ => None,
    }
}

#[cfg(unix)]
fn local_day_and_minutes(timestamp: i64) -> (u8, u16) {
    let timestamp = timestamp as libc::time_t;
    let mut output = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: localtime_r writes one tm to the provided non-null output pointer.
    let result = unsafe { libc::localtime_r(&timestamp, output.as_mut_ptr()) };
    if result.is_null() {
        return utc_day_and_minutes(timestamp as i64);
    }
    // SAFETY: a non-null localtime_r result initialized the output value.
    let output = unsafe { output.assume_init() };
    (
        output.tm_wday.clamp(0, 6) as u8,
        (output.tm_hour.clamp(0, 23) * 60 + output.tm_min.clamp(0, 59)) as u16,
    )
}

#[cfg(not(unix))]
fn local_day_and_minutes(timestamp: i64) -> (u8, u16) {
    utc_day_and_minutes(timestamp)
}

fn utc_day_and_minutes(timestamp: i64) -> (u8, u16) {
    let seconds = timestamp.rem_euclid(86_400);
    let days = timestamp.div_euclid(86_400);
    (((days + 4).rem_euclid(7)) as u8, (seconds / 60) as u16)
}

fn choose_target(rule: &ModelMappingRule) -> Option<String> {
    if rule.target_models.is_empty() {
        return None;
    }
    if rule.kind != "loadbalance" || rule.target_models.len() == 1 {
        return rule.target_models.first().cloned();
    }
    let weights = rule
        .weights
        .clone()
        .filter(|weights| weights.len() == rule.target_models.len())
        .unwrap_or_else(|| vec![1; rule.target_models.len()]);
    let distribution = WeightedIndex::new(weights).ok()?;
    Some(rule.target_models[distribution.sample(&mut rand::thread_rng())].clone())
}

fn glob(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    match pattern.split_once('*') {
        Some((prefix, suffix)) => value.starts_with(prefix) && value.ends_with(suffix),
        None => pattern.eq_ignore_ascii_case(value),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingReason {
    ClientDisabled,
    ModelDisabled,
    /// No recognized upstream parameter schema; not a claim about model reasoning.
    ModelControlsUnavailable,
    Standard,
}

pub fn should_enable_thinking(client_enabled: bool, model_enabled: bool) -> (bool, ThinkingReason) {
    if !client_enabled {
        return (false, ThinkingReason::ClientDisabled);
    }
    if !model_enabled {
        return (false, ThinkingReason::ModelDisabled);
    }
    (true, ThinkingReason::Standard)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinkingDecision {
    pub enabled: bool,
    pub reason: ThinkingReason,
    pub effort: Option<String>,
}

pub fn thinking_enabled_for_model(
    model: &str,
    modes: &std::collections::BTreeMap<String, bool>,
) -> bool {
    modes
        .iter()
        .find(|(key, _)| {
            model.eq_ignore_ascii_case(key)
                || model
                    .to_ascii_lowercase()
                    .starts_with(&format!("{}-", key.to_ascii_lowercase()))
        })
        .map(|(_, enabled)| *enabled)
        .unwrap_or(true)
}

pub fn apply_adaptive_thinking(
    payload: &mut crate::KiroPayload,
    additional_fields_schema: Option<&serde_json::Value>,
    model_enabled: bool,
) -> ThinkingDecision {
    // Rebuild from immutable client intent, never from a previous model's
    // transformed fields (which may have a different path or effort enum).
    let intent = payload.model_request_intent.clone().unwrap_or_default();
    let thinking = intent.thinking.as_ref();
    let explicit_effort = intent.effort.as_deref();
    let explicitly_disabled =
        thinking.is_some_and(|thinking| thinking.r#type.eq_ignore_ascii_case("disabled"));
    let client_enabled = !explicitly_disabled
        && (explicit_effort.is_some()
            || thinking.is_some_and(|thinking| {
                matches!(
                    thinking.r#type.to_ascii_lowercase().as_str(),
                    "enabled" | "adaptive"
                )
            }));
    // The reference never switches thinking off just because this is a tool
    // result or control turn. Keep only the explicit operator-level deny rule.
    let (enabled, reason) = should_enable_thinking(client_enabled, model_enabled);
    if !enabled {
        // Match the reference gateway's omission policy. In particular, do not
        // invent a reasoning.effort=none or thinking:disabled Kiro extension.
        // Response filtering also suppresses unsolicited default reasoning.
        payload.additional_model_request_fields = None;
        return ThinkingDecision {
            enabled,
            reason,
            effort: None,
        };
    }

    let Some((fields, effort)) =
        native_thinking_fields(thinking, explicit_effort, additional_fields_schema)
    else {
        // Kiro rejects even an empty additionalModelRequestFields object for
        // models such as Haiku 4.5. Omit the entire field, including any stale
        // controls from an earlier target, but preserve the original intent.
        payload.additional_model_request_fields = None;
        return ThinkingDecision {
            enabled: false,
            reason: ThinkingReason::ModelControlsUnavailable,
            effort: None,
        };
    };
    payload.additional_model_request_fields = Some(fields);
    ThinkingDecision {
        enabled: true,
        reason,
        effort: Some(effort),
    }
}

fn native_thinking_fields(
    thinking: Option<&crate::ThinkingConfig>,
    explicit_effort: Option<&str>,
    schema: Option<&serde_json::Value>,
) -> Option<(serde_json::Value, String)> {
    // Like chaogei/Kiro-account-manager, metadata chooses a known thinking
    // dialect only when it actually advertises effort values. It is not a
    // complete JSON Schema allowlist (output_config also requires thinking).
    for path in ["output_config", "reasoning"] {
        let field_schema = schema.and_then(|schema| schema.get("properties")?.get(path));
        let allowed = effort_values(field_schema);
        if allowed.is_empty() {
            continue;
        }
        let requested = explicit_effort
            .map(str::to_ascii_lowercase)
            .or_else(|| {
                thinking
                    .filter(|thinking| thinking.r#type == "enabled")
                    .and_then(|thinking| thinking.budget_tokens)
                    .map(|budget| budget_to_effort(budget).to_owned())
            })
            // getThinkingConfig in the reference does not read JSON Schema's
            // default, so an otherwise unspecified effort is always high.
            .unwrap_or_else(|| "high".into());
        let effort = select_effort(&requested, &allowed).to_owned();
        let fields = if path == "output_config" {
            serde_json::json!({
                "thinking": {"type":"adaptive", "display":"summarized"},
                "output_config": {"effort":effort}
            })
        } else {
            serde_json::json!({"reasoning":{"effort":effort}})
        };
        return Some((fields, effort));
    }

    // Deliberately diverge from the reference's adaptive fallback. Client
    // intent (or an operator allow rule) is not evidence of upstream support.
    // Missing, incomplete and unrecognized schemas must not invent controls.
    None
}

fn effort_values(schema: Option<&serde_json::Value>) -> Vec<&str> {
    schema
        .and_then(|schema| schema.pointer("/properties/effort/enum"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .collect()
}

fn select_effort<'a>(requested: &'a str, allowed: &'a [&'a str]) -> &'a str {
    if allowed.contains(&requested) {
        return requested;
    }
    // Preserve the advertised ordering, exactly as the reference does. Its
    // implementation selects the final entry, not the nearest numeric rank.
    allowed
        .last()
        .copied()
        .filter(|effort| !effort.is_empty())
        .unwrap_or("high")
}

fn budget_to_effort(budget: u32) -> &'static str {
    match budget {
        0..=4_000 => "low",
        4_001..=16_000 => "medium",
        16_001..=64_000 => "high",
        _ => "xhigh",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_match_is_anchored() {
        assert!(glob("claude-*", "claude-sonnet-4"));
        assert!(!glob("claude-*", "x-claude-sonnet-4"));
    }

    #[test]
    fn explicit_client_disable_takes_precedence() {
        assert_eq!(
            should_enable_thinking(false, true),
            (false, ThinkingReason::ClientDisabled)
        );
    }

    #[test]
    fn explicit_operator_disable_takes_precedence() {
        assert_eq!(
            should_enable_thinking(true, false),
            (false, ThinkingReason::ModelDisabled)
        );
    }

    #[test]
    fn unavailable_model_controls_are_omitted_for_both_protocols() {
        for model in ["claude-haiku-4.5", "unknown-model"] {
            for schema in [
                None,
                Some(serde_json::Value::Null),
                Some(serde_json::json!({"properties":{}})),
                Some(serde_json::json!({"properties":{"thinking":{"type":"object"}}})),
                Some(
                    serde_json::json!({"properties":{"output_config":{"properties":{
                        "effort":{"enum":[]}
                    }}}}),
                ),
                Some(serde_json::json!({"properties":{"reasoning":{"properties":{
                    "effort":{"enum":[null,42,""]}
                }}}})),
            ] {
                for control in [
                    serde_json::json!({"thinking":{"type":"adaptive"}}),
                    serde_json::json!({"thinking":{"type":"enabled","budget_tokens":1024}}),
                    serde_json::json!({"thinking":{"type":"disabled"}}),
                    serde_json::json!({"reasoning_effort":"high"}),
                ] {
                    let mut request = serde_json::json!({
                        "model":model, "max_tokens":4096,
                        "messages":[{"role":"user","content":"pong"}]
                    });
                    request
                        .as_object_mut()
                        .unwrap()
                        .extend(control.as_object().unwrap().clone());
                    let mut options = crate::TranslationOptions::new(model, "AI_EDITOR");
                    options.additional_model_request_fields_schema = schema.clone();
                    let openai: crate::OpenAiRequest =
                        serde_json::from_value(request.clone()).unwrap();
                    crate::validate_openai(&openai).unwrap();
                    let mut payloads = vec![crate::openai_to_kiro(&openai, &options)];
                    if control.get("reasoning_effort").is_none() {
                        let claude: crate::ClaudeRequest = serde_json::from_value(request).unwrap();
                        crate::validate_claude(&claude).unwrap();
                        payloads.push(crate::claude_to_kiro(&claude, &options));
                    }
                    for mut payload in payloads {
                        let wire = serde_json::to_value(&payload).unwrap();
                        assert!(
                            wire.get("additionalModelRequestFields").is_none(),
                            "model={model} schema={schema:?} control={control}: {wire}"
                        );
                        // Re-preparation must also remove fields left by an earlier model.
                        payload.additional_model_request_fields = Some(serde_json::json!({
                            "thinking":{"type":"adaptive"}, "output_config":{"effort":"high"}
                        }));
                        let decision = apply_adaptive_thinking(&mut payload, schema.as_ref(), true);
                        assert!(!decision.enabled);
                        assert_eq!(
                            decision.reason,
                            if control.pointer("/thinking/type")
                                == Some(&serde_json::json!("disabled"))
                            {
                                ThinkingReason::ClientDisabled
                            } else {
                                ThinkingReason::ModelControlsUnavailable
                            }
                        );
                        assert!(!payload.thinking_enabled());
                        assert!(serde_json::to_value(&payload)
                            .unwrap()
                            .get("additionalModelRequestFields")
                            .is_none());
                        assert!(payload.model_request_intent.is_some());
                    }
                }
            }
        }
    }

    #[test]
    fn recognized_model_controls_are_not_blocked_by_a_model_name_heuristic() {
        let request: crate::ClaudeRequest = serde_json::from_value(serde_json::json!({
            "model":"claude-haiku-4.5", "max_tokens":4096,
            "thinking":{"type":"adaptive"},
            "messages":[{"role":"user","content":"explain"}]
        }))
        .unwrap();
        let mut payload = crate::claude_to_kiro(
            &request,
            &crate::TranslationOptions::new("claude-haiku-4.5", "AI_EDITOR"),
        );
        // Hypothetical future support: metadata, not a Haiku denylist, is authoritative.
        let schema = serde_json::json!({"properties":{"reasoning":{"properties":{
            "effort":{"enum":["low","high"]}
        }}}});
        let decision = apply_adaptive_thinking(&mut payload, Some(&schema), true);
        assert!(decision.enabled);
        assert_eq!(
            payload.additional_model_request_fields,
            Some(serde_json::json!({"reasoning":{"effort":"high"}}))
        );
        let denied = apply_adaptive_thinking(&mut payload, Some(&schema), false);
        assert_eq!(denied.reason, ThinkingReason::ModelDisabled);
        assert!(payload.additional_model_request_fields.is_none());
    }

    #[test]
    fn request_body_thinking_type_controls_native_parameters_without_prompt_injection() {
        let request = |kind: &str| {
            serde_json::from_value::<crate::ClaudeRequest>(serde_json::json!({
                "model":"claude-sonnet-4.6",
                "messages":[{"role":"user","content":"hello"}],
                "max_tokens":4096,
                "thinking":{"type":kind}
            }))
            .expect("Claude request")
        };

        let disabled = request("disabled");
        let mut disabled_payload = crate::claude_to_kiro(
            &disabled,
            &crate::TranslationOptions::new("claude-sonnet-4.6", "AI_EDITOR"),
        );
        let disabled_decision = apply_adaptive_thinking(&mut disabled_payload, None, true);
        assert_eq!(disabled_decision.reason, ThinkingReason::ClientDisabled);
        assert!(!disabled_decision.enabled);
        assert!(!disabled_payload
            .conversation_state
            .current_message
            .user_input_message
            .content
            .contains("<thinking_mode>"));

        let adaptive = request("adaptive");
        let mut adaptive_payload = crate::claude_to_kiro(
            &adaptive,
            &crate::TranslationOptions::new("claude-sonnet-4.6", "AI_EDITOR"),
        );
        let schema = serde_json::json!({"properties":{"output_config":{"properties":{
            "effort":{"enum":["low","high"]}
        }}}});
        let adaptive_decision = apply_adaptive_thinking(&mut adaptive_payload, Some(&schema), true);
        assert!(adaptive_decision.enabled);
        assert_eq!(
            adaptive_payload
                .additional_model_request_fields
                .as_ref()
                .and_then(|fields| fields.pointer("/thinking/type"))
                .and_then(serde_json::Value::as_str),
            Some("adaptive")
        );
    }

    #[test]
    fn native_thinking_respects_model_schema_and_explicit_disable() {
        let build = |kind: &str| {
            serde_json::from_value::<crate::ClaudeRequest>(serde_json::json!({
                "model":"claude-sonnet-4.6",
                "messages":[{"role":"user","content":"explain this carefully"}],
                "max_tokens":8192,
                "top_k":0,
                "thinking":{"type":kind},
                "output_config":{"effort":"high"}
            }))
            .expect("Claude request")
        };
        let schema = serde_json::json!({
            "properties":{
                "top_k":{"type":"integer"},
                "output_config":{"properties":{
                    "effort":{"enum":["low","medium"]}
                }}
            }
        });

        let adaptive = build("adaptive");
        let mut payload = crate::claude_to_kiro(
            &adaptive,
            &crate::TranslationOptions::new("claude-sonnet-4.6", "AI_EDITOR"),
        );
        let decision = apply_adaptive_thinking(&mut payload, Some(&schema), true);
        assert!(decision.enabled);
        let fields = payload
            .additional_model_request_fields
            .as_ref()
            .expect("additional fields");
        assert!(fields.get("top_k").is_none());
        assert_eq!(
            fields
                .pointer("/output_config/effort")
                .and_then(serde_json::Value::as_str),
            Some("medium")
        );
        assert_eq!(
            fields["thinking"],
            serde_json::json!({"type":"adaptive","display":"summarized"})
        );

        let disabled = build("disabled");
        let mut payload = crate::claude_to_kiro(
            &disabled,
            &crate::TranslationOptions::new("claude-sonnet-4.6", "AI_EDITOR"),
        );
        let decision = apply_adaptive_thinking(&mut payload, Some(&schema), true);
        assert!(!decision.enabled);
        assert_eq!(decision.reason, ThinkingReason::ClientDisabled);
        assert!(payload.additional_model_request_fields.is_none());
    }

    #[test]
    fn top_k_is_omitted_even_when_present_in_model_metadata() {
        for kind in ["adaptive", "disabled"] {
            for schema in [
                None,
                Some(serde_json::json!({"properties":{}})),
                Some(serde_json::json!({"properties":{"thinking":{"type":"object"}}})),
                Some(
                    serde_json::json!({"properties":{"thinking":{"type":"object"},"top_k":{"type":"integer"}}}),
                ),
            ] {
                let request: crate::ClaudeRequest = serde_json::from_value(serde_json::json!({
                    "model":"claude-sonnet-4.6", "max_tokens":4096, "top_k":42,
                    "thinking":{"type":kind}, "messages":[{"role":"user","content":"explain"}]
                }))
                .expect("request");
                let mut payload = crate::claude_to_kiro(
                    &request,
                    &crate::TranslationOptions::new("claude-sonnet-4.6", "AI_EDITOR"),
                );
                apply_adaptive_thinking(&mut payload, schema.as_ref(), true);
                let top_k = payload
                    .additional_model_request_fields
                    .as_ref()
                    .and_then(|fields| fields.get("top_k"));
                assert!(top_k.is_none(), "kind={kind} schema={schema:?}");
                if kind == "disabled" {
                    assert!(payload.additional_model_request_fields.is_none());
                }
            }
        }
    }

    #[test]
    fn fallback_rebuilds_thinking_from_original_effort_without_leaking_unknown_fields() {
        let request: crate::ClaudeRequest = serde_json::from_value(serde_json::json!({
            "model":"claude-sonnet-4.6", "max_tokens":100000, "top_k":42,
            "thinking":{"type":"enabled","budget_tokens":64001},
            "output_config":{"effort":"xhigh"},
            "messages":[{"role":"user","content":"explain"}]
        }))
        .expect("request");
        let mut payload = crate::claude_to_kiro(
            &request,
            &crate::TranslationOptions::new("claude-sonnet-4.6", "AI_EDITOR"),
        );
        payload.additional_model_request_fields =
            Some(serde_json::json!({"top_k":42,"future_field":true}));
        apply_adaptive_thinking(&mut payload, None, true);
        assert!(payload.additional_model_request_fields.is_none());
        let output_schema = serde_json::json!({"properties":{"output_config":{"properties":{"effort":{"enum":["low","medium"]}}}}});
        apply_adaptive_thinking(&mut payload, Some(&output_schema), true);
        assert_eq!(
            payload.additional_model_request_fields.as_ref().unwrap()["output_config"]["effort"],
            "medium"
        );
        let reasoning_schema = serde_json::json!({"properties":{"reasoning":{"properties":{"effort":{"enum":["low","medium","high","xhigh"]}}}}});
        apply_adaptive_thinking(&mut payload, Some(&reasoning_schema), true);
        assert_eq!(
            payload.additional_model_request_fields,
            Some(serde_json::json!({"reasoning":{"effort":"xhigh"}}))
        );
        apply_adaptive_thinking(&mut payload, None, true);
        assert!(payload.additional_model_request_fields.is_none());
    }

    #[test]
    fn native_thinking_supports_reasoning_effort_schema() {
        let request = serde_json::from_value::<crate::OpenAiRequest>(serde_json::json!({
            "model":"claude-sonnet-4.6",
            "messages":[{"role":"user","content":"reason"}],
            "max_tokens":8192,
            "reasoning_effort":"high"
        }))
        .expect("OpenAI request");
        let schema = serde_json::json!({
            "properties":{"reasoning":{"properties":{
                "effort":{"enum":["low","high"]}
            }}}
        });
        let mut payload = crate::openai_to_kiro(
            &request,
            &crate::TranslationOptions::new("claude-sonnet-4.6", "AI_EDITOR"),
        );
        let decision = apply_adaptive_thinking(&mut payload, Some(&schema), true);
        assert!(decision.enabled);
        assert_eq!(
            payload
                .additional_model_request_fields
                .as_ref()
                .and_then(|fields| fields.pointer("/reasoning/effort"))
                .and_then(serde_json::Value::as_str),
            Some("high")
        );
    }

    #[test]
    fn manual_thinking_budgets_map_directly_to_reference_effort_levels() {
        let schema = serde_json::json!({"properties":{"output_config":{"properties":{
            "effort":{"enum":["low","medium","high","xhigh"]}
        }}}});
        for (budget, expected) in [
            (1024, "low"),
            (4000, "low"),
            (4001, "medium"),
            (16000, "medium"),
            (16001, "high"),
            (32000, "high"),
            (64000, "high"),
            (64001, "xhigh"),
        ] {
            let request: crate::ClaudeRequest = serde_json::from_value(serde_json::json!({
                "model":"claude-sonnet-4.6", "max_tokens":100000,
                "thinking":{"type":"enabled","budget_tokens":budget},
                "messages":[{"role":"user","content":"explain"}]
            }))
            .unwrap();
            crate::validate_claude(&request).unwrap();
            let mut options = crate::TranslationOptions::new("claude-sonnet-4.6", "AI_EDITOR");
            options.additional_model_request_fields_schema = Some(schema.clone());
            let payload = crate::claude_to_kiro(&request, &options);
            let fields = payload.additional_model_request_fields.unwrap();
            assert_eq!(
                fields["output_config"]["effort"], expected,
                "budget={budget}"
            );
            assert_eq!(fields["thinking"]["type"], "adaptive");
            assert!(fields["thinking"].get("budget_tokens").is_none());
        }
    }

    #[test]
    fn claude_explicit_effort_overrides_budget_and_keeps_display_filtering() {
        let request: crate::ClaudeRequest = serde_json::from_value(serde_json::json!({
            "model":"claude-sonnet-4.6", "max_tokens":4096,
            "thinking":{"type":"enabled","budget_tokens":1024,"display":"omitted"},
            "output_config":{"effort":"high"},
            "tools":[{"name":"read","input_schema":{"type":"object"}}],
            "messages":[{"role":"user","content":"explain"}]
        }))
        .unwrap();
        let schema = serde_json::json!({"properties":{"output_config":{"properties":{
            "effort":{"enum":["low","medium","high"]}
        }}}});
        let mut payload = crate::claude_to_kiro(
            &request,
            &crate::TranslationOptions::new("claude-sonnet-4.6", "AI_EDITOR"),
        );
        let decision = apply_adaptive_thinking(&mut payload, Some(&schema), true);
        assert_eq!(decision.reason, ThinkingReason::Standard);
        assert_eq!(decision.effort.as_deref(), Some("high"));
        assert_eq!(
            payload.additional_model_request_fields,
            Some(serde_json::json!({
                "thinking":{"type":"adaptive","display":"summarized"},"output_config":{"effort":"high"}
            }))
        );
    }

    #[test]
    fn thinking_metadata_defaults_and_incomplete_schema_omission() {
        let request: crate::ClaudeRequest = serde_json::from_value(serde_json::json!({
            "model":"claude-sonnet-4.6", "max_tokens":4096,
            "thinking":{"type":"adaptive","display":"omitted"},
            "messages":[{"role":"user","content":"explain"}]
        }))
        .unwrap();
        let mut payload = crate::claude_to_kiro(
            &request,
            &crate::TranslationOptions::new("claude-sonnet-4.6", "AI_EDITOR"),
        );
        assert!(payload.additional_model_request_fields.is_none());
        for (schema, path, effort) in [
            (
                serde_json::json!({"properties":{"output_config":{"properties":{"effort":{"enum":["low","medium","high"]}}}}}),
                "output_config",
                "high",
            ),
            (
                serde_json::json!({"properties":{"output_config":{"properties":{"effort":{"enum":["low","medium","high"],"default":"medium"}}}}}),
                "output_config",
                "high",
            ),
            (
                serde_json::json!({"properties":{"output_config":{},"reasoning":{"properties":{"effort":{"enum":["low","high"]}}}}}),
                "reasoning",
                "high",
            ),
        ] {
            apply_adaptive_thinking(&mut payload, Some(&schema), true);
            assert_eq!(
                payload.additional_model_request_fields.as_ref().unwrap()[path]["effort"],
                effort
            );
        }
        for schema in [
            None,
            Some(
                serde_json::json!({"properties":{"output_config":{"properties":{"effort":{"enum":[]}}}}}),
            ),
        ] {
            apply_adaptive_thinking(&mut payload, schema.as_ref(), true);
            assert!(payload.additional_model_request_fields.is_none());
        }
    }

    #[test]
    fn request_intent_is_local_and_preserves_openai_effort_across_model_changes() {
        let request: crate::OpenAiRequest = serde_json::from_value(serde_json::json!({
            "model":"claude-sonnet-4.6", "max_completion_tokens":4096,
            "temperature":0, "top_p":0, "reasoning_effort":"minimal",
            "messages":[{"role":"user","content":"explain"}]
        }))
        .unwrap();
        crate::validate_openai(&request).unwrap();
        let mut payload = crate::openai_to_kiro(
            &request,
            &crate::TranslationOptions::new("claude-sonnet-4.6", "AI_EDITOR"),
        );
        let schema = serde_json::json!({"properties":{"reasoning":{"properties":{
            "effort":{"enum":["low","medium","high"]}
        }}}});
        apply_adaptive_thinking(&mut payload, Some(&schema), true);
        assert_eq!(
            payload.additional_model_request_fields,
            Some(serde_json::json!({"reasoning":{"effort":"high"}}))
        );
        let mut wire = serde_json::to_value(&payload).unwrap();
        assert_eq!(
            wire["inferenceConfig"],
            serde_json::json!({"maxTokens":4096,"temperature":0.0,"topP":0.0})
        );
        assert!(wire.get("modelRequestIntent").is_none());
        wire["modelRequestIntent"] = serde_json::json!({"effort":"max"});
        let parsed: crate::KiroPayload = serde_json::from_value(wire).unwrap();
        assert!(
            parsed.model_request_intent.is_none(),
            "intent cannot be forged through JSON"
        );
        let schema = serde_json::json!({"properties":{"output_config":{"properties":{"effort":{"enum":["minimal","low","high"]}}}}});
        apply_adaptive_thinking(&mut payload, Some(&schema), true);
        assert_eq!(
            payload.additional_model_request_fields.as_ref().unwrap()["output_config"]["effort"],
            "minimal"
        );
    }

    #[test]
    fn inference_wire_contract_preserves_zero_values_and_omits_local_stop_controls() {
        let request: crate::ClaudeRequest = serde_json::from_value(serde_json::json!({
            "model":"claude-sonnet-4.6", "max_tokens":1,
            "temperature":0, "top_p":0, "top_k":42, "stop_sequences":["STOP"],
            "messages":[{"role":"user","content":"explain"}]
        }))
        .unwrap();
        let payload = crate::claude_to_kiro(
            &request,
            &crate::TranslationOptions::new("claude-sonnet-4.6", "AI_EDITOR"),
        );
        let wire = serde_json::to_value(payload).unwrap();
        assert_eq!(
            wire["inferenceConfig"],
            serde_json::json!({"maxTokens":1,"temperature":0.0,"topP":0.0})
        );
        assert!(wire.get("additionalModelRequestFields").is_none());
    }

    #[test]
    fn daily_schedule_supports_cross_midnight_and_weekdays() {
        let schedule = ModelMappingSchedule {
            mode: "daily".into(),
            days_of_week: Some(vec![1]),
            days: None,
            start_minutes: Some(22 * 60),
            start: None,
            end_minutes: Some(6 * 60),
            end: None,
            start_at: None,
            end_at: None,
        };
        assert!(schedule_active_at(&schedule, 0, 1, 23 * 60));
        assert!(schedule_active_at(&schedule, 0, 1, 5 * 60));
        assert!(!schedule_active_at(&schedule, 0, 1, 12 * 60));
        assert!(!schedule_active_at(&schedule, 0, 2, 23 * 60));

        let human_schedule = ModelMappingSchedule {
            mode: "always".into(),
            days_of_week: None,
            days: Some(vec!["mon".into(), "tue".into()]),
            start_minutes: None,
            start: Some("09:00".into()),
            end_minutes: None,
            end: Some("18:00".into()),
            start_at: None,
            end_at: None,
        };
        assert!(schedule_active_at(&human_schedule, 0, 1, 12 * 60));
        assert!(!schedule_active_at(&human_schedule, 0, 3, 12 * 60));
        assert!(!schedule_active_at(&human_schedule, 0, 1, 20 * 60));
    }

    #[test]
    fn catalog_aliases_resolve_client_claude_ids_without_changing_versions() {
        let models = vec![
            "claude-opus-4.8".into(),
            "claude-opus-5.5".into(),
            "claude-sonnet-4.8".into(),
            "claude-haiku-4.5".into(),
        ];
        for alias in [
            "claude-opus-4-8",
            " CLAUDE_OPUS_4_8 ",
            "anthropic.claude-opus-4-8",
            "claude-opus-4-8-20261001",
            "claude-opus-4-8-latest",
            "claude-4-8-opus",
            "opus4.8",
        ] {
            assert_eq!(
                resolve_model_alias(alias, &models).as_deref(),
                Some("claude-opus-4.8"),
                "{alias}"
            );
        }
        assert_eq!(
            resolve_model_alias("claude-sonnet-4-8", &models).as_deref(),
            Some("claude-sonnet-4.8")
        );
        assert_eq!(
            resolve_model_alias("claude-haiku-4-5", &models).as_deref(),
            Some("claude-haiku-4.5")
        );
        for unknown in [
            "claude-opus-4-9",
            "claude-opus-4",
            "claude-haiku-4-8",
            "claude-fable-5",
        ] {
            assert!(resolve_model_alias(unknown, &models).is_none(), "{unknown}");
        }
    }

    #[test]
    fn catalog_aliases_preserve_exact_ids_and_reject_ambiguous_matches() {
        let models: Vec<String> = vec!["claude-opus-4.8".into(), "claude-opus-4-8".into()];
        for exact in &models {
            assert_eq!(resolve_model_alias(exact, &models).as_ref(), Some(exact));
        }
        assert!(resolve_model_alias("opus4.8", &models).is_none());
        assert!(resolve_model_alias("claude-opus-4-8", &[]).is_none());
        assert_eq!(
            resolve_model_alias("GPT-4.1", &["gpt-4.1".into()]).as_deref(),
            Some("gpt-4.1")
        );
    }

    #[test]
    fn dynamic_model_resolution_preserves_the_accounts_exact_version() {
        let models = vec![
            "claude-sonnet-4-20250514".into(),
            "claude-sonnet-4-20250801".into(),
            "claude-opus-4-1".into(),
        ];
        assert_eq!(
            resolve_dynamic_model("CLAUDE_SONNET_4", &models).as_deref(),
            Some("claude-sonnet-4-20250801")
        );
        assert_eq!(
            resolve_dynamic_model("claude.opus.4.1", &models).as_deref(),
            Some("claude-opus-4-1")
        );
        assert!(resolve_dynamic_model("unrelated-model", &models).is_none());
    }

    #[test]
    fn dynamic_model_resolution_accepts_reordered_client_aliases() {
        let models = vec!["claude-opus-4.6".into(), "claude-sonnet-4.6".into()];
        assert_eq!(
            resolve_dynamic_model("claude-4.6-sonnet", &models).as_deref(),
            Some("claude-sonnet-4.6")
        );
        assert_eq!(
            resolve_dynamic_model("claude-4-6-opus", &models).as_deref(),
            Some("claude-opus-4.6")
        );
        assert!(resolve_dynamic_model("claude-4.6-haiku", &models).is_none());
        assert_eq!(
            resolve_dynamic_model(
                "claude-4.6-sonnet",
                &["CLAUDE_SONNET_4_6_20260217_V1_0".into()]
            )
            .as_deref(),
            Some("CLAUDE_SONNET_4_6_20260217_V1_0")
        );
    }

    #[test]
    fn dynamic_model_resolution_accepts_compact_claude_aliases() {
        let models = vec![
            "claude-opus-5".into(),
            "claude-sonnet-4.6".into(),
            "claude-haiku-4.5".into(),
        ];
        assert_eq!(
            resolve_dynamic_model("opus5", &models).as_deref(),
            Some("claude-opus-5")
        );
        assert_eq!(
            resolve_dynamic_model("sonnet4.6", &models).as_deref(),
            Some("claude-sonnet-4.6")
        );
        assert_eq!(
            resolve_dynamic_model("claudehaiku4.5", &models).as_deref(),
            Some("claude-haiku-4.5")
        );
        assert!(resolve_dynamic_model("opusfive", &models).is_none());
    }

    #[test]
    fn explicit_mapping_routes_before_automatic_model_resolution() {
        let rule = ModelMappingRule {
            name: "force-opus".into(),
            enabled: true,
            kind: "replace".into(),
            source_models: vec!["claude-4.6-sonnet".into()],
            target_models: vec!["claude-opus-4.6".into()],
            providers: Vec::new(),
            service_ids: Vec::new(),
            priority: 1,
            weights: None,
            max_remaining_credit_percent: None,
            api_key_ids: None,
            schedule: None,
        };
        let route = map_model("claude-4.6-sonnet", &[rule], None, None, "");
        assert_eq!(route.mapped, "claude-opus-4.6");
        assert_eq!(route.rule.as_deref(), Some("force-opus"));
        assert_eq!(
            resolve_dynamic_model(
                &route.mapped,
                &["claude-sonnet-4.6".into(), "claude-opus-4.6".into()]
            )
            .as_deref(),
            Some("claude-opus-4.6")
        );
    }

    #[test]
    fn low_credit_mapping_is_active_all_day_until_credit_recovers() {
        let rule = ModelMappingRule {
            name: "low-credit-fallback".into(),
            enabled: true,
            kind: "replace".into(),
            source_models: vec!["claude-opus-*".into()],
            target_models: vec!["claude-sonnet-4.6".into()],
            providers: Vec::new(),
            service_ids: Vec::new(),
            priority: 1,
            weights: None,
            max_remaining_credit_percent: Some(10.0),
            api_key_ids: None,
            // No schedule is the documented all-day default.
            schedule: None,
        };

        let low = map_model(
            "claude-opus-4.6",
            std::slice::from_ref(&rule),
            None,
            Some(9.9),
            "",
        );
        assert_eq!(low.mapped, "claude-sonnet-4.6");
        assert_eq!(low.rule.as_deref(), Some("low-credit-fallback"));

        let recovered = map_model("claude-opus-4.6", &[rule], None, Some(10.0), "");
        assert_eq!(recovered.mapped, "claude-opus-4.6");
        assert!(recovered.rule.is_none());
    }

    #[test]
    fn same_provider_qualified_targets_become_provider_local_models() {
        let rule = ModelMappingRule {
            name: "qualified-local".into(),
            source_models: vec!["team-fast".into()],
            target_models: vec!["kiro/claude-sonnet-4.6".into()],
            ..ModelMappingRule::default()
        };

        let route = map_model_for_provider(
            "team-fast",
            &[rule],
            ModelMappingContext {
                provider_id: "kiro",
                ..ModelMappingContext::default()
            },
            "",
        );

        assert_eq!(route.mapped, "claude-sonnet-4.6");
        assert_eq!(route.rule.as_deref(), Some("qualified-local"));
    }
}
