//! Per-round cache accounting shared by streaming and non-streaming paths.
//!
//! Round 0 is the initial generation for the client's request; only an
//! accepted round 0 may use local simulation. An empty-output round does not
//! consume round 0, because the empty-result retry resends the same prompt. Later rounds keep a reported
//! upstream cache group, otherwise their whole input is uncached. Each round
//! is settled exactly once, so a plan built for round 0 is never re-applied to
//! the request total.

use std::time::Instant;

use super::{Allocation, PromptCachePlan, PublicationIntent, SkipReason};
use crate::http::response::DecodedResponse;
use crate::http::response::UsageSources;

/// Request-level cumulative cache totals. Constant space across rounds.
#[derive(Debug, Clone, Default)]
pub(crate) struct CacheLedger {
    pub read: u64,
    pub write: u64,
    /// Part of `write` created by 1h breakpoints; the rest is 5m.
    pub write_1h: u64,
    pub any_local: bool,
    pub any_upstream: bool,
    /// Settled rounds, for the accounting log.
    pub settled_rounds: u32,
    /// Round 0 has been consumed: it produced output or carried an upstream
    /// cache group. An empty round that is retried with the same prompt does
    /// not consume it, so the retry is still treated as the initial generation.
    initial_settled: bool,
    /// Why round 0 did not use local simulation, when it did not.
    pub initial_skip: Option<SkipReason>,
    /// Round 0 normalized input, for the accounting log.
    initial_input: u64,
    intent: Option<PublicationIntent>,
    sources: UsageSources,
    initial_dispatch: Option<Instant>,
    present_fields: u8,
    zero_fields: u8,
    positive_fields: u8,
}

impl CacheLedger {
    pub fn is_initial(&self) -> bool {
        !self.initial_settled
    }
    pub fn input_estimate(&self, plan: &PromptCachePlan) -> Option<u64> {
        (!self.initial_settled)
            .then(|| {
                plan.profile
                    .as_ref()
                    .map(super::PromptCacheProfile::full_tokens)
            })
            .flatten()
            .filter(|full| *full > 0)
    }

    /// Apply cache accounting to one completed round in place. The round's
    /// `decoded.usage.input_tokens` must already be its normalized total.
    pub fn settle_round(
        &mut self,
        plan: &PromptCachePlan,
        dispatched_at: Option<Instant>,
        decoded: &mut DecodedResponse,
    ) {
        if !self.initial_settled {
            self.initial_dispatch = dispatched_at;
        }
        self.settle_cache(plan, dispatched_at, decoded);
        self.sources.merge(decoded.usage_sources());
        let (present, zero, positive) = decoded.usage_field_states();
        self.present_fields |= present;
        self.zero_fields |= zero;
        self.positive_fields |= positive;
    }

    fn settle_cache(
        &mut self,
        plan: &PromptCachePlan,
        dispatched_at: Option<Instant>,
        decoded: &mut DecodedResponse,
    ) {
        let initial = !self.initial_settled;
        self.settled_rounds = self.settled_rounds.saturating_add(1);
        if initial {
            self.initial_input = decoded.usage.input_tokens;
        }
        if plan.is_legacy() {
            // Legacy accounting commits once on the cumulative response.
            self.initial_settled |= decoded.has_generated_output();
            return;
        }
        if decoded.has_upstream_cache_usage() {
            self.initial_settled = true;
            self.any_upstream = true;
            self.read = self.read.saturating_add(decoded.usage.cache_read_tokens);
            self.write = self.write.saturating_add(decoded.usage.cache_write_tokens);
            if initial {
                self.initial_skip = Some(SkipReason::UpstreamReported);
            }
            return;
        }
        decoded.usage.cache_read_tokens = 0;
        decoded.usage.cache_write_tokens = 0;
        if !initial {
            return;
        }
        if !decoded.has_generated_output() {
            // Leave round 0 open: an empty-output retry resends this prompt.
            self.initial_skip = Some(SkipReason::NoOutput);
            return;
        }
        self.initial_settled = true;
        let local = dispatched_at
            .ok_or(SkipReason::NoProfile)
            .and_then(|dispatched_at| plan.allocate(decoded.usage.input_tokens, dispatched_at));
        let (allocation, intent) = match local {
            Ok(local) => local,
            Err(reason) => {
                self.initial_skip = Some(reason);
                return;
            }
        };
        decoded.usage.cache_read_tokens = allocation.read;
        decoded.usage.cache_write_tokens = allocation.write();
        decoded.cache_usage_estimated = true;
        self.initial_skip = None;
        self.any_local = true;
        self.read = self.read.saturating_add(allocation.read);
        self.write = self.write.saturating_add(allocation.write());
        self.write_1h = self.write_1h.saturating_add(allocation.write_1h);
        self.intent = Some(intent);
    }

