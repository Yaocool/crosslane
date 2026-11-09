//! Local prompt-cache usage accounting for Kiro, which normally reports credits
//! rather than token/cache counters. These are proxy estimates, not upstream
//! billing evidence. Only completed generations publish reusable prefixes.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use crosslane_translate::{
    estimated_binary_document_tokens, estimated_image_tokens, ClaudeRequest, OpenAiRequest,
    TokenCountCache,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::super::response::DecodedResponse;

const DEFAULT_TTL: Duration = Duration::from_secs(5 * 60);
const ONE_HOUR_TTL: Duration = Duration::from_secs(60 * 60);
const LOOKBACK_BLOCKS: usize = 20;
const MAX_ENTRIES_PER_ACCOUNT: usize = 200;
const MAX_ACCOUNTS: usize = 512;

#[derive(Debug, Clone)]
struct CachePoint {
    fingerprint: String,
    tokens: u64,
    ttl: Duration,
}

#[derive(Debug, Clone)]
pub struct LegacyProfile {
    model: String,
    points: Vec<CachePoint>,
    candidates: HashSet<String>,
}

#[derive(Debug, Clone, Default)]
pub struct LegacyPlan {
    profile: Option<LegacyProfile>,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
}

impl LegacyPlan {
    pub fn usage(&self, input_tokens: u64) -> (u64, u64) {
        if self
            .profile
            .as_ref()
            .is_none_or(|profile| input_tokens < minimum_tokens(&profile.model))
        {
            return (0, 0);
        }
        let read = self.cache_read_tokens.min(input_tokens);
        let write = self
            .cache_write_tokens
            .min(input_tokens.saturating_sub(read));
        (read, write)
    }
}

#[derive(Debug, Clone)]
struct Entry {
    expires_at: Instant,
    ttl: Duration,
    tokens: u64,
}

#[derive(Debug, Default)]
pub struct LegacyTracker {
    entries: Mutex<HashMap<String, HashMap<String, Entry>>>,
}

impl LegacyTracker {
    pub async fn claude_profile(
        &self,
        tokenizer: &TokenCountCache,
        request: &ClaudeRequest,
        total_tokens: u64,
        model: &str,
    ) -> Option<LegacyProfile> {
        let mut blocks = claude_prefix(request);
        for message in request
            .messages
            .iter()
            .filter(|message| message.role != "system")
        {
            append_content(
                &mut blocks,
                &message.role,
                &message.content,
                cache_ttl(message.cache_control.as_ref()),
            );
        }
        if let Some(ttl) = cache_ttl(request.cache_control.as_ref()) {
            if let Some(last) = blocks.iter_mut().rev().find(|block| block.cacheable) {
                last.ttl = last.ttl.or(Some(ttl));
            }
        }
        build_profile(tokenizer, blocks, total_tokens, model, None).await
    }

    /// History removed by compaction cannot seed a cache. Keep only the stable
    /// tools/system prefix that still precedes the compacted checkpoint.
    pub async fn claude_compacted_profile(
        &self,
        tokenizer: &TokenCountCache,
        request: &ClaudeRequest,
        total_tokens: u64,
        model: &str,
    ) -> Option<LegacyProfile> {
        build_profile(tokenizer, claude_prefix(request), total_tokens, model, None).await
    }

