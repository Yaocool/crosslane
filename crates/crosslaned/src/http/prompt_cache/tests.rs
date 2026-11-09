use std::time::{Duration, Instant};

use crosslane_translate::{
    claude_to_kiro, openai_to_kiro, ClaudeRequest, KiroPayload, OpenAiRequest, TokenCountCache,
    TranslationOptions,
};
use serde_json::{json, Value};

use super::*;
use crate::http::response::DecodedResponse;

const MODEL: &str = "claude-opus-5.5";

#[tokio::test]
async fn unmarked_explicit_profile_skips_tokenization_but_implicit_keeps_anchors() {
    let tracker = PromptCacheTracker::default();
    let tokenizer = TokenCountCache::new(64).unwrap();
    let payload = claude_payload(json!({
        "model":MODEL, "max_tokens":64,
        "messages":[{"role":"user","content":"unmarked content ".repeat(4096)}]
    }));
    assert!(tracker
        .profile(
            &tokenizer,
            &payload,
            CacheMode::Explicit,
            &ProfileSalt::default(),
            false
        )
        .await
        .is_none());
    assert_eq!(tokenizer.stats().hits + tokenizer.stats().misses, 0);
    let implicit = tracker
        .profile(
            &tokenizer,
            &payload,
            CacheMode::Implicit,
            &ProfileSalt::default(),
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        implicit.full_tokens(),
        tokenizer.estimate_kiro_payload(&payload).await.unwrap() as u64
    );
    assert!(!implicit.writes().is_empty());
}

fn scope(account: &str, key: Option<&str>) -> Scope {
    Scope {
        account_id: account.into(),
        service_id: "service".into(),
        api_key_id: key.map(str::to_owned),
    }
}

fn claude_payload(request: Value) -> KiroPayload {
    let request: ClaudeRequest = serde_json::from_value(request).unwrap();
    let mut options = TranslationOptions::new(MODEL, "AI_EDITOR");
    options.enable_prompt_cache = true;
    options.enhance_system_prompt = false;
    claude_to_kiro(&request, &options)
}

fn claude(system: &str, messages: Value) -> KiroPayload {
    claude_payload(json!({
        "model":MODEL,"max_tokens":64,
        "system":[{"type":"text","text":system,"cache_control":{"type":"ephemeral"}}],
        "messages":messages,
    }))
}

async fn profile(
    tracker: &PromptCacheTracker,
    payload: &KiroPayload,
    mode: CacheMode,
) -> PromptCacheProfile {
    tracker
        .profile(
            &TokenCountCache::new(64).unwrap(),
            payload,
            mode,
            &ProfileSalt::default(),
            false,
        )
        .await
        .expect("profile")
}

/// Allocate one round at its own full count and publish it.
fn plan_for(
    tracker: &PromptCacheTracker,
    scope: Scope,
    profile: &PromptCacheProfile,
) -> PromptCachePlan {
    let mut profile = profile.clone();
    profile.service_id = scope.service_id;
    profile.api_key_id = scope.api_key_id;
    tracker.plan(&scope.account_id, MODEL, Some(&profile))
}

fn run(tracker: &PromptCacheTracker, scope: Scope, profile: &PromptCacheProfile) -> Allocation {
    let plan = plan_for(tracker, scope, profile);
    let (allocation, intent) = plan
        .allocate(profile.full_tokens(), Instant::now())
        .expect("local accounting applies");
    tracker.publish(&intent);
    allocation
}

fn stable() -> String {
    "stable library reference ".repeat(400)
}

#[tokio::test]
async fn cold_write_then_warm_read_keeps_a_positive_tail() {
    let tracker = PromptCacheTracker::default();
    let payload = claude(&stable(), json!([{"role":"user","content":"hello"}]));
    let p = profile(&tracker, &payload, CacheMode::Explicit).await;
    let cold = run(&tracker, scope("a", None), &p);
    assert_eq!(cold.read, 0);
    assert!(cold.write() > 512);
    assert!(cold.uncached > 0);
    let warm = run(&tracker, scope("a", None), &p);
    assert_eq!(warm.read, cold.write());
    assert_eq!(warm.write(), 0);
    assert_eq!(
        warm.uncached, cold.uncached,
        "uncached input never collapses to 0"
    );
}

