//! Positional layout of a translated Kiro request for local prompt-cache
//! accounting. Units are enumerated in cache-prefix order (tool definitions,
//! history messages, current message). Counts use exactly the segments and
//! fixed overheads of `estimate_kiro_payload` on the redacted copy, so their
//! sum is the context estimate `F`. Identities use the unredacted payload, so
//! request-local redaction nonces never break a fingerprint.

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

use super::{
    collect_assistant_message, collect_user_message, KiroPayload, KiroUserInputMessage,
    TokenCountCache, CONVERSATION_OVERHEAD_TOKENS, MESSAGE_OVERHEAD_TOKENS,
    TOOL_SCHEMA_OVERHEAD_TOKENS,
};
use crate::{KiroTool, LocalCachePoint};

struct CountGroup {
    segments: Vec<String>,
    sources: Vec<(usize, Vec<String>, u64)>,
    fixed: usize,
}

#[derive(Debug, PartialEq, Eq)]
struct FragmentRange {
    unit: usize,
    end: usize,
}

/// Match source fragments without revisiting already claimed occurrences.
/// Ranges only accumulate, so an occurrence rejected for overlap can never
/// become available to a later fragment with the same trimmed text.
struct FragmentMapper<'a> {
    segments: &'a [String],
    ranges: Vec<BTreeMap<usize, FragmentRange>>,
    next_matches: HashMap<&'a str, (usize, usize)>,
    exact_segments: Option<HashMap<&'a str, Vec<usize>>>,
}

impl<'a> FragmentMapper<'a> {
    fn new(segments: &'a [String]) -> Self {
        // Most groups have only a few collector fields. Index larger groups
        // so repeated structured fields do not rescan all earlier segments.
        let exact_segments = (segments.len() > 8).then(|| {
            let mut exact = HashMap::<&str, Vec<usize>>::new();
            for (index, text) in segments.iter().enumerate().rev() {
                exact.entry(text).or_default().push(index);
            }
            exact
        });
        Self {
            segments,
            ranges: (0..segments.len()).map(|_| BTreeMap::new()).collect(),
            next_matches: HashMap::new(),
            exact_segments,
        }
    }

    fn assign(&mut self, fragment: &'a str, unit: usize) {
        if fragment.is_empty() {
            return;
        }
        // Structured collector fields match whole, unclaimed segments first,
        // even when an earlier segment contains the trimmed fragment.
        let found = match self.exact_segment(fragment) {
            Some(index) => Some((index, 0, fragment.len())),
            None => self.find_fragment(fragment.trim()),
        };
        if let Some((index, start, end)) = found {
            self.ranges[index].insert(start, FragmentRange { unit, end });
        }
    }

    fn exact_segment(&mut self, fragment: &str) -> Option<usize> {
        if let Some(exact) = &mut self.exact_segments {
            let candidates = exact.get_mut(fragment)?;
            while let Some(index) = candidates.pop() {
                if self.ranges[index].is_empty() {
                    return Some(index);
                }
            }
            None
        } else {
            self.segments
                .iter()
                .enumerate()
                .position(|(index, text)| text == fragment && self.ranges[index].is_empty())
        }
    }

    fn find_fragment(&mut self, fragment: &'a str) -> Option<(usize, usize, usize)> {
        if fragment.is_empty() {
            return None;
        }
        let (index, offset) = self.next_matches.entry(fragment).or_default();
        while let Some(text) = self.segments.get(*index) {
            while let Some(relative) = text[*offset..].find(fragment) {
                let start = *offset + relative;
                let end = start + fragment.len();
                // Advance by the entire match, including rejected matches,
                // to preserve str::match_indices for self-overlapping text.
                *offset = end;
                let overlaps = self.ranges[*index]
                    .range(..end)
                    .next_back()
                    .is_some_and(|(_, range)| range.end > start);
                if !overlaps {
                    return Some((*index, start, end));
                }
            }
            *index += 1;
            *offset = 0;
        }
        None
    }

