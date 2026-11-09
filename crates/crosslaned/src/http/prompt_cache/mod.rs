//! Local prompt-cache usage accounting for Kiro, which normally reports
//! credits rather than token or cache counters. Values are proxy estimates,
//! not upstream billing evidence.
//!
//! A profile is built from the translated Kiro payload: counts come from the
//! same segments as `estimate_kiro_payload` (so the full total equals the
//! context estimate and proxy-injected text is uncached), while fingerprints
//! use the unredacted semantic content so request-local redaction nonces never
//! break a prefix. Allocation is integer-proportional and conserves the round's
//! normalized input. Entries are published only after an accepted initial
//! generation reached transport EOF.

mod allocate;
pub(crate) mod legacy;
mod policy;
mod rounds;
mod store;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crosslane_translate::{prefix_units, KiroPayload, PrefixUnitKind, TokenCountCache};
use sha2::{Digest, Sha256};

pub(crate) use allocate::Allocation;
use allocate::{allocate, select_hit, Hit, Ttl, WritePoint};
pub(crate) use rounds::CacheLedger;
pub(crate) use store::Scope;
use store::{EntryStore, Update};

const NAMESPACE: &[u8] = b"crosslane-local-prompt-cache-v3\0";
const LOOKBACK_POSITIONS: usize = 20;
const MAX_EXPLICIT_BREAKPOINTS: usize = 4;
const SHORT_TTL: Duration = Duration::from_secs(5 * 60);
const LONG_TTL: Duration = Duration::from_secs(60 * 60);

impl Ttl {
    fn duration(self) -> Duration {
        match self {
            Ttl::FiveMinutes => SHORT_TTL,
            Ttl::OneHour => LONG_TTL,
        }
    }
}

/// Why local cache accounting did not apply to a round. Logged so that a zero
/// cache count can be explained without reading code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkipReason {
    /// Prompt cache disabled, or no usable write point in the request.
    NoProfile,
    /// The accepted attempt resolved to a different model than the profile.
    ModelMismatch,
    /// Every write point is below the model's minimum cacheable length.
    BelowThreshold,
    /// Input or local full count was zero.
    EmptyInput,
    /// A positive upstream cache group was used instead.
    UpstreamReported,
    /// The round produced no output (for example a bare refusal).
    NoOutput,
    /// The client view was cut by a stop sequence before upstream EOF.
    NotEof,
    /// The request failed, was cancelled or refused; nothing is published.
    NotPublished,
    /// Every entry would already be expired when published.
    ExpiredAtPublish,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoProfile => "no_profile",
            Self::ModelMismatch => "model_mismatch",
            Self::BelowThreshold => "below_threshold",
            Self::EmptyInput => "empty_input",
            Self::UpstreamReported => "upstream_reported",
            Self::NoOutput => "no_output",
            Self::NotEof => "not_eof",
            Self::NotPublished => "not_published",
            Self::ExpiredAtPublish => "expired_at_publish",
        }
    }
}

/// Accounting mode, decided by whether the client supplied explicit markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheMode {
    /// Anthropic semantics: explicit breakpoints, 20-position lookback.
    Explicit,
    /// OpenAI semantics: automatic longest-prefix matching, two anchors.
    Implicit,
}

/// Request-level inputs that change model behaviour without changing content,
/// plus the tenant that owns the entries.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProfileSalt {
    pub service_id: String,
    pub api_key_id: Option<String>,
    pub prompt_cache_key: Option<String>,
    pub tool_choice: Option<serde_json::Value>,
    pub parallel_tool_calls: Option<bool>,
}

#[derive(Debug, Clone)]
struct Position {
    fingerprint: [u8; 32],
    cum: u64,
    /// Lookback slot shared by consecutive tool-call/tool-result turns.
    slot: usize,
    write: Option<Ttl>,
    stable_prefix: bool,
}