#[tokio::test]
async fn full_count_equals_the_context_estimate() {
    let tracker = PromptCacheTracker::default();
    let tokenizer = TokenCountCache::new(64).unwrap();
    let payload = claude(&stable(), json!([{"role":"user","content":"hello"}]));
    let p = profile(&tracker, &payload, CacheMode::Explicit).await;
    assert_eq!(
        p.full_tokens(),
        tokenizer.estimate_kiro_payload(&payload).await.unwrap() as u64
    );
}

#[tokio::test]
async fn growing_conversation_reads_the_prior_prefix() {
    let tracker = PromptCacheTracker::default();
    let first = claude(&stable(), json!([{"role":"user","content":"question one"}]));
    let p1 = profile(&tracker, &first, CacheMode::Explicit).await;
    let cold = run(&tracker, scope("a", None), &p1);
    let grown = claude(
        &stable(),
        json!([
            {"role":"user","content":"question one"},
            {"role":"assistant","content":"answer"},
            {"role":"user","content":[{"type":"text","text":"follow up ".repeat(200),"cache_control":{"type":"ephemeral"}}]}
        ]),
    );
    let p2 = profile(&tracker, &grown, CacheMode::Explicit).await;
    let next = run(&tracker, scope("a", None), &p2);
    assert_eq!(next.read, cold.write(), "system breakpoint is reused");
    assert!(next.write() > 0);
    assert!(next.uncached > 0);
}

#[tokio::test]
async fn changed_tools_invalidate_and_scopes_isolate() {
    let tracker = PromptCacheTracker::default();
    let payload = claude(&stable(), json!([{"role":"user","content":"hello"}]));
    let p = profile(&tracker, &payload, CacheMode::Explicit).await;
    run(&tracker, scope("a", Some("k1")), &p);
    for other in [
        scope("b", Some("k1")),
        scope("a", Some("k2")),
        scope("a", None),
    ] {
        let plan = plan_for(&tracker, other, &p);
        assert_eq!(
            plan.allocate(p.full_tokens(), Instant::now())
                .unwrap()
                .0
                .read,
            0
        );
    }
    let with_tool = claude_payload(json!({
        "model":MODEL,"max_tokens":64,
        "system":[{"type":"text","text":stable(),"cache_control":{"type":"ephemeral"}}],
        "tools":[{"name":"Read","input_schema":{"type":"object"}}],
        "messages":[{"role":"user","content":"hello"}],
    }));
    let changed = profile(&tracker, &with_tool, CacheMode::Explicit).await;
    let plan = plan_for(&tracker, scope("a", Some("k1")), &changed);
    assert_eq!(
        plan.allocate(changed.full_tokens(), Instant::now())
            .unwrap()
            .0
            .read,
        0
    );
}

#[tokio::test]
async fn entries_published_after_dispatch_or_expired_do_not_hit() {
    let tracker = PromptCacheTracker::default();
    let payload = claude(&stable(), json!([{"role":"user","content":"hello"}]));
    let p = profile(&tracker, &payload, CacheMode::Explicit).await;
    let dispatched_before = Instant::now();
    std::thread::sleep(Duration::from_millis(2));
    run(&tracker, scope("a", None), &p);
    let plan = plan_for(&tracker, scope("a", None), &p);
    assert_eq!(
        plan.allocate(p.full_tokens(), dispatched_before)
            .unwrap()
            .0
            .read,
        0
    );
    tracker.store.expire_all(Instant::now());
    let plan = plan_for(&tracker, scope("a", None), &p);
    assert_eq!(
        plan.allocate(p.full_tokens(), Instant::now())
            .unwrap()
            .0
            .read,
        0
    );
}

#[tokio::test]
async fn small_prefixes_stay_cold_and_unmarked_requests_have_no_profile() {
    let tracker = PromptCacheTracker::default();
    let small = claude("small prefix", json!([{"role":"user","content":"hello"}]));
    let p = profile(&tracker, &small, CacheMode::Explicit).await;
    assert!(plan_for(&tracker, scope("a", None), &p)
        .allocate(p.full_tokens(), Instant::now())
        .is_err());
    let unmarked = claude_payload(json!({
        "model":MODEL,"max_tokens":64,"system":stable(),
        "messages":[{"role":"user","content":"hello"}],
    }));
    assert!(tracker
        .profile(
            &TokenCountCache::new(8).unwrap(),
            &unmarked,
            CacheMode::Explicit,
            &ProfileSalt::default(),
            false
        )
        .await
        .is_none());
}