    fn into_ranges(self) -> Vec<BTreeMap<usize, FragmentRange>> {
        self.ranges
    }
}

#[cfg(test)]
const BILLING_HEADER_PREFIX: &str = "x-anthropic-billing-header:";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixUnitKind {
    Tool,
    /// History message inside the protected system prefix.
    System,
    History,
    Current,
    /// Unmapped or synthetic content changes the following prefix identity,
    /// but contributes no cacheable weight or client lookback position.
    Injected,
}

/// One cache position with its content identity and optional breakpoint.
#[derive(Debug, Clone, PartialEq)]
pub struct PrefixUnit {
    pub kind: PrefixUnitKind,
    pub cache_point: Option<LocalCachePoint>,
    /// Semantic content without wire cache markers, per-request IDs or the
    /// volatile Claude Code billing header line.
    pub identity: Value,
    /// Assistant tool-call and user tool-result turns may share a lookback slot.
    pub tool_turn: bool,
}

/// Units of the payload in cache-prefix order. Pure and deterministic.
pub fn prefix_units(payload: &KiroPayload) -> Vec<PrefixUnit> {
    let mut units = Vec::new();
    let current = &payload
        .conversation_state
        .current_message
        .user_input_message;
    for tool in current
        .user_input_message_context
        .iter()
        .flat_map(|context| context.tools.iter())
    {
        match tool {
            KiroTool::Specification { tool_specification } => units.push(PrefixUnit {
                kind: PrefixUnitKind::Tool,
                cache_point: None,
                identity: serde_json::json!({"specification":tool_specification, "documentation":tool_specification.local_documentation}),
                tool_turn: false,
            }),
            KiroTool::CachePoint { cache_point } => {
                if let Some(last) = units.last_mut() {
                    last.cache_point = Some(cache_point.local);
                }
            }
        }
    }
    let protected = payload.protected_history_len();
    for (index, message) in payload.conversation_state.history.iter().enumerate() {
        let kind = if index < protected {
            PrefixUnitKind::System
        } else {
            PrefixUnitKind::History
        };
        if let Some(user) = &message.user_input_message {
            append_sources(
                &mut units,
                crate::LocalSources::for_user(user),
                kind,
                || serde_json::json!({"injected_user":crate::LocalSources::user_digest(user)}),
            );
        }
        if let Some(assistant) = &message.assistant_response_message {
            append_sources(
                &mut units,
                crate::LocalSources::for_assistant(assistant),
                kind,
                || serde_json::json!({"injected_assistant":crate::LocalSources::assistant_digest(assistant)}),
            );
        }
    }
    append_sources(
        &mut units,
        crate::LocalSources::for_user(current),
        PrefixUnitKind::Current,
        || serde_json::json!({"injected_user":crate::LocalSources::user_digest(current)}),
    );
    units
}

fn append_sources(
    units: &mut Vec<PrefixUnit>,
    sources: Option<&crate::LocalSources>,
    kind: PrefixUnitKind,
    injected: impl FnOnce() -> Value,
) {
    if sources.is_none_or(|sources| sources.blocks.is_empty()) {
        units.push(PrefixUnit {
            kind: PrefixUnitKind::Injected,
            cache_point: None,
            identity: injected(),
            tool_turn: false,
        });
    }
    for block in sources.into_iter().flat_map(|sources| &sources.blocks) {
        units.push(PrefixUnit {
            kind,
            cache_point: block.cache_point,
            identity: block.identity.clone(),
            tool_turn: block.tool_turn,
        });
    }
}

/// Only the first physical line of the system prompt, and only when it is the
/// exact Claude Code metadata header. Following text remains significant.
#[cfg(test)]
fn strip_billing_header(content: &str) -> &str {
    if !content.starts_with(BILLING_HEADER_PREFIX) {
        return content;
    }
    content.split_once('\n').map_or("", |(_, rest)| rest)
}