/// One attempt's local cache profile. Holds only counts and digests.
#[derive(Debug, Clone)]
pub struct PromptCacheProfile {
    model: String,
    salt: ProfileSalt,
    readable_prefix_only: bool,
    #[cfg(test)]
    profile_domain: Option<String>,
    /// Frozen `prompt_cache_accounting = legacy`: the previous algorithm runs
    /// unchanged and none of the v3 fields below are used.
    legacy: Option<Box<legacy::LegacyProfile>>,
    service_id: String,
    api_key_id: Option<String>,
    mode: CacheMode,
    full: u64,
    positions: Vec<Position>,
    /// Lookback positions are limited to this prefix after compaction/truncation.
    readable_until: usize,
}

impl PromptCacheProfile {
    fn from_legacy(model: &str, profile: legacy::LegacyProfile) -> Self {
        Self {
            model: model.to_owned(),
            salt: ProfileSalt::default(),
            readable_prefix_only: false,
            #[cfg(test)]
            profile_domain: None,
            legacy: Some(Box::new(profile)),
            service_id: String::new(),
            api_key_id: None,
            mode: CacheMode::Explicit,
            full: 0,
            positions: Vec::new(),
            readable_until: 0,
        }
    }

    pub fn full_tokens(&self) -> u64 {
        self.full
    }

    fn writes(&self) -> Vec<WritePoint> {
        self.positions
            .iter()
            .take(self.readable_until)
            .filter_map(|position| {
                position.write.map(|ttl| WritePoint {
                    cum: position.cum,
                    ttl,
                })
            })
            .collect()
    }
}

#[derive(Debug, Default)]
pub struct PromptCacheTracker {
    store: EntryStore,
    legacy: legacy::LegacyTracker,
}

/// Snapshot-based plan for one accepted attempt. Pure after construction.
#[derive(Debug, Clone, Default)]
pub struct PromptCachePlan {
    profile: Option<PromptCacheProfile>,
    /// Set when a profile existed but did not match the accepted attempt.
    mismatch: bool,
    legacy: Option<legacy::LegacyPlan>,
    scope: Option<Scope>,
    /// Snapshot at T0: fingerprint → slots valid at that moment.
    snapshot: HashMap<[u8; 32], store::Slots>,
    snapshot_at: Option<Instant>,
    stream_slot_wait_ms: u64,
}

/// Frozen before generation begins, including any stream-slot wait. Only the
/// selected account/tenant's bounded store is copied; later publications are
/// invisible even if they precede the accepted endpoint's dispatch.
#[derive(Debug, Clone, Default)]
pub(crate) struct AttemptSnapshot {
    scope: Option<Scope>,
    at: Option<Instant>,
    entries: HashMap<[u8; 32], store::Slots>,
}

/// What a completed initial round may publish.
#[derive(Debug, Clone)]
pub(crate) struct PublicationIntent {
    scope: Scope,
    dispatched_at: Instant,
    updates: Vec<(usize, Ttl)>,
    profile: PromptCacheProfile,
    pub(crate) policy_listed: bool,
}