    pub async fn openai_profile(
        &self,
        tokenizer: &TokenCountCache,
        request: &OpenAiRequest,
        total_tokens: u64,
        model: &str,
    ) -> Option<LegacyProfile> {
        let mut blocks = Vec::new();
        for tool in &request.tools {
            append_tool(&mut blocks, serde_json::to_value(tool).ok()?);
        }
        for message in request
            .messages
            .iter()
            .filter(|message| matches!(message.role.as_str(), "system" | "developer"))
        {
            if let Some(content) = &message.content {
                append_content(
                    &mut blocks,
                    &message.role,
                    content,
                    cache_ttl(message.cache_control.as_ref()),
                );
            }
        }
        for message in request
            .messages
            .iter()
            .filter(|message| !matches!(message.role.as_str(), "system" | "developer"))
        {
            let start = blocks.len();
            if let Some(content) = &message.content {
                append_content(
                    &mut blocks,
                    &message.role,
                    content,
                    cache_ttl(message.cache_control.as_ref()),
                );
            }
            // Structured calls/results are part of the prefix too: equal text
            // with different tool arguments must never count as a cache hit.
            if !message.tool_calls.is_empty() || message.tool_call_id.is_some() {
                blocks.push(CacheBlock::new(
                    &message.role,
                    json!({
                        "tool_calls":message.tool_calls,"tool_call_id":message.tool_call_id,
                    }),
                    None,
                    true,
                ));
            }
            if matches!(
                message.role.as_str(),
                "user" | "tool" | "developer" | "system"
            ) && blocks.len() > start
            {
                // OpenAI has implicit caching; cache_control is not required.
                if let Some(last) = blocks.last_mut() {
                    last.implicit = true;
                }
            }
        }
        if !blocks.iter().any(|block| block.ttl.is_some()) {
            if let Some(last) = blocks.iter_mut().rev().find(|block| block.implicit) {
                last.ttl = Some(DEFAULT_TTL);
            }
        }
        build_profile(
            tokenizer,
            blocks,
            total_tokens,
            model,
            request.prompt_cache_key.as_deref(),
        )
        .await
    }

    pub fn plan(
        &self,
        account_id: &str,
        model: &str,
        profile: Option<&LegacyProfile>,
    ) -> LegacyPlan {
        let Some(profile) = profile.filter(|profile| profile.model == model) else {
            return LegacyPlan::default();
        };
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        prune_expired(&mut entries, Instant::now());
        let total = profile
            .points
            .iter()
            .map(|point| point.tokens)
            .max()
            .unwrap_or(0);
        let read = entries
            .get(account_id)
            .into_iter()
            .flat_map(|account| account.iter())
            .filter(|(fingerprint, _)| profile.candidates.contains(*fingerprint))
            .map(|(_, entry)| entry.tokens)
            .max()
            .unwrap_or(0)
            .min(total);
        LegacyPlan {
            profile: Some(profile.clone()),
            cache_read_tokens: read,
            cache_write_tokens: total.saturating_sub(read),
        }
    }

    /// Called only after successful EOF. Explicit upstream counters, including
    /// zero, remain authoritative. Refusals without generated output do not
    /// create local cache entries even when an upstream charge was reported.
    pub fn commit(&self, account_id: &str, plan: &LegacyPlan, decoded: &mut DecodedResponse) {
        let Some(profile) = &plan.profile else {
            return;
        };
        if decoded.has_upstream_cache_usage() || !decoded.has_generated_output() {
            return;
        }
        if decoded.usage.input_tokens < minimum_tokens(&profile.model) {
            return;
        }
        let (read, write) = plan.usage(decoded.usage.input_tokens);
        decoded.usage.cache_read_tokens = read;
        decoded.usage.cache_write_tokens = write;
        decoded.cache_usage_estimated = true;
        if let Some(sources) = &mut decoded.cumulative_usage_sources {
            sources.cache |= crate::http::response::UsageSources::LOCAL;
        }
        if decoded.stop_sequence.is_some() {
            return;
        }
        let now = Instant::now();
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        prune_expired(&mut entries, now);
        if !entries.contains_key(account_id) && entries.len() >= MAX_ACCOUNTS {
            let oldest = entries
                .iter()
                .filter_map(|(id, account)| {
                    account
                        .values()
                        .map(|entry| entry.expires_at)
                        .min()
                        .map(|expiry| (id.clone(), expiry))
                })
                .min_by_key(|(_, expiry)| *expiry)
                .map(|(id, _)| id);
            if let Some(oldest) = oldest {
                entries.remove(&oldest);
            }
        }
        let account = entries.entry(account_id.to_owned()).or_default();
        for fingerprint in &profile.candidates {
            if let Some(entry) = account.get_mut(fingerprint) {
                entry.expires_at = now + entry.ttl;
            }
        }
        for point in &profile.points {
            account.insert(
                point.fingerprint.clone(),
                Entry {
                    expires_at: now + point.ttl,
                    ttl: point.ttl,
                    tokens: point.tokens.min(decoded.usage.input_tokens),
                },
            );
        }
        while account.len() > MAX_ENTRIES_PER_ACCOUNT {
            let Some(oldest) = account
                .iter()
                .min_by_key(|(_, entry)| entry.expires_at)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            account.remove(&oldest);
        }
    }
}