impl TokenCountCache {
    /// Client-source counts and uncached overhead. Every actual collector
    /// segment is counted once; a merged segment is partitioned by fragment
    /// weights using floor(T * E_i / E). Remainders stay uncached.
    pub async fn prefix_counts(&self, payload: &KiroPayload) -> Result<(Vec<u64>, u64), String> {
        let prepared = crate::prepare_redacted_kiro_payload(payload, &[]);
        let session = prepared.redaction_session.as_ref();
        let current = &prepared
            .conversation_state
            .current_message
            .user_input_message;
        let mut groups = Vec::new();
        let mut moved_documentation = Vec::new();
        let mut unit_count = 0;
        let mut overhead = CONVERSATION_OVERHEAD_TOKENS as u64;
        for tool in current
            .user_input_message_context
            .iter()
            .flat_map(|context| &context.tools)
        {
            if let Some(spec) = tool.specification() {
                groups.push(CountGroup {
                    segments: vec![
                        spec.name.clone(),
                        spec.description.clone(),
                        spec.input_schema.json.to_string(),
                    ],
                    sources: vec![(
                        unit_count,
                        vec![
                            spec.name.clone(),
                            if spec.local_description_is_injected {
                                String::new()
                            } else {
                                spec.description.clone()
                            },
                            spec.input_schema.json.to_string(),
                        ],
                        0,
                    )],
                    fixed: 0,
                });
                if let Some(documentation) = &spec.local_documentation {
                    let text = session.map_or_else(
                        || documentation.clone(),
                        |session| session.accounting_text(documentation),
                    );
                    moved_documentation.push((unit_count, vec![text], 0));
                }
                overhead += TOOL_SCHEMA_OVERHEAD_TOKENS as u64;
                unit_count += 1;
            } else {
                overhead += MESSAGE_OVERHEAD_TOKENS as u64;
            }
        }
        let mut add_message = |user: Option<&KiroUserInputMessage>,
                               assistant: Option<&crate::KiroAssistantMessage>,
                               sources: Option<&crate::LocalSources>,
                               current_message: bool| {
            let mut group = CountGroup {
                segments: Vec::new(),
                sources: Vec::new(),
                fixed: 0,
            };
            if let Some(user) = user {
                let mut user = user.clone();
                if current_message {
                    if let Some(context) = user.user_input_message_context.as_mut() {
                        context.tools.clear();
                    }
                }
                collect_user_message(&user, &mut group.segments, &mut group.fixed);
            } else if let Some(assistant) = assistant {
                collect_assistant_message(assistant, &mut group.segments, &mut group.fixed);
            } else {
                return;
            }
            if user.is_some_and(|user| {
                moved_documentation.iter().any(|(_, fragments, _)| {
                    fragments
                        .iter()
                        .any(|text| user.content.contains(text.trim()))
                })
            }) {
                group
                    .sources
                    .extend(std::mem::take(&mut moved_documentation));
            }
            if sources.is_none_or(|sources| sources.blocks.is_empty()) {
                group.sources.push((unit_count, Vec::new(), 0));
                unit_count += 1;
            }
            for block in sources.iter().flat_map(|source| &source.blocks) {
                let fragments = block
                    .fragments
                    .iter()
                    .map(|text| {
                        session
                            .map_or_else(|| text.clone(), |session| session.accounting_text(text))
                    })
                    .collect();
                group
                    .sources
                    .push((unit_count, fragments, block.media_tokens));
                unit_count += 1;
            }
            groups.push(group);
        };
        for (message, original) in prepared
            .conversation_state
            .history
            .iter()
            .zip(&payload.conversation_state.history)
        {
            if message.user_input_message.is_some() {
                add_message(
                    message.user_input_message.as_ref(),
                    None,
                    original
                        .user_input_message
                        .as_ref()
                        .and_then(crate::LocalSources::for_user),
                    false,
                );
            }
            if message.assistant_response_message.is_some() {
                add_message(
                    None,
                    message.assistant_response_message.as_ref(),
                    original
                        .assistant_response_message
                        .as_ref()
                        .and_then(crate::LocalSources::for_assistant),
                    false,
                );
            }
        }
        add_message(
            Some(current),
            None,
            crate::LocalSources::for_user(
                &payload
                    .conversation_state
                    .current_message
                    .user_input_message,
            ),
            true,
        );
        let mut counts = vec![0u64; unit_count];
        let mut texts = Vec::new();
        let mut shares = Vec::new();
        for group in groups {
            let mut mapper = FragmentMapper::new(&group.segments);
            let mut media_left = group.fixed as u64;
            for (unit, fragments, media) in &group.sources {
                let media = (*media).min(media_left);
                counts[*unit] += media;
                media_left -= media;
                for fragment in fragments {
                    mapper.assign(fragment, *unit);
                }
            }
            overhead += media_left;
            let ranges = mapper.into_ranges();
            for (segment, ranges) in group.segments.into_iter().zip(ranges) {
                let total_index = texts.len();
                texts.push(segment.clone());
                if ranges.is_empty() {
                    shares.push((total_index, Vec::new()));
                    continue;
                }
                if ranges.len() == 1 {
                    let (start, range) = ranges.first_key_value().unwrap();
                    if *start == 0 && range.end == segment.len() {
                        shares.push((total_index, vec![(Some(range.unit), total_index)]));
                        continue;
                    }
                }
                let mut parts = Vec::new();
                let mut cursor = 0;
                for (start, FragmentRange { unit, end }) in ranges {
                    if start > cursor {
                        parts.push((None, texts.len()));
                        texts.push(segment[cursor..start].to_owned());
                    }
                    parts.push((Some(unit), texts.len()));
                    texts.push(segment[start..end].to_owned());
                    cursor = end;
                }
                if cursor < segment.len() {
                    parts.push((None, texts.len()));
                    texts.push(segment[cursor..].to_owned());
                }
                shares.push((total_index, parts));
            }
        }
        let tokens = self.count_many(texts).await?;
        for (total_index, parts) in shares {
            let total = tokens[total_index] as u64;
            let weights = parts
                .iter()
                .map(|(_, index)| tokens[*index] as u64)
                .sum::<u64>();
            let mut allocated = 0;
            if weights > 0 {
                let mut cumulative_weight = 0u64;
                let mut boundary = 0u64;
                for (unit, index) in parts {
                    cumulative_weight += tokens[index] as u64;
                    let next = (u128::from(total) * u128::from(cumulative_weight)
                        / u128::from(weights)) as u64;
                    if let Some(unit) = unit {
                        counts[unit] += next - boundary;
                        allocated += next - boundary;
                    }
                    boundary = next;
                }
            }
            overhead += total - allocated;
        }
        Ok((counts, overhead))
    }