#[tokio::test]
async fn billing_header_drift_keeps_the_prefix() {
    let tracker = PromptCacheTracker::default();
    let with_header = |version: &str| {
        claude_payload(json!({
            "model":MODEL,"max_tokens":64,
            "system":[
                {"type":"text","text":format!("x-anthropic-billing-header: cc_version={version}; cc_entrypoint=cli;")},
                {"type":"text","text":stable(),"cache_control":{"type":"ephemeral"}}
            ],
            "messages":[{"role":"user","content":"hello"}],
        }))
    };
    let first = profile(&tracker, &with_header("1.0.0"), CacheMode::Explicit).await;
    let cold = run(&tracker, scope("a", None), &first);
    let second = profile(&tracker, &with_header("2.0.0"), CacheMode::Explicit).await;
    let warm = run(&tracker, scope("a", None), &second);
    assert_eq!(warm.read, cold.write());
}

#[tokio::test]
async fn prompt_cache_key_and_tool_choice_salt_the_prefix() {
    let tracker = PromptCacheTracker::default();
    let tokenizer = TokenCountCache::new(64).unwrap();
    let payload = claude(&stable(), json!([{"role":"user","content":"hello"}]));
    let salted = |key: &str, choice: Value| ProfileSalt {
        service_id: "service".into(),
        api_key_id: None,
        prompt_cache_key: Some(key.into()),
        tool_choice: Some(choice),
        parallel_tool_calls: None,
    };
    let base = tracker
        .profile(
            &tokenizer,
            &payload,
            CacheMode::Explicit,
            &salted("one", json!("auto")),
            false,
        )
        .await
        .unwrap();
    run(&tracker, scope("a", None), &base);
    for salt in [
        salted("two", json!("auto")),
        salted("one", json!("required")),
    ] {
        let other = tracker
            .profile(&tokenizer, &payload, CacheMode::Explicit, &salt, false)
            .await
            .unwrap();
        let plan = plan_for(&tracker, scope("a", None), &other);
        assert_eq!(
            plan.allocate(other.full_tokens(), Instant::now())
                .unwrap()
                .0
                .read,
            0
        );
    }
}

fn openai(messages: Value) -> KiroPayload {
    let request: OpenAiRequest =
        serde_json::from_value(json!({"model":MODEL,"messages":messages})).unwrap();
    let mut options = TranslationOptions::new(MODEL, "AI_EDITOR");
    options.enable_prompt_cache = true;
    options.enhance_system_prompt = false;
    openai_to_kiro(&request, &options)
}

#[tokio::test]
async fn implicit_mode_hits_previous_turn_across_long_tool_runs() {
    let tracker = PromptCacheTracker::default();
    let mut messages = vec![
        json!({"role":"system","content":stable()}),
        json!({"role":"user","content":"start"}),
    ];
    let mut previous_write = None;
    for turn in 0..6 {
        for call in 0..12 {
            let id = format!("call_{turn}_{call}");
            messages.push(json!({"role":"assistant","content":null,"tool_calls":[
                {"id":id,"type":"function","function":{"name":"read","arguments":"{}"}}]}));
            messages.push(json!({"role":"tool","tool_call_id":id,"content":"result ".repeat(20)}));
        }
        let payload = openai(Value::Array(messages.clone()));
        let p = profile(&tracker, &payload, CacheMode::Implicit).await;
        assert!(
            p.writes().len() <= 2,
            "implicit mode writes at most two anchors"
        );
        let allocation = run(&tracker, scope("a", None), &p);
        if let Some(previous) = previous_write {
            assert!(
                allocation.read >= previous,
                "turn {turn} reads the prior boundary"
            );
        }
        previous_write = Some(allocation.read + allocation.write());
        assert!(allocation.uncached > 0);
    }
}