impl PromptCacheTracker {
    /// Build a profile from the translated payload that will be sent. `salt`
    /// covers request controls; tool/system/history/current content and their
    /// cache markers come from the payload. Returns `None` when there is no
    /// usable write point (no markers in explicit mode, or nothing to cache).
    pub async fn profile(
        &self,
        tokenizer: &TokenCountCache,
        payload: &KiroPayload,
        mode: CacheMode,
        salt: &ProfileSalt,
        readable_prefix_only: bool,
    ) -> Option<PromptCacheProfile> {
        if mode == CacheMode::Explicit && !has_explicit_marker(payload) {
            return None;
        }
        let units = prefix_units(payload);
        // Explicit requests without a usable marker cannot publish or read a
        // cache entry. Avoid preparing/redacting and tokenizing their full body.
        // Implicit OpenAI profiles still need their automatic anchors.
        if mode == CacheMode::Explicit && !units.iter().any(|unit| unit.cache_point.is_some()) {
            return None;
        }
        let (counts, trailing) = match tokenizer.prefix_counts(payload).await {
            Ok(counts) => counts,
            Err(error) => {
                tracing::warn!(%error, "prompt cache profile skipped: token counting failed");
                return None;
            }
        };
        if counts.len() != units.len() {
            tracing::warn!("prompt cache profile skipped: unit layout mismatch");
            return None;
        }
        let model = payload
            .conversation_state
            .current_message
            .user_input_message
            .model_id
            .clone();
        let mut hasher = Sha256::new();
        hasher.update(NAMESPACE);
        hasher.update(prelude(payload, &model, salt));
        let stable_end = units
            .iter()
            .rposition(|unit| matches!(unit.kind, PrefixUnitKind::Tool | PrefixUnitKind::System));
        let last_message = units.iter().rposition(|unit| {
            unit.kind != PrefixUnitKind::Tool
                && unit
                    .identity
                    .get("role")
                    .and_then(serde_json::Value::as_str)
                    == Some("user")
        });
        let mut positions = Vec::with_capacity(units.len());
        let mut cum = 0u64;
        let mut slot = 0usize;
        let mut previous_tool_turn = false;
        let mut previous_role = None;
        let mut explicit = 0usize;
        for (index, (unit, tokens)) in units.iter().zip(counts).enumerate() {
            let encoded = serde_json::to_vec(&unit.identity).unwrap_or_default();
            hasher.update((encoded.len() as u64).to_le_bytes());
            hasher.update(&encoded);
            cum = cum.saturating_add(tokens);
            let role = unit
                .identity
                .get("role")
                .and_then(serde_json::Value::as_str);
            if unit.kind != PrefixUnitKind::Injected
                && !(unit.tool_turn && previous_tool_turn && role == previous_role)
            {
                slot += 1;
            }
            previous_tool_turn = unit.tool_turn;
            previous_role = role;
            let write = match mode {
                CacheMode::Explicit => unit.cache_point.and_then(|point| {
                    (explicit < MAX_EXPLICIT_BREAKPOINTS).then(|| {
                        explicit += 1;
                        if point.one_hour {
                            Ttl::OneHour
                        } else {
                            Ttl::FiveMinutes
                        }
                    })
                }),
                // Two anchors: the stable prefix and the latest message boundary.
                CacheMode::Implicit => (Some(index) == stable_end || Some(index) == last_message)
                    .then_some(Ttl::FiveMinutes),
            };
            positions.push(Position {
                fingerprint: hasher.clone().finalize().into(),
                cum,
                slot,
                write,
                stable_prefix: Some(index) == stable_end,
            });
        }
        let full = cum.saturating_add(trailing);
        let readable_until = if readable_prefix_only {
            stable_end.map_or(0, |end| end + 1)
        } else {
            positions.len()
        };
        let profile = PromptCacheProfile {
            model,
            salt: salt.clone(),
            readable_prefix_only,
            #[cfg(test)]
            profile_domain: payload.profile_arn.clone(),
            legacy: None,
            service_id: salt.service_id.clone(),
            api_key_id: salt.api_key_id.clone(),
            mode,
            full,
            positions,
            readable_until,
        };
        if profile.writes().is_empty() {
            tracing::debug!(
                event = "proxy.prompt_cache.no_breakpoints",
                unit_count = units.len(),
                full_tokens = full,
                client_breakpoints = units.iter().filter(|u| u.cache_point.is_some()).count(),
                injected_units = units
                    .iter()
                    .filter(|u| u.kind == PrefixUnitKind::Injected)
                    .count(),
                "translated request has no usable cache breakpoint"
            );
            None
        } else {
            Some(profile)
        }
    }

