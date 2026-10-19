//! Select the existing empty retry's route without changing its request.

use crosslane_core::account::Account;
use crosslane_kiro::{EndpointKey, KiroClient};

#[derive(Clone, Copy)]
pub(in crate::http) struct EmptyRetryConditions {
    pub claude_protocol: bool,
    pub semantic_started: bool,
    pub output_tokens: u64,
    pub already_retried: bool,
}

impl EmptyRetryConditions {
    fn permits_isolation(self) -> bool {
        self.claude_protocol
            && !self.semantic_started
            && self.output_tokens == 0
            && !self.already_retried
    }
}

/// Keep the selected client with the plan so a config reload cannot change
/// endpoint policy between eligibility and dispatch. The client rechecks its
/// read-only endpoint availability before the one isolated send.
pub(in crate::http) fn isolated_empty_retry_client(
    client: KiroClient,
    account: &Account,
    frozen_payload: &crosslane_translate::KiroPayload,
    previous_endpoint: EndpointKey,
    conditions: EmptyRetryConditions,
) -> Option<KiroClient> {
    (conditions.permits_isolation()
        && client.runtime_isolated_retry_eligible(account, frozen_payload, previous_endpoint))
    .then_some(client)
}

#[cfg(test)]
mod tests {
    use super::EmptyRetryConditions;

    #[test]
    fn only_an_unspent_pre_semantic_zero_output_claude_attempt_is_isolated() {
        let allowed = EmptyRetryConditions {
            claude_protocol: true,
            semantic_started: false,
            output_tokens: 0,
            already_retried: false,
        };
        assert!(allowed.permits_isolation());
        for rejected in [
            EmptyRetryConditions {
                claude_protocol: false,
                ..allowed
            },
            EmptyRetryConditions {
                semantic_started: true,
                ..allowed
            },
            EmptyRetryConditions {
                output_tokens: 1,
                ..allowed
            },
            EmptyRetryConditions {
                already_retried: true,
                ..allowed
            },
        ] {
            assert!(!rejected.permits_isolation());
        }
    }
}