#[tokio::test]
async fn readable_prefix_only_never_reads_or_writes_history() {
    let tracker = PromptCacheTracker::default();
    let tokenizer = TokenCountCache::new(64).unwrap();
    let payload = claude(
        &stable(),
        json!([
            {"role":"user","content":[{"type":"text","text":"history ".repeat(800),"cache_control":{"type":"ephemeral"}}]},
            {"role":"assistant","content":"ok"},
            {"role":"user","content":"now"}
        ]),
    );
    let full = profile(&tracker, &payload, CacheMode::Explicit).await;
    let prefix = tracker
        .profile(
            &tokenizer,
            &payload,
            CacheMode::Explicit,
            &ProfileSalt::default(),
            true,
        )
        .await
        .unwrap();
    assert_eq!(
        prefix.full_tokens(),
        full.full_tokens(),
        "F still covers the real input"
    );
    let allocation = run(&tracker, scope("a", None), &prefix);
    let full_allocation = plan_for(&tracker, scope("b", None), &full)
        .allocate(full.full_tokens(), Instant::now())
        .unwrap()
        .0;
    assert!(allocation.write() < full_allocation.write());
    assert!(allocation.uncached > full_allocation.uncached);
}

#[tokio::test]
async fn ledger_settles_round_zero_locally_and_continuations_uncached() {
    let tracker = PromptCacheTracker::default();
    let payload = claude(&stable(), json!([{"role":"user","content":"hello"}]));
    let p = profile(&tracker, &payload, CacheMode::Explicit).await;
    let plan = plan_for(&tracker, scope("a", None), &p);
    let mut ledger = CacheLedger::default();
    let mut first = DecodedResponse {
        text: "answer".into(),
        ..Default::default()
    };
    first.usage.input_tokens = p.full_tokens();
    ledger.settle_round(&plan, Some(Instant::now()), &mut first);
    assert!(first.usage.cache_write_tokens > 0);
    let mut second = DecodedResponse {
        text: "more".into(),
        ..Default::default()
    };
    second.usage.input_tokens = 4000;
    ledger.settle_round(&plan, Some(Instant::now()), &mut second);
    assert_eq!(
        (
            second.usage.cache_read_tokens,
            second.usage.cache_write_tokens
        ),
        (0, 0)
    );
    assert_eq!(ledger.write, first.usage.cache_write_tokens);
    assert!(ledger.take_intent().is_some());
}

#[tokio::test]
async fn refusals_without_output_do_not_publish() {
    let tracker = PromptCacheTracker::default();
    let payload = claude(&stable(), json!([{"role":"user","content":"hello"}]));
    let p = profile(&tracker, &payload, CacheMode::Explicit).await;
    let plan = plan_for(&tracker, scope("a", None), &p);
    let mut ledger = CacheLedger::default();
    let mut refusal = DecodedResponse::default();
    refusal.usage.input_tokens = p.full_tokens();
    refusal.usage.credits = 0.1;
    ledger.settle_round(&plan, Some(Instant::now()), &mut refusal);
    assert!(ledger.take_intent().is_none());
    assert_eq!(refusal.usage.cache_write_tokens, 0);
}

#[tokio::test]
async fn skip_reasons_explain_zero_cache_counts() {
    let tracker = PromptCacheTracker::default();
    let small = claude("small prefix", json!([{"role":"user","content":"hello"}]));
    let p = profile(&tracker, &small, CacheMode::Explicit).await;
    let plan = plan_for(&tracker, scope("a", None), &p);
    let mut ledger = CacheLedger::default();
    let mut round = DecodedResponse {
        text: "answer".into(),
        ..Default::default()
    };
    round.usage.input_tokens = p.full_tokens();
    ledger.settle_round(&plan, Some(Instant::now()), &mut round);
    assert_eq!(ledger.initial_skip, Some(SkipReason::BelowThreshold));

    let other_model = tracker.plan("a", "claude-sonnet-4.5", Some(&p));
    assert_eq!(
        other_model.allocate(100, Instant::now()).err(),
        Some(SkipReason::ModelMismatch)
    );
    assert_eq!(
        PromptCachePlan::default()
            .allocate(100, Instant::now())
            .err(),
        Some(SkipReason::NoProfile)
    );
}

#[tokio::test]
async fn stop_sequence_cancellation_is_recorded() {
    let tracker = PromptCacheTracker::default();
    let payload = claude(&stable(), json!([{"role":"user","content":"hello"}]));
    let p = profile(&tracker, &payload, CacheMode::Explicit).await;
    let plan = plan_for(&tracker, scope("a", None), &p);
    let mut ledger = CacheLedger::default();
    let mut round = DecodedResponse {
        text: "answer".into(),
        ..Default::default()
    };
    round.usage.input_tokens = p.full_tokens();
    ledger.settle_round(&plan, Some(Instant::now()), &mut round);
    ledger.cancel_publication(SkipReason::NotEof);
    assert_eq!(ledger.initial_skip, Some(SkipReason::NotEof));
    assert!(ledger.take_intent().is_none());
    // Accounting for the round remains; only publication is withheld.
    assert!(round.usage.cache_write_tokens > 0);
}