    /// Provisional values for a `message_start` that has not been sent yet.
    pub fn provisional(
        plan: &PromptCachePlan,
        dispatched_at: Option<Instant>,
        input: u64,
    ) -> Allocation {
        if let Some((read, write)) = plan.legacy_usage(input) {
            return Allocation {
                read,
                write_5m: write,
                write_1h: 0,
                uncached: input.saturating_sub(read).saturating_sub(write),
            };
        }
        dispatched_at
            .and_then(|dispatched_at| plan.allocate(input, dispatched_at).ok())
            .map_or_else(
                || Allocation::uncached_only(input),
                |(allocation, _)| allocation,
            )
    }

    /// Mark the cumulative response when any round used local simulation, so
    /// mixed local/upstream totals are never labelled as server-reported.
    /// The 1h share of local writes travels with the response for output.
    pub fn mark_cumulative(&self, decoded: &mut DecodedResponse) {
        decoded.cumulative_usage_sources = Some(self.sources);
        if self.any_local {
            decoded.cache_usage_estimated = true;
            decoded.cache_write_1h_tokens = self.write_1h.min(decoded.usage.cache_write_tokens);
        }
    }

    /// Cancel local publication when the request will not complete cleanly.
    pub fn cancel_publication(&mut self, reason: SkipReason) {
        if self.intent.take().is_some() {
            self.initial_skip = Some(reason);
        }
    }

    pub(super) fn take_intent(&mut self) -> Option<PublicationIntent> {
        self.intent.take()
    }

    /// One debug line per request: mode, totals, and why local accounting was
    /// skipped. Numbers and model ID only; never prompt content or keys.
    pub(super) fn log(
        &self,
        trace_id: &str,
        plan: &PromptCachePlan,
        decoded: &DecodedResponse,
        published: Option<usize>,
        policy_default: bool,
    ) {
        let (mode, full, model) = plan.log_fields();
        let skip = self.initial_skip.map(SkipReason::as_str);
        let policy_default =
            policy_default || (!model.is_empty() && !super::policy::policy(model).listed);
        let sampled = trace_id.bytes().fold(0u64, |hash, byte| {
            hash.wrapping_mul(31).wrapping_add(u64::from(byte))
        }) % 100
            == 0;
        let info = sampled
            || skip.is_some()
            || policy_default
            || self.sources.input & UsageSources::ADJUSTED != 0;
        let dispatch_delay_ms =
            self.initial_dispatch
                .zip(plan.snapshot_at)
                .map(|(dispatch, snapshot)| {
                    dispatch.saturating_duration_since(snapshot).as_millis() as u64
                });
        let (legacy_read, legacy_write) =
            plan.clamped_comparison(self.initial_input, self.initial_dispatch);
        macro_rules! emit {
            ($level:expr) => {
                tracing::event!(
                    $level,
                    event = "proxy.prompt_cache.accounting",
                    trace_id,
                    mode,
                    model,
                    policy_source = if policy_default { "default" } else { "table" },
                    rounds = self.settled_rounds,
                    completion = if published.is_some() {
                        "completed"
                    } else {
                        "no_publication"
                    },
                    input_source = UsageSources::name(self.sources.input),
                    output_source = UsageSources::name(self.sources.output),
                    cache_source = UsageSources::name(self.sources.cache),
                    full_tokens = full,
                    initial_input_tokens = self.initial_input,
                    input_tokens = decoded.usage.input_tokens,
                    cache_read_tokens = decoded.usage.cache_read_tokens,
                    cache_write_tokens = decoded.usage.cache_write_tokens,
                    cache_write_1h_tokens = decoded.cache_write_1h_tokens,
                    uncached_tokens = decoded
                        .usage
                        .input_tokens
                        .saturating_sub(decoded.usage.cache_read_tokens)
                        .saturating_sub(decoded.usage.cache_write_tokens),
                    legacy_clamped_read_tokens = legacy_read,
                    legacy_clamped_write_tokens = legacy_write,
                    legacy_clamped_uncached_tokens = self
                        .initial_input
                        .saturating_sub(legacy_read)
                        .saturating_sub(legacy_write),
                    dispatch_delay_ms,
                    stream_slot_wait_ms = plan.stream_slot_wait_ms,
                    usage_fields_present = self.present_fields,
                    usage_fields_zero = self.zero_fields,
                    usage_fields_positive = self.positive_fields,
                    local = self.any_local,
                    upstream = self.any_upstream,
                    published_slots = published,
                    skip_reason = skip,
                    "prompt cache accounting settled"
                );
            };
        }
        if info {
            emit!(tracing::Level::INFO);
        } else {
            emit!(tracing::Level::DEBUG);
        }
    }
}