    /// Count many independent segments with one blocking task. Each value is
    /// identical to `count` for that segment, including LRU reuse.
    pub async fn count_many(&self, texts: Vec<String>) -> Result<Vec<usize>, String> {
        let mut results = vec![None; texts.len()];
        let mut pending = Vec::new();
        for (index, text) in texts.into_iter().enumerate() {
            match self.cached_count(&text) {
                Some(count) => results[index] = Some(count),
                None => pending.push((index, text)),
            }
        }
        if !pending.is_empty() {
            let tokenizer = std::sync::Arc::clone(&self.tokenizer);
            let computed = tokio::task::spawn_blocking(move || {
                let mut memo = std::collections::HashMap::<String, usize>::new();
                pending
                    .into_iter()
                    .map(|(index, text)| {
                        let count = *memo
                            .entry(text.clone())
                            .or_insert_with(|| super::estimated_text_tokens(&tokenizer, &text));
                        (index, text, count)
                    })
                    .collect::<Vec<_>>()
            })
            .await
            .map_err(|error| error.to_string())?;
            for (index, text, count) in computed {
                self.store_count(&text, count);
                results[index] = Some(count);
            }
        }
        Ok(results.into_iter().map(Option::unwrap_or_default).collect())
    }
}

#[cfg(test)]
mod fragment_tests {
    use super::*;