#[tokio::test]
async fn one_hour_writes_flow_into_the_claude_breakdown() {
    let tracker = PromptCacheTracker::default();
    let payload = claude_payload(json!({
        "model":MODEL,"max_tokens":64,
        "system":[
            {"type":"text","text":stable(),"cache_control":{"type":"ephemeral","ttl":"1h"}},
        ],
        "messages":[
            {"role":"user","content":"earlier"},
            {"role":"assistant","content":"ok"},
            {"role":"user","content":[{"type":"text","text":"recent ".repeat(300),"cache_control":{"type":"ephemeral"}}]}
        ],
    }));
    let p = profile(&tracker, &payload, CacheMode::Explicit).await;
    let plan = plan_for(&tracker, scope("a", None), &p);
    let mut ledger = CacheLedger::default();
    let mut round = DecodedResponse {
        text: "answer".into(),
        ..Default::default()
    };
    round.usage.input_tokens = p.full_tokens();
    ledger.settle_round(&plan, Some(Instant::now()), &mut round);
    ledger.mark_cumulative(&mut round);
    let breakdown = round.claude_cache_creation();
    let long = breakdown["ephemeral_1h_input_tokens"].as_u64().unwrap();
    let short = breakdown["ephemeral_5m_input_tokens"].as_u64().unwrap();
    assert!(long > 0, "system prefix is written with the 1h TTL");
    assert!(short > 0, "the later 5m breakpoint writes the rest");
    assert_eq!(long + short, round.usage.cache_write_tokens);
}

#[tokio::test]
async fn empty_round_does_not_consume_the_initial_generation() {
    let tracker = PromptCacheTracker::default();
    let payload = claude(&stable(), json!([{"role":"user","content":"hello"}]));
    let p = profile(&tracker, &payload, CacheMode::Explicit).await;
    let plan = plan_for(&tracker, scope("a", None), &p);
    let mut ledger = CacheLedger::default();
    let mut empty = DecodedResponse::default();
    empty.usage.input_tokens = p.full_tokens();
    ledger.settle_round(&plan, Some(Instant::now()), &mut empty);
    assert_eq!(ledger.initial_skip, Some(SkipReason::NoOutput));
    assert_eq!(
        (
            empty.usage.cache_read_tokens,
            empty.usage.cache_write_tokens
        ),
        (0, 0)
    );

    // The empty-result retry resends the same prompt; it is still round 0.
    let mut retry = DecodedResponse {
        text: "answer".into(),
        ..Default::default()
    };
    retry.usage.input_tokens = p.full_tokens();
    ledger.settle_round(&plan, Some(Instant::now()), &mut retry);
    assert!(retry.usage.cache_write_tokens > 0);
    assert!(retry.cache_usage_estimated);
    assert_eq!(ledger.initial_skip, None);
    assert_eq!(ledger.settled_rounds, 2);
    tracker.finish_request("t", "a", &plan, &mut ledger, &mut retry);
    let warm = plan_for(&tracker, scope("a", None), &p);
    assert!(
        warm.allocate(p.full_tokens(), Instant::now())
            .unwrap()
            .0
            .read
            > 0
    );
}

#[tokio::test]
async fn abandoned_refusal_with_output_does_not_publish() {
    let tracker = PromptCacheTracker::default();
    let payload = claude(&stable(), json!([{"role":"user","content":"hello"}]));
    let p = profile(&tracker, &payload, CacheMode::Explicit).await;
    let plan = plan_for(&tracker, scope("a", None), &p);
    let mut ledger = CacheLedger::default();
    let mut refusal = DecodedResponse {
        text: "partial".into(),
        stop_reason: Some("refusal".into()),
        ..Default::default()
    };
    refusal.usage.input_tokens = p.full_tokens();
    ledger.settle_round(&plan, Some(Instant::now()), &mut refusal);
    assert!(
        refusal.usage.cache_write_tokens > 0,
        "the round is still accounted"
    );
    tracker.abandon_request("t", &plan, &mut ledger, &refusal);
    assert_eq!(ledger.initial_skip, Some(SkipReason::NotPublished));
    let next = plan_for(&tracker, scope("a", None), &p);
    assert_eq!(
        next.allocate(p.full_tokens(), Instant::now())
            .unwrap()
            .0
            .read,
        0
    );
}

