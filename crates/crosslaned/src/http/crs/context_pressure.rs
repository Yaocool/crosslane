//! Upstream occupancy calibrates admission, never billing or reported tokens.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crosslane_translate::KiroPayload;
use sha2::{Digest, Sha256};

const TTL: Duration = Duration::from_secs(30 * 60);
const MAX_ENTRIES: usize = 512;

struct Observation {
    window: u32,
    ratio: f64,
    expires: Instant,
    touched: Instant,
}

#[derive(Default)]
pub(crate) struct ContextPressure {
    entries: Mutex<HashMap<[u8; 32], Observation>>,
}

fn scope(account: &str, payload: &KiroPayload) -> Option<[u8; 32]> {
    let intent = payload.model_request_intent.as_ref()?;
    if !intent.crs_native_messages && !intent.crs_native_summary {
        return None;
    }
    let state = &payload.conversation_state;
    if account.is_empty() || state.conversation_id.is_empty() {
        return None;
    }
    // A client or proxy checkpoint changes this source boundary. Do not carry
    // a large-log calibration into the replacement summary's new context.
    let first = state
        .history
        .iter()
        .skip(payload.protected_history_len())
        .find_map(|message| message.user_input_message.as_ref())
        .unwrap_or(&state.current_message.user_input_message);
    let model =
        crosslane_core::model_policy::model_key(&state.current_message.user_input_message.model_id);
    let mut digest = Sha256::new();
    for value in [
        account,
        model.as_str(),
        state.conversation_id.as_str(),
        first.content.as_str(),
    ] {
        digest.update((value.len() as u64).to_le_bytes());
        digest.update(value.as_bytes());
    }
    Some(digest.finalize().into())
}

impl ContextPressure {
    pub(crate) fn observe(
        &self,
        account: &str,
        payload: &KiroPayload,
        window: u32,
        estimated: u64,
        percentage: f64,
    ) {
        // Tiny samples are dominated by framing/output. Invalid or implausible
        // telemetry must not create a permanent small window for a session.
        if window == 0
            || estimated < 4096
            || !percentage.is_finite()
            || !(10.0..=100.0).contains(&percentage)
        {
            return;
        }
        let ratio = percentage / 100.0 * f64::from(window) / estimated as f64;
        if !ratio.is_finite() || !(1.0..=8.0).contains(&ratio) {
            return;
        }
        let Some(key) = scope(account, payload) else {
            return;
        };
        let now = Instant::now();
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        entries.retain(|_, entry| entry.expires > now);
        let ratio = entries
            .get(&key)
            .filter(|entry| entry.window == window)
            .map_or(ratio, |entry| entry.ratio.max(ratio));
        entries.insert(
            key,
            Observation {
                window,
                ratio,
                expires: now + TTL,
                touched: now,
            },
        );
        while entries.len() > MAX_ENTRIES {
            let oldest = entries
                .iter()
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| *key);
            if let Some(oldest) = oldest {
                entries.remove(&oldest);
            }
        }
    }

    pub(crate) fn ratio(&self, account: &str, payload: &KiroPayload, window: u32) -> f64 {
        let Some(key) = scope(account, payload) else {
            return 1.0;
        };
        let now = Instant::now();
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        entries
            .get_mut(&key)
            .filter(|entry| entry.expires > now && entry.window == window)
            .map_or(1.0, |entry| {
                entry.touched = now;
                entry.ratio
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosslane_translate::ModelRequestIntent;
    use serde_json::json;

    fn payload() -> KiroPayload {
        let mut payload: KiroPayload = serde_json::from_value(json!({
            "conversationState":{"conversationId":"session", "chatTriggerType":"MANUAL",
                "history":[{"userInputMessage":{"content":"original source", "modelId":"claude-opus-5.5","origin":"AI_EDITOR"}}],
                "currentMessage":{"userInputMessage":{"content":"continue", "modelId":"claude-opus-5.5","origin":"AI_EDITOR"}}}
        })).unwrap();
        payload.model_request_intent = Some(ModelRequestIntent {
            crs_native_messages: true,
            ..Default::default()
        });
        payload
    }

    #[test]
    fn observed_pressure_is_account_model_session_and_checkpoint_scoped() {
        let tracker = ContextPressure::default();
        let mut input = payload();
        tracker.observe("account", &input, 1_000_000, 400_000, 80.0);
        assert_eq!(tracker.ratio("account", &input, 1_000_000), 2.0);
        assert_eq!(tracker.ratio("other", &input, 1_000_000), 1.0);
        assert_eq!(tracker.ratio("account", &input, 200_000), 1.0);
        input
            .conversation_state
            .current_message
            .user_input_message
            .model_id = "claude-sonnet-5".into();
        assert_eq!(tracker.ratio("account", &input, 1_000_000), 1.0);
        input = payload();
        input.conversation_state.conversation_id = "other-session".into();
        assert_eq!(tracker.ratio("account", &input, 1_000_000), 1.0);
        input = payload();
        input.conversation_state.history[0]
            .user_input_message
            .as_mut()
            .unwrap()
            .content = "complete replacement checkpoint".into();
        assert_eq!(tracker.ratio("account", &input, 1_000_000), 1.0);
    }

    #[test]
    fn telemetry_cannot_expand_windows_or_poison_unrelated_protocols() {
        for percentage in [f64::NAN, f64::INFINITY, -1.0, 0.0, 1.0, 101.0] {
            let tracker = ContextPressure::default();
            let input = payload();
            tracker.observe("account", &input, 1_000_000, 400_000, percentage);
            assert_eq!(tracker.ratio("account", &input, 1_000_000), 1.0);
        }
        let tracker = ContextPressure::default();
        let mut input = payload();
        input.model_request_intent = None;
        tracker.observe("account", &input, 1_000_000, 400_000, 80.0);
        assert_eq!(tracker.ratio("account", &input, 1_000_000), 1.0);
    }
}