    type Assignments = Vec<Vec<(usize, usize, usize)>>;

    fn assignments(segments: &[String], fragments: &[&str]) -> Assignments {
        let mut mapper = FragmentMapper::new(segments);
        for (unit, fragment) in fragments.iter().copied().enumerate() {
            mapper.assign(fragment, unit);
        }
        mapper
            .into_ranges()
            .into_iter()
            .map(|ranges| {
                ranges
                    .into_iter()
                    .map(|(start, range)| (range.unit, start, range.end))
                    .collect()
            })
            .collect()
    }

    // Preserve the original exhaustive search as an independent semantic
    // oracle: every fragment starts at the first segment and occurrence.
    fn reference_assignments(segments: &[String], fragments: &[&str]) -> Assignments {
        let mut ranges: Assignments = vec![Vec::new(); segments.len()];
        for (unit, fragment) in fragments.iter().copied().enumerate() {
            if fragment.is_empty() {
                continue;
            }
            let exact = segments
                .iter()
                .enumerate()
                .position(|(index, text)| text == fragment && ranges[index].is_empty());
            let found = exact.map(|index| (index, 0, fragment.len())).or_else(|| {
                let fragment = fragment.trim();
                if fragment.is_empty() {
                    return None;
                }
                segments.iter().enumerate().find_map(|(index, text)| {
                    text.match_indices(fragment)
                        .find(|(start, _)| {
                            !ranges[index]
                                .iter()
                                .any(|(_, a, b)| *start < *b && *start + fragment.len() > *a)
                        })
                        .map(|(start, _)| (index, start, start + fragment.len()))
                })
            });
            if let Some((index, start, end)) = found {
                ranges[index].push((unit, start, end));
            }
        }
        for ranges in &mut ranges {
            ranges.sort_unstable_by_key(|(_, start, _)| *start);
        }
        ranges
    }

    fn assert_matches_reference(segments: &[&str], fragments: &[&str]) -> Assignments {
        let segments: Vec<_> = segments.iter().map(|text| (*text).to_owned()).collect();
        let actual = assignments(&segments, fragments);
        assert_eq!(
            actual,
            reference_assignments(&segments, fragments),
            "segments={segments:?}, fragments={fragments:?}"
        );
        actual
    }

    #[test]
    fn whole_fields_win_before_trimmed_substrings() {
        let ranges = assert_matches_reference(
            &["prefix body suffix", "body", "body", " body "],
            &["body", " body ", "body", "body", "body"],
        );
        assert_eq!(ranges[0], vec![(3, 7, 11)]);
        assert_eq!(ranges[1], vec![(0, 0, 4)]);
        assert_eq!(ranges[2], vec![(2, 0, 4)]);
        assert_eq!(ranges[3], vec![(1, 0, 6)]);

        // Exercise the indexed exact path, including segments already
        // partially occupied by a different fragment.
        assert_matches_reference(
            &["body", "body", "body", "x", "x", "x", "x", "x", "x"],
            &["bo", "body", "x", "body", "body", "x", "dy", "x", "x"],
        );
    }

    #[test]
    fn self_overlapping_patterns_keep_match_indices_boundaries() {
        let ranges = assert_matches_reference(&["aaaaa"], &["a", "aa", "aa", "a", "a"]);
        assert_eq!(ranges[0], vec![(0, 0, 1), (3, 1, 2), (1, 2, 4), (4, 4, 5)]);
    }