#[tokio::test]
async fn review_uncached_system_tail_preserves_prefix_hit() {
    let tracker = PromptCacheTracker::default();
    let make = |tail: &str| {
        claude_payload(json!({
            "model":MODEL,"max_tokens":64,
            "system":[
                {"type":"text","text":stable(),"cache_control":{"type":"ephemeral"}},
                {"type":"text","text":tail}
            ],
            "messages":[{"role":"user","content":"hello"}]
        }))
    };
    let first = profile(&tracker, &make("uncached tail one"), CacheMode::Explicit).await;
    let cold = run(&tracker, scope("a", None), &first);
    let next = profile(&tracker, &make("uncached tail two"), CacheMode::Explicit).await;
    let warm = run(&tracker, scope("a", None), &next);
    assert!(warm.read > 0, "changing content AFTER the breakpoint lost the stable prefix: cold={cold:?}, next={warm:?}");
}

#[tokio::test]
async fn review_system_mixed_ttl_preserves_both_breakpoints() {
    let tracker = PromptCacheTracker::default();
    let payload = claude_payload(json!({
        "model":MODEL,"max_tokens":64,
        "system":[
            {"type":"text","text":stable(),"cache_control":{"type":"ephemeral","ttl":"1h"}},
            {"type":"text","text":"short prefix ".repeat(300),"cache_control":{"type":"ephemeral","ttl":"5m"}}
        ],
        "messages":[{"role":"user","content":"hello"}]
    }));
    let p = profile(&tracker, &payload, CacheMode::Explicit).await;
    let cold = run(&tracker, scope("a", None), &p);
    assert!(
        cold.write_1h > 0 && cold.write_5m > 0,
        "the system's two physical breakpoints must retain distinct TTLs: {cold:?}"
    );
}

#[tokio::test]
async fn review_profile_domain_isolates_cache() {
    let tracker = PromptCacheTracker::default();
    let mut first = claude(&stable(), json!([{"role":"user","content":"hello"}]));
    first.profile_arn = Some("arn:aws:codewhisperer:us-east-1:000000000000:profile/one".into());
    let p1 = profile(&tracker, &first, CacheMode::Explicit).await;
    run(&tracker, scope("a", None), &p1);
    first.profile_arn = Some("arn:aws:codewhisperer:us-east-1:000000000000:profile/two".into());
    let p2 = profile(&tracker, &first, CacheMode::Explicit).await;
    let next = run(&tracker, scope("a", None), &p2);
    assert_eq!(
        next.read, 0,
        "cache scope must isolate the upstream profile domain"
    );
}

#[tokio::test]
async fn review_cache_valid_at_dispatch_survives_late_response_snapshot() {
    let tracker = PromptCacheTracker::default();
    let payload = claude(&stable(), json!([{"role":"user","content":"hello"}]));
    let p = profile(&tracker, &payload, CacheMode::Explicit).await;
    let now = Instant::now();
    let position = p
        .positions
        .iter()
        .find(|position| position.write.is_some())
        .unwrap();
    tracker.store.publish(
        &scope("a", None),
        &[store::Update {
            fingerprint: position.fingerprint,
            ttl: Ttl::FiveMinutes,
            expiry: now + Duration::from_secs(1),
        }],
        now - Duration::from_secs(3),
    );
    let snapshot = tracker.snapshot("a", "service", None);
    let dispatched = now + Duration::from_millis(100);
    // A concurrent request prunes the store after this request was sent.
    tracker
        .store
        .snapshot(&scope("a", None), now + Duration::from_secs(2));
    let mut p = p;
    p.service_id = "service".into();
    let plan = tracker.plan_snapshot("a", MODEL, Some(&p), &snapshot, "", None, 0);
    let result = plan.allocate(p.full_tokens(), dispatched).unwrap().0;
    assert!(
        result.read > 0,
        "the pre-send snapshot must survive response-time pruning: {result:?}"
    );
    assert_eq!(
        plan.allocate(p.full_tokens(), now + Duration::from_secs(2))
            .unwrap()
            .0
            .read,
        0,
        "a queued request dispatched after expiry must miss"
    );
}