fn prune_expired(entries: &mut HashMap<String, HashMap<String, Entry>>, now: Instant) {
    entries.retain(|_, account| {
        account.retain(|_, entry| entry.expires_at > now);
        !account.is_empty()
    });
}

struct CacheBlock {
    value: Value,
    ttl: Option<Duration>,
    cacheable: bool,
    implicit: bool,
}
impl CacheBlock {
    fn new(role: &str, mut value: Value, ttl: Option<Duration>, cacheable: bool) -> Self {
        strip_content_control(&mut value);
        Self {
            value: json!({"role":role,"content":value}),
            ttl,
            cacheable,
            implicit: false,
        }
    }
}

fn claude_prefix(request: &ClaudeRequest) -> Vec<CacheBlock> {
    let mut blocks = Vec::new();
    for tool in request.tools.iter().filter(|tool| !tool.defer_loading) {
        if let Ok(value) = serde_json::to_value(tool) {
            append_tool(&mut blocks, value);
        }
    }
    if let Some(system) = &request.system {
        append_content(&mut blocks, "system", system, None);
    }
    for message in request
        .messages
        .iter()
        .filter(|message| message.role == "system")
    {
        append_content(
            &mut blocks,
            "system",
            &message.content,
            cache_ttl(message.cache_control.as_ref()),
        );
    }
    blocks
}

fn append_tool(blocks: &mut Vec<CacheBlock>, mut tool: Value) {
    let ttl = cache_ttl(tool.get("cache_control"))
        .or_else(|| cache_ttl(tool.pointer("/function/cache_control")));
    if let Some(function) = tool.get_mut("function").and_then(Value::as_object_mut) {
        function.remove("cache_control");
    }
    blocks.push(CacheBlock::new("tool_definition", tool, ttl, true));
}

fn append_content(
    blocks: &mut Vec<CacheBlock>,
    role: &str,
    content: &Value,
    outer_ttl: Option<Duration>,
) {
    let start = blocks.len();
    let values = content
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_else(|| std::slice::from_ref(content));
    for value in values {
        let cacheable = match value {
            Value::String(text) => !text.is_empty(),
            Value::Object(_) => {
                !matches!(
                    value.get("type").and_then(Value::as_str),
                    Some("thinking" | "redacted_thinking")
                ) && value.get("text").and_then(Value::as_str) != Some("")
            }
            _ => false,
        };
        let ttl = block_cache_ttl(value);
        let value = if value.is_string() {
            json!({"type":"text","text":value})
        } else {
            value.clone()
        };
        blocks.push(CacheBlock::new(
            role,
            value,
            ttl.filter(|_| cacheable),
            cacheable,
        ));
    }
    if let Some(ttl) = outer_ttl {
        if let Some(last) = blocks[start..]
            .iter_mut()
            .rev()
            .find(|block| block.cacheable)
        {
            last.ttl = Some(ttl);
        }
    }
}

fn block_cache_ttl(value: &Value) -> Option<Duration> {
    cache_ttl(value.get("cache_control")).or_else(|| {
        let nested = match value.get("type").and_then(Value::as_str) {
            Some("tool_result" | "search_result") => value.get("content"),
            Some("document")
                if value.pointer("/source/type").and_then(Value::as_str) == Some("content") =>
            {
                value.pointer("/source/content")
            }
            _ => None,
        };
        nested
            .and_then(Value::as_array)
            .and_then(|values| values.iter().rev().find_map(block_cache_ttl))
    })
}