    #[test]
    fn out_of_order_unicode_fragments_preserve_gaps_and_overlap_rejection() {
        let ranges = assert_matches_reference(
            &["alpha 雪☃ beta 雪☃ gamma"],
            &["gamma", "雪☃", "alpha", " 雪☃ ", "beta", "雪", "", " \t"],
        );
        let assigned: Vec<_> = ranges[0].iter().map(|(unit, _, _)| *unit).collect();
        assert_eq!(assigned, vec![2, 1, 4, 3, 0]);
        assert!(ranges[0].windows(2).all(|pair| pair[0].2 < pair[1].1));
        assert_matches_reference(
            &["", " \t", "first\n[redacted-雪]\nlast", "[redacted-雪]"],
            &[
                " \t",
                "",
                "[redacted-雪]",
                "last",
                " [redacted-雪] ",
                "first",
            ],
        );
    }

    #[test]
    fn varied_fragment_sequences_match_the_exhaustive_mapper() {
        let segments = [
            "",
            "a",
            "aa",
            "ababa",
            "a a a",
            "aba aba",
            "雪☃雪☃",
            " \t雪 \n",
            "x body x",
            "body",
            " body ",
            "name",
            "{\"type\":\"string\"}",
        ];
        let fragments = [
            "",
            "a",
            "b",
            "ab",
            "ba",
            "aa",
            "aba",
            "a a a",
            "雪",
            "☃",
            "雪☃",
            " \t",
            "a ",
            " a",
            " 雪 ",
            "body",
            " body ",
            "name",
            "{\"type\":\"string\"}",
        ];
        for seed in 0..128u32 {
            let mut state = seed + 1;
            let mut pick = |len| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                state as usize % len
            };
            let selected_segments: Vec<_> = (0..1 + pick(16))
                .map(|_| segments[pick(segments.len())])
                .collect();
            let selected_fragments: Vec<_> =
                (0..48).map(|_| fragments[pick(fragments.len())]).collect();
            assert_matches_reference(&selected_segments, &selected_fragments);
        }
    }

    #[test]
    fn repeated_fragments_receive_each_disjoint_occurrence_once() {
        let segments = vec!["same\n".repeat(2048)];
        let fragments = vec![" same "; 2049];
        let ranges = assignments(&segments, &fragments);
        assert_eq!(ranges[0].len(), 2048);
        for (index, &(unit, start, end)) in ranges[0].iter().enumerate() {
            assert_eq!((unit, start, end), (index, index * 5, index * 5 + 4));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{claude_to_kiro, ClaudeRequest, TranslationOptions};
    use serde_json::json;

    fn request(system: Value, messages: Value) -> ClaudeRequest {
        serde_json::from_value(json!({
            "model":"source","max_tokens":64,"system":system,
            "tools":[{"name":"Read","description":"Read a file.","input_schema":{"type":"object","properties":{"path":{"type":"string"}}},
                "cache_control":{"type":"ephemeral","ttl":"1h"}}],
            "messages":messages,
        }))
        .unwrap()
    }

    fn translate(request: &ClaudeRequest) -> KiroPayload {
        let mut options = TranslationOptions::new("target", "AI_EDITOR");
        options.enable_prompt_cache = true;
        claude_to_kiro(request, &options)
    }

    #[tokio::test]
    async fn counts_sum_to_the_context_estimate_and_align_with_units() {
        let request = request(
            json!([{"type":"text","text":"Rules. ".repeat(400),"cache_control":{"type":"ephemeral"}}]),
            json!([
                {"role":"user","content":"first question"},
                {"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Read","input":{"path":"a"}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"file body"},
                    {"type":"text","text":"next","cache_control":{"type":"ephemeral"}}]}
            ]),
        );
        let payload = translate(&request);
        let tokenizer = TokenCountCache::new(32).unwrap();
        let (counts, trailing) = tokenizer.prefix_counts(&payload).await.unwrap();
        let units = prefix_units(&payload);
        assert_eq!(counts.len(), units.len());
        assert_eq!(
            counts.iter().sum::<u64>() + trailing,
            tokenizer.estimate_kiro_payload(&payload).await.unwrap() as u64
        );
        assert_eq!(units[0].kind, PrefixUnitKind::Tool);
        assert_eq!(units[0].cache_point.map(|point| point.one_hour), Some(true));
        assert!(units
            .iter()
            .any(|unit| unit.kind == PrefixUnitKind::System && unit.cache_point.is_some()));
        assert!(units.last().unwrap().cache_point.is_some());
        assert!(units
            .iter()
            .any(|unit| unit.kind == PrefixUnitKind::Current && unit.tool_turn));
    }

    #[tokio::test]
    async fn billing_header_line_does_not_change_identity() {
        let messages = json!([{"role":"user","content":"hello"}]);
        let first = translate(&request(
            json!([{"type":"text","text":"x-anthropic-billing-header: cc_version=1.0.1; cc_entrypoint=cli;"},
                {"type":"text","text":"Stable rules."}]),
            messages.clone(),
        ));
        let second = translate(&request(
            json!([{"type":"text","text":"x-anthropic-billing-header: cc_version=2.0.0; cc_entrypoint=sdk;"},
                {"type":"text","text":"Stable rules."}]),
            messages.clone(),
        ));
        let changed = translate(&request(
            json!([{"type":"text","text":"x-anthropic-billing-header: cc_version=2.0.0;"},
                {"type":"text","text":"Different rules."}]),
            messages,
        ));
        let system = |payload: &KiroPayload| {
            prefix_units(payload)
                .into_iter()
                .find(|unit| unit.kind == PrefixUnitKind::System)
                .unwrap()
                .identity
        };
        assert_eq!(system(&first), system(&second));
        assert_ne!(system(&first), system(&changed));
    }

    #[test]
    fn billing_header_is_only_stripped_at_the_exact_start() {
        assert_eq!(
            strip_billing_header("x-anthropic-billing-header: a\nbody"),
            "body"
        );
        assert_eq!(
            strip_billing_header(" x-anthropic-billing-header: a\nbody"),
            " x-anthropic-billing-header: a\nbody"
        );
        assert_eq!(
            strip_billing_header("body\nx-anthropic-billing-header: a"),
            "body\nx-anthropic-billing-header: a"
        );
    }

    #[tokio::test]
    async fn count_many_matches_individual_counts() {
        let tokenizer = TokenCountCache::new(8).unwrap();
        let texts = vec![
            "short".to_owned(),
            "long text ".repeat(200),
            "short".to_owned(),
            "雪☃ mixed".to_owned(),
            String::new(),
        ];
        let batch = tokenizer.count_many(texts.clone()).await.unwrap();
        for (text, count) in texts.into_iter().zip(batch) {
            assert_eq!(tokenizer.count(text).await.unwrap(), count);
        }
    }
}

