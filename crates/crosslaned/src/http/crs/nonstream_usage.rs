//! Completed non-streaming rounds keep their reported usage on a later error.
//! No prompt/output estimates or partially collected event streams enter this ledger.

use crosslane_kiro::{KiroEvent, UsageInfo};

use super::super::response::DecodedResponse;

#[derive(Default)]
pub(in crate::http) struct ConfirmedNonstreamUsage {
    usage: UsageInfo,
    endpoint: String,
}

impl ConfirmedNonstreamUsage {
    pub(in crate::http) fn observe_completed_round(
        &mut self,
        events: &[KiroEvent],
        endpoint: &str,
    ) {
        self.endpoint = endpoint.to_owned();
        let mut reported = DecodedResponse::default();
        for event in events {
            if matches!(
                event,
                KiroEvent::Usage { .. } | KiroEvent::MessageMetadata { .. }
            ) {
                // Reuse the normal cumulative-per-round parser; repeated
                // metadata frames must not be charged as separate rounds.
                let _ = reported.push(event.clone());
            }
        }
        let usage = reported.usage;
        self.usage.input_tokens = self.usage.input_tokens.saturating_add(usage.input_tokens);
        self.usage.output_tokens = self.usage.output_tokens.saturating_add(usage.output_tokens);
        self.usage.cache_read_tokens = self
            .usage
            .cache_read_tokens
            .saturating_add(usage.cache_read_tokens);
        self.usage.cache_write_tokens = self
            .usage
            .cache_write_tokens
            .saturating_add(usage.cache_write_tokens);
        self.usage.reasoning_tokens = self
            .usage
            .reasoning_tokens
            .saturating_add(usage.reasoning_tokens);
        if usage.credits.is_finite() && usage.credits > 0.0 {
            self.usage.credits += usage.credits;
        }
    }

    pub(in crate::http) fn last_endpoint(&self) -> &str {
        &self.endpoint
    }

    pub(in crate::http) fn into_reported(self) -> Option<UsageInfo> {
        (self.usage.input_tokens > 0
            || self.usage.output_tokens > 0
            || self.usage.credits > 0.0
            || self.usage.cache_read_tokens > 0
            || self.usage.cache_write_tokens > 0
            || self.usage.reasoning_tokens > 0)
            .then_some(self.usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn repeated_metadata_is_one_round_and_only_reported_fields_are_retained() {
        let event = KiroEvent::Usage {
            usage: json!({"inputTokens":10,"outputTokens":0,
            "creditsConsumed":0.125,"cacheReadInputTokens":3}),
        };
        let mut ledger = ConfirmedNonstreamUsage::default();
        ledger.observe_completed_round(&[event.clone(), event], "AmazonQ");
        ledger.observe_completed_round(
            &[KiroEvent::MessageMetadata {
                usage: json!({"inputTokens":20,"outputTokens":2,"creditsConsumed":0.25}),
            }],
            "KiroRuntime",
        );
        assert_eq!(ledger.last_endpoint(), "KiroRuntime");
        let usage = ledger.into_reported().unwrap();
        assert_eq!(usage.input_tokens, 30);
        assert_eq!(usage.output_tokens, 2);
        assert_eq!(usage.cache_read_tokens, 3);
        assert_eq!(usage.credits, 0.375);
    }

    #[test]
    fn context_percent_and_content_do_not_fabricate_reported_usage() {
        let mut ledger = ConfirmedNonstreamUsage::default();
        ledger.observe_completed_round(
            &[
                KiroEvent::Other {
                    event_type: "contextUsageEvent".into(),
                    payload: json!({"contextUsagePercentage":75.0}),
                },
                KiroEvent::AssistantResponse {
                    content: "creditsConsumed=999 inputTokens=1000".into(),
                },
            ],
            "AmazonQ",
        );
        assert!(ledger.into_reported().is_none());
    }
}