// Match the input tokenizer's media basis instead of tokenizing base64 as text.
// Fingerprints still cover the original bytes, so changed attachments miss.
fn token_projection(value: &Value, fixed: &mut u64) -> Value {
    let mut projected = value.clone();
    match value.get("type").and_then(Value::as_str) {
        Some("image") => {
            if let Some(data) = value.pointer("/source/data").and_then(Value::as_str) {
                *fixed = fixed.saturating_add(estimated_image_tokens(data) as u64);
                projected["source"].as_object_mut().unwrap().remove("data");
            }
        }
        Some("image_url" | "input_image") => {
            let url = value
                .pointer("/image_url/url")
                .or_else(|| value.get("image_url"))
                .and_then(Value::as_str);
            if let Some(url) = url.filter(|url| url.starts_with("data:")) {
                *fixed = fixed.saturating_add(estimated_image_tokens(url) as u64);
                projected["image_url"] = json!({"type":"binary-image"});
            }
        }
        Some("document") => {
            if let Some(source) = value.get("source") {
                if source.get("type").and_then(Value::as_str) == Some("content") {
                    if let Some(blocks) = source.get("content").and_then(Value::as_array) {
                        projected["source"]["content"] = Value::Array(
                            blocks
                                .iter()
                                .map(|block| token_projection(block, fixed))
                                .collect(),
                        );
                    }
                } else if let Some(data) = source.get("data").and_then(Value::as_str) {
                    let text_type = source
                        .get("media_type")
                        .and_then(Value::as_str)
                        .is_some_and(|media| {
                            media.starts_with("text/")
                                || matches!(media, "application/json" | "application/xml")
                        });
                    let decoded_text = text_type
                        .then(|| {
                            base64::engine::general_purpose::STANDARD
                                .decode(data)
                                .ok()
                                .and_then(|bytes| String::from_utf8(bytes).ok())
                        })
                        .flatten();
                    if source.get("type").and_then(Value::as_str) == Some("text") {
                        *fixed = fixed.saturating_add(estimated_binary_document_tokens("") as u64);
                    } else if let Some(text) = decoded_text {
                        *fixed = fixed.saturating_add(estimated_binary_document_tokens("") as u64);
                        projected["source"] = json!({"type":"text","data":text});
                    } else {
                        *fixed =
                            fixed.saturating_add(estimated_binary_document_tokens(data) as u64);
                        projected["source"].as_object_mut().unwrap().remove("data");
                    }
                }
            }
        }
        Some("tool_result" | "search_result") => {
            if let Some(blocks) = value.get("content").and_then(Value::as_array) {
                projected["content"] = Value::Array(
                    blocks
                        .iter()
                        .map(|block| token_projection(block, fixed))
                        .collect(),
                );
            }
        }
        _ => {}
    }
    projected
}

// Only protocol content controls are omitted from fingerprints. A business
// property named cache_control inside a tool's input/schema remains significant.
fn strip_content_control(value: &mut Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    object.remove("cache_control");
    match object.get("type").and_then(Value::as_str) {
        Some("tool_result" | "search_result") => {
            if let Some(content) = object.get_mut("content").and_then(Value::as_array_mut) {
                for block in content {
                    strip_content_control(block);
                }
            }
        }
        Some("document") => {
            if let Some(content) = value
                .pointer_mut("/source/content")
                .and_then(Value::as_array_mut)
            {
                for block in content {
                    strip_content_control(block);
                }
            }
        }
        _ => {}
    }
}

async fn build_profile(
    tokenizer: &TokenCountCache,
    blocks: Vec<CacheBlock>,
    total_tokens: u64,
    model: &str,
    cache_key: Option<&str>,
) -> Option<LegacyProfile> {
    let writes: Vec<_> = blocks
        .iter()
        .enumerate()
        .filter_map(|(i, block)| block.ttl.map(|ttl| (i, ttl)))
        .collect();
    if writes.is_empty() {
        return None;
    }
    let lookbacks: HashSet<_> = writes
        .iter()
        .flat_map(|(i, _)| i.saturating_sub(LOOKBACK_BLOCKS - 1)..=*i)
        .collect();
    let mut hasher = Sha256::new();
    hasher.update(b"crosslane-local-prompt-cache-v2\0");
    hasher.update(serde_json::to_vec(&(model, cache_key)).ok()?);
    let mut text = String::new();
    let mut media_tokens = 0u64;
    let mut points = Vec::new();
    let mut candidates = HashSet::new();
    for (i, block) in blocks.into_iter().enumerate() {
        let encoded = stable_json(&block.value);
        hasher.update((encoded.len() as u64).to_le_bytes());
        hasher.update(encoded.as_bytes());
        let projected = json!({"role":block.value["role"],"content":token_projection(&block.value["content"], &mut media_tokens)});
        text.push_str(&stable_json(&projected));
        text.push('\n');
        let fingerprint = format!("{:x}", hasher.clone().finalize());
        if lookbacks.contains(&i) {
            candidates.insert(fingerprint.clone());
        }
        if let Some(ttl) = block.ttl {
            let tokens = (tokenizer.count(text.clone()).await.ok()? as u64)
                .saturating_add(media_tokens)
                .min(total_tokens);
            if tokens >= minimum_tokens(model) {
                points.push(CachePoint {
                    fingerprint,
                    tokens,
                    ttl,
                });
            }
        }
    }
    (!points.is_empty()).then_some(LegacyProfile {
        model: model.to_owned(),
        points,
        candidates,
    })
}