#[cfg(test)]
mod source_tests {
    use super::*;
    use crate::{claude_to_kiro, ClaudeRequest, TranslationOptions};
    use serde_json::json;

    #[tokio::test]
    async fn moved_tool_documentation_and_media_count_once() {
        let request: ClaudeRequest = serde_json::from_value(json!({"model":"claude-opus-5.5","max_tokens":64,
            "tools":[{"name":"Read","description":"long tool documentation ".repeat(600),"input_schema":{"type":"object"},"cache_control":{"type":"ephemeral"}}],
            "messages":[{"role":"user","content":[{"type":"document","source":{"type":"text","media_type":"text/plain","data":"document body ".repeat(200)}},{"type":"text","text":"summarize"}]}]
        })).unwrap();
        let mut options = TranslationOptions::new("claude-opus-5.5", "CLI");
        options.enable_prompt_cache = true;
        let payload = claude_to_kiro(&request, &options);
        let tokenizer = TokenCountCache::new(64).unwrap();
        let (counts, uncached) = tokenizer.prefix_counts(&payload).await.unwrap();
        assert!(
            counts[0] > 1500,
            "moved documentation belongs to the tool boundary: {counts:?}"
        );
        assert_eq!(
            counts.iter().sum::<u64>() + uncached,
            tokenizer.estimate_kiro_payload(&payload).await.unwrap() as u64
        );
        let breakdown = tokenizer.context_token_breakdown(&payload).await.unwrap();
        assert_eq!(
            breakdown.total_input_tokens,
            counts.iter().sum::<u64>() + uncached
        );
        assert_eq!(
            breakdown.total_input_tokens,
            breakdown.protected_prefix_tokens
                + breakdown.tool_definition_tokens
                + breakdown.current_message_tokens
                + breakdown.history_tokens
                + breakdown.overhead_tokens
        );
        assert!(!serde_json::to_string(&payload)
            .unwrap()
            .contains("local_sources"));
        assert!(!serde_json::to_string(&payload)
            .unwrap()
            .contains("local_documentation"));
    }