    /// Take a bounded snapshot for this attempt at T0. A profile built for a
    /// different model than the accepted attempt does not apply.
    #[cfg(test)]
    pub fn plan(
        &self,
        account_id: &str,
        model: &str,
        profile: Option<&PromptCacheProfile>,
    ) -> PromptCachePlan {
        let snapshot = profile
            .map(|p| self.snapshot(account_id, &p.service_id, p.api_key_id.as_deref()))
            .unwrap_or_default();
        self.plan_snapshot(
            account_id,
            model,
            profile,
            &snapshot,
            "",
            profile.and_then(|p| p.profile_domain.as_deref()),
            0,
        )
    }

    pub(crate) fn snapshot_profile(
        &self,
        account_id: &str,
        profile: Option<&PromptCacheProfile>,
    ) -> AttemptSnapshot {
        profile
            .filter(|p| p.legacy.is_none())
            .map(|p| self.snapshot(account_id, &p.service_id, p.api_key_id.as_deref()))
            .unwrap_or_default()
    }

    pub(crate) fn snapshot_payload(
        &self,
        account_id: &str,
        service_id: &str,
        key_id: Option<&str>,
        payload: &KiroPayload,
    ) -> AttemptSnapshot {
        let marked = payload
            .conversation_state
            .current_message
            .user_input_message
            .local_sources
            .is_some()
            || payload.conversation_state.history.iter().any(|message| {
                message
                    .user_input_message
                    .as_ref()
                    .is_some_and(|m| m.local_sources.is_some())
                    || message
                        .assistant_response_message
                        .as_ref()
                        .is_some_and(|m| m.local_sources.is_some())
            });
        if marked {
            self.snapshot(account_id, service_id, key_id)
        } else {
            AttemptSnapshot::default()
        }
    }

    /// An empty retry may add transport instructions or switch the model.
    /// Recount its actual payload while preserving the admission-time mode.
    pub(crate) async fn retry_profile(
        &self,
        tokenizer: &TokenCountCache,
        profile: Option<&PromptCacheProfile>,
        payload: &KiroPayload,
    ) -> Option<PromptCacheProfile> {
        let profile = profile?;
        if profile.legacy.is_some() {
            return Some(profile.clone());
        }
        self.profile(
            tokenizer,
            payload,
            profile.mode,
            &profile.salt,
            profile.readable_prefix_only,
        )
        .await
    }

    pub(crate) fn snapshot(
        &self,
        account_id: &str,
        service_id: &str,
        key_id: Option<&str>,
    ) -> AttemptSnapshot {
        let scope = Scope {
            account_id: account_id.into(),
            service_id: service_id.into(),
            api_key_id: key_id.map(str::to_owned),
        };
        let at = Instant::now();
        AttemptSnapshot {
            entries: self.store.snapshot(&scope, at),
            scope: Some(scope),
            at: Some(at),
        }
    }