fn minimum_tokens(model: &str) -> u64 {
    let model = model.to_ascii_lowercase().replace('-', ".");
    if model.contains("opus.5") || model.contains("sonnet.5.5") || model.contains("haiku.5.5") {
        512
    } else if model.contains("opus.4.7") {
        2048
    } else if model.contains("opus.4.6")
        || model.contains("opus.4.5")
        || model.contains("haiku.4.5")
    {
        4096
    } else {
        1024
    }
}

fn cache_ttl(control: Option<&Value>) -> Option<Duration> {
    let control = control?;
    if control.get("type").and_then(Value::as_str) != Some("ephemeral") {
        return None;
    }
    match control.get("ttl") {
        Some(Value::String(value)) if value == "1h" => Some(ONE_HOUR_TTL),
        None | Some(Value::Null) => Some(DEFAULT_TTL),
        Some(Value::String(value)) if value == "5m" => Some(DEFAULT_TTL),
        _ => None,
    }
}

fn stable_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|key| format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_default(),
                        stable_json(&map[key])
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        Value::Array(values) => format!(
            "[{}]",
            values.iter().map(stable_json).collect::<Vec<_>>().join(",")
        ),
        _ => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosslane_kiro::KiroEvent;
    const MODEL: &str = "claude-opus-5.5";
    fn request() -> ClaudeRequest {
        serde_json::from_value(json!({"model":MODEL,"max_tokens":64,"system":[{"type":"text","text":"stable library reference ".repeat(1000),"cache_control":{"type":"ephemeral"}}],"messages":[{"role":"user","content":"hello"}]})).unwrap()
    }
    fn decoded() -> DecodedResponse {
        DecodedResponse {
            text: "answer".into(),
            usage: crosslane_kiro::UsageInfo {
                input_tokens: 8000,
                ..Default::default()
            },
            ..Default::default()
        }
    }
    async fn profile(tracker: &LegacyTracker, req: &ClaudeRequest) -> LegacyProfile {
        tracker
            .claude_profile(&TokenCountCache::new(64).unwrap(), req, 8000, MODEL)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn cold_write_then_warm_read_preserves_total_input() {
        let tracker = LegacyTracker::default();
        let p = profile(&tracker, &request()).await;
        let cold = tracker.plan("one", MODEL, Some(&p));
        let mut d = decoded();
        tracker.commit("one", &cold, &mut d);
        assert!(d.usage.cache_write_tokens > 512);
        assert_eq!(d.usage.cache_read_tokens, 0);
        assert_eq!(d.usage.input_tokens, 8000);
        assert_eq!(d.token_usage_source(), "estimated");
        let warm = tracker.plan("one", MODEL, Some(&p));
        let mut next = decoded();
        tracker.commit("one", &warm, &mut next);
        assert_eq!(next.usage.cache_read_tokens, d.usage.cache_write_tokens);
        assert_eq!(next.usage.cache_write_tokens, 0);
        assert_eq!(tracker.plan("two", MODEL, Some(&p)).cache_read_tokens, 0);
        assert_eq!(
            tracker
                .plan("one", "other-model", Some(&p))
                .cache_read_tokens,
            0
        );
    }

    #[tokio::test]
    async fn growing_breakpoints_reuse_prior_prefix_without_hashing_cache_markers() {
        let tracker = LegacyTracker::default();
        let mut req = request();
        req.system = None;
        req.messages[0].content = json!([{"type":"text","text":"stable reference ".repeat(1200),"cache_control":{"type":"ephemeral"}}]);
        let first = profile(&tracker, &req).await;
        let mut d = decoded();
        tracker.commit("one", &tracker.plan("one", MODEL, Some(&first)), &mut d);
        req.messages[0].content[0]
            .as_object_mut()
            .unwrap()
            .remove("cache_control");
        req.messages
            .push(serde_json::from_value(json!({"role":"assistant","content":"answer"})).unwrap());
        req.messages.push(serde_json::from_value(json!({"role":"user","content":[{"type":"text","text":"new context ".repeat(100),"cache_control":{"type":"ephemeral"}}]})).unwrap());
        let grown = profile(&tracker, &req).await;
        let mut next = decoded();
        tracker.commit("one", &tracker.plan("one", MODEL, Some(&grown)), &mut next);
        assert_eq!(next.usage.cache_read_tokens, d.usage.cache_write_tokens);
        assert!(next.usage.cache_write_tokens > 0);
        assert!(
            next.usage.cache_read_tokens + next.usage.cache_write_tokens <= next.usage.input_tokens
        );
    }

    #[tokio::test]
    async fn system_breakpoint_survives_growing_conversation_and_changed_tools_invalidate_it() {
        let tracker = LegacyTracker::default();
        let req = request();
        let p = profile(&tracker, &req).await;
        tracker.commit("one", &tracker.plan("one", MODEL, Some(&p)), &mut decoded());
        let mut next = req.clone();
        next.messages[0].content = json!("another question");
        let same = profile(&tracker, &next).await;
        assert!(tracker.plan("one", MODEL, Some(&same)).cache_read_tokens > 0);
        next.tools.push(
            serde_json::from_value(json!({"name":"Read","input_schema":{"type":"object"}}))
                .unwrap(),
        );
        let changed = profile(&tracker, &next).await;
        assert_eq!(
            tracker.plan("one", MODEL, Some(&changed)).cache_read_tokens,
            0
        );
    }

    #[tokio::test]
    async fn plans_are_read_only_and_refusals_do_not_seed_entries() {
        let tracker = LegacyTracker::default();
        let p = profile(&tracker, &request()).await;
        let first = tracker.plan("one", MODEL, Some(&p));
        assert_eq!(tracker.plan("one", MODEL, Some(&p)).cache_read_tokens, 0);
        let mut refusal = DecodedResponse::default();
        refusal.usage.input_tokens = 8000;
        refusal.usage.credits = 0.1;
        tracker.commit("one", &first, &mut refusal);
        assert_eq!(tracker.plan("one", MODEL, Some(&p)).cache_read_tokens, 0);
        tracker.commit("one", &first, &mut decoded());
        assert!(tracker.plan("one", MODEL, Some(&p)).cache_read_tokens > 0);
    }

    #[tokio::test]
    async fn explicit_upstream_zero_or_positive_cache_counts_are_preserved() {
        let tracker = LegacyTracker::default();
        let p = profile(&tracker, &request()).await;
        for count in [0, 1234] {
            let mut d = decoded();
            d.push(KiroEvent::Usage {
                usage: json!({"cacheReadInputTokens":count,"cacheWriteInputTokens":0}),
            })
            .unwrap();
            tracker.commit("one", &tracker.plan("one", MODEL, Some(&p)), &mut d);
            assert_eq!(d.usage.cache_read_tokens, count);
            assert_eq!(d.usage.cache_write_tokens, 0);
            assert!(!d.cache_usage_estimated);
        }
        assert_eq!(tracker.plan("one", MODEL, Some(&p)).cache_read_tokens, 0);
    }

    #[tokio::test]
    async fn ttl_expiry_and_small_prefixes_remain_cold() {
        let tracker = LegacyTracker::default();
        let p = profile(&tracker, &request()).await;
        tracker.commit("one", &tracker.plan("one", MODEL, Some(&p)), &mut decoded());
        for entry in tracker
            .entries
            .lock()
            .unwrap()
            .get_mut("one")
            .unwrap()
            .values_mut()
        {
            entry.expires_at = Instant::now() - Duration::from_secs(1);
        }
        assert_eq!(tracker.plan("one", MODEL, Some(&p)).cache_read_tokens, 0);
        let mut req = request();
        req.system.as_mut().unwrap()[0]["text"] = json!("small prefix");
        assert!(tracker
            .claude_profile(&TokenCountCache::new(32).unwrap(), &req, 8000, MODEL)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn automatic_claude_and_openai_caching_work_and_keys_are_isolated() {
        let tracker = LegacyTracker::default();
        let tokenizer = TokenCountCache::new(64).unwrap();
        let mut req = request();
        req.system.as_mut().unwrap()[0]
            .as_object_mut()
            .unwrap()
            .remove("cache_control");
        req.cache_control = Some(json!({"type":"ephemeral"}));
        assert!(tracker
            .claude_profile(&tokenizer, &req, 8000, MODEL)
            .await
            .is_some());
        let req: OpenAiRequest=serde_json::from_value(json!({"model":MODEL,"messages":[{"role":"system","content":"stable reference ".repeat(1000)},{"role":"user","content":"question"}],"prompt_cache_key":"one"})).unwrap();
        let first = tracker
            .openai_profile(&tokenizer, &req, 8000, MODEL)
            .await
            .unwrap();
        tracker.commit(
            "one",
            &tracker.plan("one", MODEL, Some(&first)),
            &mut decoded(),
        );
        assert!(tracker.plan("one", MODEL, Some(&first)).cache_read_tokens > 0);
        let mut different = req;
        different.prompt_cache_key = Some("two".into());
        let second = tracker
            .openai_profile(&tokenizer, &different, 8000, MODEL)
            .await
            .unwrap();
        assert_eq!(
            tracker.plan("one", MODEL, Some(&second)).cache_read_tokens,
            0
        );
    }

    #[tokio::test]
    async fn compacted_history_does_not_seed_removed_breakpoints() {
        let tracker = LegacyTracker::default();
        let tokenizer = TokenCountCache::new(64).unwrap();
        let mut req = request();
        req.system = None;
        req.messages[0].content = json!([{"type":"text","text":"old history ".repeat(2000),"cache_control":{"type":"ephemeral"}}]);
        assert!(tracker
            .claude_compacted_profile(&tokenizer, &req, 8000, MODEL)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn attachment_tokens_use_media_overhead_and_changed_bytes_invalidate_the_prefix() {
        let tracker = LegacyTracker::default();
        let tokenizer = TokenCountCache::new(64).unwrap();
        let mut bytes = vec![0u8; 48_000];
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let mut req = request();
        req.system = None;
        req.messages[0].content = json!([{"type":"image","source":{"type":"base64","media_type":"image/png","data":encoded},"cache_control":{"type":"ephemeral"}}]);
        let p = tracker
            .claude_profile(&tokenizer, &req, 8000, MODEL)
            .await
            .unwrap();
        let expected = estimated_image_tokens(&encoded) as u64;
        assert!(p.points[0].tokens >= expected);
        assert!(p.points[0].tokens < expected + 100);
        tracker.commit("one", &tracker.plan("one", MODEL, Some(&p)), &mut decoded());
        bytes[0] = 1;
        req.messages[0].content[0]["source"]["data"] =
            json!(base64::engine::general_purpose::STANDARD.encode(bytes));
        let changed = tracker
            .claude_profile(&tokenizer, &req, 8000, MODEL)
            .await
            .unwrap();
        assert_eq!(
            tracker.plan("one", MODEL, Some(&changed)).cache_read_tokens,
            0
        );
        assert_eq!(tracker.plan("one", MODEL, Some(&p)).usage(100), (0, 0));
    }

    #[tokio::test]
    async fn promoted_system_messages_and_nested_tool_result_markers_are_accounted_for() {
        let tracker = LegacyTracker::default();
        let mut req = request();
        req.system = None;
        req.messages[0].content = json!([{"type":"tool_result","tool_use_id":"fixture","content":[{"type":"text","text":"stable reference ".repeat(1200),"cache_control":{"type":"ephemeral"}}]}]);
        let p = profile(&tracker, &req).await;
        tracker.commit("one", &tracker.plan("one", MODEL, Some(&p)), &mut decoded());
        req.messages.push(
            serde_json::from_value(
                json!({"role":"system","content":"a newly promoted system message"}),
            )
            .unwrap(),
        );
        let changed = profile(&tracker, &req).await;
        assert_eq!(
            tracker.plan("one", MODEL, Some(&changed)).cache_read_tokens,
            0
        );
    }

    #[test]
    fn business_cache_control_properties_are_not_erased() {
        let mut block = json!({"type":"tool_use","input":{"cache_control":"first"},"cache_control":{"type":"ephemeral"}});
        strip_content_control(&mut block);
        assert_eq!(block["input"]["cache_control"], "first");
        assert!(block.get("cache_control").is_none());
        assert_eq!(minimum_tokens("claude-opus-4-6"), 4096);
        assert_eq!(minimum_tokens(MODEL), 512);
    }
}