    #[tokio::test]
    async fn rewritten_message_cannot_inherit_a_client_boundary() {
        let request: ClaudeRequest = serde_json::from_value(json!({"model":"model","max_tokens":64,"messages":[{"role":"user","content":[{"type":"text","text":"source ".repeat(800),"cache_control":{"type":"ephemeral"}}]}]})).unwrap();
        let mut options = TranslationOptions::new("model", "CLI");
        options.enable_prompt_cache = true;
        let mut payload = claude_to_kiro(&request, &options);
        assert!(prefix_units(&payload)
            .iter()
            .any(|unit| unit.cache_point.is_some()));
        payload
            .conversation_state
            .current_message
            .user_input_message
            .content = "proxy replacement".into();
        assert!(!prefix_units(&payload)
            .iter()
            .any(|unit| unit.cache_point.is_some()));
        let tokenizer = TokenCountCache::new(16).unwrap();
        let (counts, uncached) = tokenizer.prefix_counts(&payload).await.unwrap();
        assert_eq!(
            counts.iter().sum::<u64>() + uncached,
            tokenizer.estimate_kiro_payload(&payload).await.unwrap() as u64
        );
    }

    #[test]
    fn media_rewrite_invalidates_sources_and_changes_injected_identity() {
        let request: ClaudeRequest = serde_json::from_value(json!({"model":"model","max_tokens":64,
            "messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"},"cache_control":{"type":"ephemeral"}}]}]
        })).unwrap();
        let mut options = TranslationOptions::new("model", "CLI");
        options.enable_prompt_cache = true;
        let mut payload = claude_to_kiro(&request, &options);
        assert!(prefix_units(&payload)
            .iter()
            .any(|unit| unit.cache_point.is_some()));
        payload
            .conversation_state
            .current_message
            .user_input_message
            .images[0]
            .source
            .bytes = "BBBB".into();
        let changed = prefix_units(&payload);
        assert!(!changed.iter().any(|unit| unit.cache_point.is_some()));
        payload
            .conversation_state
            .current_message
            .user_input_message
            .images[0]
            .source
            .bytes = "CCCC".into();
        assert_ne!(
            changed.last().unwrap().identity,
            prefix_units(&payload).last().unwrap().identity
        );
    }

    #[tokio::test]
    async fn redacted_sources_remain_cacheable_with_exact_total() {
        let request: ClaudeRequest = serde_json::from_value(json!({"model":"model","max_tokens":64,
            "system":[{"type":"text","text":format!("{}\npassword=opaqueCacheFixture123456", "stable rules ".repeat(800)),"cache_control":{"type":"ephemeral"}}],
            "messages":[{"role":"user","content":"continue"}]
        })).unwrap();
        let mut options = TranslationOptions::new("model", "CLI");
        options.enable_prompt_cache = true;
        let payload = claude_to_kiro(&request, &options);
        let tokenizer = TokenCountCache::new(64).unwrap();
        let (counts, uncached) = tokenizer.prefix_counts(&payload).await.unwrap();
        let boundary = prefix_units(&payload)
            .iter()
            .position(|unit| unit.cache_point.is_some())
            .unwrap();
        assert!(counts[boundary] > 1500);
        assert_eq!(
            counts.iter().sum::<u64>() + uncached,
            tokenizer.estimate_kiro_payload(&payload).await.unwrap() as u64
        );
    }
}