#[test]
fn review_cumulative_provenance_includes_previous_rounds() {
    let mut ledger = CacheLedger::default();
    let plan = PromptCachePlan::default();
    let mut first = DecodedResponse {
        text: "first".into(),
        ..Default::default()
    };
    first.usage.input_tokens = 4000;
    first.usage.output_tokens = 500;
    ledger.settle_round(&plan, None, &mut first);
    let mut second = DecodedResponse {
        text: "second".into(),
        ..Default::default()
    };
    second
        .push(crosslane_kiro::KiroEvent::Usage {
            usage: json!({"inputTokens":1000,"outputTokens":100,"cacheReadInputTokens":300}),
        })
        .unwrap();
    ledger.settle_round(&plan, None, &mut second);
    second.usage.input_tokens += first.usage.input_tokens;
    second.usage.output_tokens += first.usage.output_tokens;
    ledger.mark_cumulative(&mut second);
    assert_eq!(
        second.token_usage_source(),
        "estimated",
        "cumulative input contains a local estimate from the first round"
    );
}

#[tokio::test]
async fn review_injected_system_text_stays_uncached() {
    let tracker = PromptCacheTracker::default();
    let request: ClaudeRequest = serde_json::from_value(json!({
        "model":MODEL,"max_tokens":64,
        "system":[{"type":"text","text":stable(),"cache_control":{"type":"ephemeral"}}],
        "messages":[{"role":"user","content":"hello"}]
    }))
    .unwrap();
    let mut options = TranslationOptions::new(MODEL, "AI_EDITOR");
    options.enable_prompt_cache = true;
    options.enhance_system_prompt = false;
    let plain = profile(
        &tracker,
        &claude_to_kiro(&request, &options),
        CacheMode::Explicit,
    )
    .await;
    options.enhance_system_prompt = true;
    let enhanced = profile(
        &tracker,
        &claude_to_kiro(&request, &options),
        CacheMode::Explicit,
    )
    .await;
    let plain_alloc = run(&tracker, scope("plain", None), &plain);
    let enhanced_alloc = run(&tracker, scope("enhanced", None), &enhanced);
    let injected = enhanced.full_tokens() - plain.full_tokens();
    assert!(injected > 0);
    assert_eq!(
        enhanced_alloc.uncached,
        plain_alloc.uncached + injected,
        "the extra proxy instructions must not be included in cacheable client content"
    );
}

#[tokio::test]
async fn accepted_endpoint_and_snapshot_boundaries_are_isolated() {
    let tracker = PromptCacheTracker::default();
    let payload = claude(&stable(), json!([{"role":"user","content":"hello"}]));
    let mut p = profile(&tracker, &payload, CacheMode::Explicit).await;
    p.service_id = "service".into();
    let before = tracker.snapshot("a", "service", None);
    let cold = tracker.plan_snapshot(
        "a",
        MODEL,
        Some(&p),
        &before,
        "amazonq/origin-a",
        Some("profile-a"),
        17,
    );
    let (_, intent) = cold.allocate(p.full_tokens(), Instant::now()).unwrap();
    tracker.publish(&intent);
    let after = tracker.snapshot("a", "service", None);
    let plan = |snapshot: &AttemptSnapshot, endpoint, profile_domain| {
        tracker.plan_snapshot(
            "a",
            MODEL,
            Some(&p),
            snapshot,
            endpoint,
            Some(profile_domain),
            0,
        )
    };
    assert_eq!(
        plan(&before, "amazonq/origin-a", "profile-a")
            .allocate(p.full_tokens(), Instant::now())
            .unwrap()
            .0
            .read,
        0,
        "a publication after T0 is invisible even when it precedes T1"
    );
    assert!(
        plan(&after, "amazonq/origin-a", "profile-a")
            .allocate(p.full_tokens(), Instant::now())
            .unwrap()
            .0
            .read
            > 0
    );
    for (endpoint, domain) in [
        ("runtime/origin-a", "profile-a"),
        ("amazonq/origin-b", "profile-a"),
        ("amazonq/origin-a", "profile-b"),
    ] {
        assert_eq!(
            plan(&after, endpoint, domain)
                .allocate(p.full_tokens(), Instant::now())
                .unwrap()
                .0
                .read,
            0
        );
    }
}