    pub(crate) fn plan_accepted(
        &self,
        account_id: &str,
        model: &str,
        profile: Option<&PromptCacheProfile>,
        snapshot: &AttemptSnapshot,
        response: &crosslane_kiro::KiroResponse,
        payload: &KiroPayload,
    ) -> PromptCachePlan {
        let domain = serde_json::json!([
            format!("{:?}", response.endpoint.key),
            response.endpoint.url,
            response.endpoint.origin
        ])
        .to_string();
        self.plan_snapshot(
            account_id,
            model,
            profile,
            snapshot,
            &domain,
            payload.profile_arn.as_deref(),
            response.stream_slot_wait_ms(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn plan_snapshot(
        &self,
        account_id: &str,
        model: &str,
        profile: Option<&PromptCacheProfile>,
        snapshot: &AttemptSnapshot,
        endpoint: &str,
        profile_domain: Option<&str>,
        stream_slot_wait_ms: u64,
    ) -> PromptCachePlan {
        let Some(profile) = profile else {
            return PromptCachePlan::default();
        };
        if profile.model != model {
            return PromptCachePlan {
                mismatch: true,
                ..PromptCachePlan::default()
            };
        }
        if let Some(legacy) = &profile.legacy {
            return PromptCachePlan {
                legacy: Some(self.legacy.plan(account_id, model, Some(legacy))),
                ..PromptCachePlan::default()
            };
        }
        let scope = Scope {
            account_id: account_id.to_owned(),
            service_id: profile.service_id.clone(),
            api_key_id: profile.api_key_id.clone(),
        };
        let mut profile = profile.clone();
        let domain = serde_json::to_vec(&(endpoint, profile_domain)).unwrap_or_default();
        for position in profile
            .positions
            .iter_mut()
            .filter(|_| !endpoint.is_empty() || profile_domain.is_some())
        {
            let mut hasher = Sha256::new();
            hasher.update(&domain);
            hasher.update(position.fingerprint);
            position.fingerprint = hasher.finalize().into();
        }
        PromptCachePlan {
            snapshot: if snapshot.scope.as_ref() == Some(&scope) {
                snapshot.entries.clone()
            } else {
                HashMap::new()
            },
            snapshot_at: snapshot.at,
            stream_slot_wait_ms,
            profile: Some(profile),
            mismatch: false,
            legacy: None,
            scope: Some(scope),
        }
    }

    /// Legacy profiles keep the previous request-based builders for rollback.
    pub async fn legacy_claude_profile(
        &self,
        tokenizer: &TokenCountCache,
        request: &crosslane_translate::ClaudeRequest,
        total_tokens: u64,
        model: &str,
        compacted: bool,
    ) -> Option<PromptCacheProfile> {
        let profile = if compacted {
            self.legacy
                .claude_compacted_profile(tokenizer, request, total_tokens, model)
                .await
        } else {
            self.legacy
                .claude_profile(tokenizer, request, total_tokens, model)
                .await
        }?;
        Some(PromptCacheProfile::from_legacy(model, profile))
    }

    pub async fn legacy_openai_profile(
        &self,
        tokenizer: &TokenCountCache,
        request: &crosslane_translate::OpenAiRequest,
        total_tokens: u64,
        model: &str,
    ) -> Option<PromptCacheProfile> {
        let profile = self
            .legacy
            .openai_profile(tokenizer, request, total_tokens, model)
            .await?;
        Some(PromptCacheProfile::from_legacy(model, profile))
    }

    /// End of a successful request. v3 publishes the accepted round-0 intent;
    /// legacy runs its original commit on the cumulative response.
    pub(crate) fn finish_request(
        &self,
        trace_id: &str,
        account_id: &str,
        plan: &PromptCachePlan,
        ledger: &mut CacheLedger,
        decoded: &mut crate::http::response::DecodedResponse,
    ) {
        if let Some(legacy) = &plan.legacy {
            self.legacy.commit(account_id, legacy, decoded);
            ledger.log(trace_id, plan, decoded, None, false);
            return;
        }
        let intent = ledger.take_intent();
        let policy_default = intent.as_ref().is_some_and(|intent| !intent.policy_listed);
        let published = intent.as_ref().map(|intent| self.publish(intent));
        if published == Some(0) {
            ledger.initial_skip = Some(SkipReason::ExpiredAtPublish);
        }
        ledger.log(trace_id, plan, decoded, published, policy_default);
    }

    /// Log a request that ended without publication (failure or refusal).
    pub(crate) fn abandon_request(
        &self,
        trace_id: &str,
        plan: &PromptCachePlan,
        ledger: &mut CacheLedger,
        decoded: &crate::http::response::DecodedResponse,
    ) {
        ledger.cancel_publication(SkipReason::NotPublished);
        ledger.log(trace_id, plan, decoded, None, false);
    }

    /// Publish an accepted initial round. Skips slots that would already be
    /// expired; never shortens an existing entry.
    /// Returns how many slots were written; zero means every slot expired.
    pub(crate) fn publish(&self, intent: &PublicationIntent) -> usize {
        let now = Instant::now();
        let updates: Vec<Update> = intent
            .updates
            .iter()
            .filter_map(|(index, ttl)| {
                let expiry = intent.dispatched_at + ttl.duration();
                (expiry > now).then(|| Update {
                    fingerprint: intent.profile.positions[*index].fingerprint,
                    ttl: *ttl,
                    expiry,
                })
            })
            .collect();
        self.store.publish(&intent.scope, &updates, now);
        updates.len()
    }
}

fn has_explicit_marker(payload: &KiroPayload) -> bool {
    let current = &payload
        .conversation_state
        .current_message
        .user_input_message;
    let marked = |sources: Option<&std::sync::Arc<crosslane_translate::LocalSources>>| {
        sources.is_some_and(|sources| {
            sources
                .blocks
                .iter()
                .any(|block| block.cache_point.is_some())
        })
    };
    current
        .user_input_message_context
        .as_ref()
        .is_some_and(|context| {
            context
                .tools
                .iter()
                .any(|tool| matches!(tool, crosslane_translate::KiroTool::CachePoint { .. }))
        })
        || marked(current.local_sources.as_ref())
        || payload.conversation_state.history.iter().any(|message| {
            marked(
                message
                    .user_input_message
                    .as_ref()
                    .and_then(|user| user.local_sources.as_ref()),
            ) || marked(
                message
                    .assistant_response_message
                    .as_ref()
                    .and_then(|assistant| assistant.local_sources.as_ref()),
            )
        })
}

impl PromptCachePlan {
    /// Counterfactual old clamp on the same prefix counts, without a second
    /// store or a second tokenization pass. This is not a legacy-store replay.
    fn clamped_comparison(&self, input: u64, dispatched: Option<Instant>) -> (u64, u64) {
        if let Some(usage) = self.legacy_usage(input) {
            return usage;
        }
        let Some(profile) = &self.profile else {
            return (0, 0);
        };
        let last = profile
            .writes()
            .iter()
            .map(|point| point.cum)
            .max()
            .unwrap_or(0);
        let matched = dispatched
            .map(|at| {
                self.hits(profile, at)
                    .into_iter()
                    .map(|(hit, _)| hit.cum)
                    .filter(|cum| *cum <= last)
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        let read = matched.min(input);
        (
            read,
            last.saturating_sub(matched).min(input.saturating_sub(read)),
        )
    }

    pub(crate) fn is_legacy(&self) -> bool {
        self.legacy.is_some()
    }

    /// Legacy provisional counters, unchanged from the previous algorithm.
    pub(crate) fn legacy_usage(&self, input: u64) -> Option<(u64, u64)> {
        self.legacy.as_ref().map(|plan| plan.usage(input))
    }

    /// Allocate `input` for this attempt. Hits must still be valid at the
    /// attempt's dispatch time `dispatched_at` (T1). Returns `None` when
    /// local accounting does not apply.
    pub(crate) fn allocate(
        &self,
        input: u64,
        dispatched_at: Instant,
    ) -> Result<(Allocation, PublicationIntent), SkipReason> {
        let profile = self.profile.as_ref().ok_or(if self.mismatch {
            SkipReason::ModelMismatch
        } else {
            SkipReason::NoProfile
        })?;
        if input == 0 || profile.full == 0 {
            return Err(SkipReason::EmptyInput);
        }
        let policy = policy::policy(&profile.model);
        let writes = profile.writes();
        let hits = self.hits(profile, dispatched_at);
        let selected = select_hit(
            &hits.iter().map(|(hit, _)| *hit).collect::<Vec<_>>(),
            &writes,
        );
        let allocation = allocate(
            input,
            profile.full,
            &writes,
            selected,
            policy.min_tokens,
            policy.boundary,
        )
        .ok_or(SkipReason::BelowThreshold)?;
        let shared = input.min(profile.full);
        let qualifies = |cum: u64| {
            ((u128::from(shared) * u128::from(cum)) / u128::from(profile.full)) as u64
                >= policy.min_tokens
        };
        let mut updates: Vec<(usize, Ttl)> = profile
            .positions
            .iter()
            .enumerate()
            .take(profile.readable_until)
            .filter_map(|(index, position)| {
                position
                    .write
                    .filter(|_| position.cum < profile.full && qualifies(position.cum))
                    .map(|ttl| (index, ttl))
            })
            .collect();
        // Refresh only the selected read slot, not every candidate.
        if let Some(hit) = selected {
            if let Some((index, ttl)) = hits
                .iter()
                .find(|(candidate, _)| candidate.cum == hit.cum)
                .map(|(_, refresh)| *refresh)
            {
                updates.push((index, ttl));
            }
        }
        Ok((
            allocation,
            PublicationIntent {
                scope: self.scope.clone().ok_or(SkipReason::NoProfile)?,
                dispatched_at,
                updates,
                profile: profile.clone(),
                policy_listed: policy.listed,
            },
        ))
    }

    /// Facts for the accounting log; never prompt content.
    pub(crate) fn log_fields(&self) -> (&'static str, u64, &str) {
        match &self.profile {
            Some(profile) => (
                match profile.mode {
                    CacheMode::Explicit => "explicit",
                    CacheMode::Implicit => "implicit",
                },
                profile.full,
                profile.model.as_str(),
            ),
            None if self.legacy.is_some() => ("legacy", 0, ""),
            None => ("none", 0, ""),
        }
    }

    /// Candidate hits with the slot to refresh when selected.
    fn hits(
        &self,
        profile: &PromptCacheProfile,
        dispatched_at: Instant,
    ) -> Vec<(Hit, (usize, Ttl))> {
        let last_write_slot = profile
            .positions
            .iter()
            .take(profile.readable_until)
            .filter(|position| position.write.is_some())
            .map(|position| position.slot)
            .collect::<Vec<_>>();
        let in_window = |position: &Position| match profile.mode {
            // Each breakpoint looks back over 20 positions, itself included.
            CacheMode::Explicit => last_write_slot
                .iter()
                .any(|slot| position.slot <= *slot && slot - position.slot < LOOKBACK_POSITIONS),
            CacheMode::Implicit => true,
        };
        profile
            .positions
            .iter()
            .enumerate()
            .take(profile.readable_until)
            .filter(|(_, position)| in_window(position) || position.stable_prefix)
            .filter_map(|(index, position)| {
                let slots = self.snapshot.get(&position.fingerprint)?;
                let long = slots.long.filter(|slot| slot.valid_at(dispatched_at));
                let short = slots.short.filter(|slot| slot.valid_at(dispatched_at));
                let refresh = if long.is_some() {
                    Ttl::OneHour
                } else if short.is_some() {
                    Ttl::FiveMinutes
                } else {
                    return None;
                };
                Some((
                    Hit {
                        cum: position.cum,
                        long_lived: long.is_some(),
                    },
                    (index, refresh),
                ))
            })
            .collect()
    }
}

/// Request controls hashed ahead of all content. Wire transport identifiers
/// (conversation ID, profile ARN) and the inference token budget are excluded.
fn prelude(payload: &KiroPayload, model: &str, salt: &ProfileSalt) -> Vec<u8> {
    let intent = payload.model_request_intent.as_ref();
    serde_json::to_vec(&serde_json::json!({
        "model": model,
        "agent_mode": payload.conversation_state.agent_task_type,
        "origin": payload.conversation_state.current_message.user_input_message.origin,
        "model_fields": payload.additional_model_request_fields,
        "thinking": intent.and_then(|intent| serde_json::to_value(&intent.thinking).ok()),
        "effort": intent.and_then(|intent| intent.effort.clone()),
        "prompt_cache_key": salt.prompt_cache_key,
        "tool_choice": salt.tool_choice,
        "parallel_tool_calls": salt.parallel_tool_calls,
    }))
    .unwrap_or_default()
}

#[cfg(test)]
mod tests;