#[tokio::test]
async fn adjacent_messages_keep_their_original_cache_boundaries() {
    let tracker = PromptCacheTracker::default();
    let make = |tail: &str| {
        claude_payload(json!({"model":MODEL,"max_tokens":64,"messages":[
            {"role":"user","content":stable(),"cache_control":{"type":"ephemeral","ttl":"1h"}},
            {"role":"user","content":tail}
        ]}))
    };
    let first = profile(&tracker, &make("uncached first"), CacheMode::Explicit).await;
    let write = run(&tracker, scope("a", None), &first);
    let next = profile(&tracker, &make("uncached changed"), CacheMode::Explicit).await;
    let read = run(&tracker, scope("a", None), &next);
    assert!(write.write_1h > 0);
    assert!(read.read > 0);
}

#[tokio::test]
#[ignore = "manual performance baseline; run alone with --nocapture --test-threads=1"]
async fn performance_profile_baseline() {
    let tokenizer = TokenCountCache::new(4096).unwrap();
    let mut scenarios = vec![(
        "short",
        claude("small system", json!([{"role":"user","content":"hello"}])),
    )];
    let tools = (0..40).map(|n| json!({"name":format!("tool_{n}"),"description":"tool docs ".repeat(120),"input_schema":{"type":"object","properties":{"path":{"type":"string"}}}})).collect::<Vec<_>>();
    scenarios.push(("tools", claude_payload(json!({"model":MODEL,"max_tokens":64,"system":[{"type":"text","text":stable(),"cache_control":{"type":"ephemeral"}}],"tools":tools,"messages":[{"role":"user","content":"hello"}]}))));
    let mut turns = Vec::new();
    for n in 0..100 {
        turns.push(
            json!({"role":"user","content":format!("question {n} {}", "context ".repeat(20))}),
        );
        turns.push(
            json!({"role":"assistant","content":format!("answer {n} {}", "reasoning ".repeat(20))}),
        );
    }
    turns.push(json!({"role":"user","content":"continue"}));
    scenarios.push(("100turn", claude(&stable(), Value::Array(turns))));
    use base64::Engine;
    let image = base64::engine::general_purpose::STANDARD.encode(vec![17u8; 256 * 1024]);
    scenarios.push(("media", claude(&stable(), json!([{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":image}},{"type":"text","text":"describe"}]}]))));
    let mut compacted = scenarios[2].1.clone();
    tokenizer
        .compact_kiro_payload(&mut compacted, 2000, 2)
        .await
        .unwrap();
    scenarios.push(("compaction", compacted));
    scenarios.push((
        "redaction",
        claude(
            &format!("{}\npassword=opaquePerformanceFixture123456", stable()),
            json!([{"role":"user","content":"continue"}]),
        ),
    ));
    drop(tokenizer);
    for (name, payload) in scenarios {
        if std::env::var("CROSSLANE_CACHE_PERF_CASE")
            .ok()
            .is_some_and(|case| case != name)
        {
            continue;
        }
        // Isolate each fixture's LRU state and alternate measurement order so
        // scheduler pressure cannot systematically penalize the second path.
        let tracker = PromptCacheTracker::default();
        let tokenizer = TokenCountCache::new(4096).unwrap();
        let salt = ProfileSalt::default();
        for _ in 0..10 {
            tokenizer.estimate_kiro_payload(&payload).await.unwrap();
            tracker
                .profile(&tokenizer, &payload, CacheMode::Explicit, &salt, false)
                .await;
        }
        let mut baseline = Vec::new();
        let mut profile_times = Vec::new();
        for iteration in 0..200 {
            let mut expected = 0;
            let mut measured = 0;
            for pass in 0..2 {
                let started = Instant::now();
                if (iteration + pass) % 2 == 0 {
                    expected = tokenizer.estimate_kiro_payload(&payload).await.unwrap() as u64;
                    baseline.push(started.elapsed().as_nanos());
                } else {
                    measured = tracker
                        .profile(&tokenizer, &payload, CacheMode::Explicit, &salt, false)
                        .await
                        .unwrap()
                        .full_tokens();
                    profile_times.push(started.elapsed().as_nanos());
                }
            }
            assert_eq!(measured, expected);
        }
        baseline.sort_unstable();
        profile_times.sort_unstable();
        let base = baseline[197];
        let measured = profile_times[197];
        println!(
            "PERF {name} baseline_p99_us={} profile_p99_us={} ratio={:.3}",
            base / 1000,
            measured / 1000,
            measured as f64 / base as f64
        );
    }
}
