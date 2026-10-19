//! One pre-response recovery for a real Claude Code summary's generic HTTP 500.
//! The first attempt keeps its existing timeout and dispatch behavior.

use std::time::Duration;

use crosslane_core::account::Account;
use crosslane_kiro::KiroError;
use crosslane_pool::{
    account_credit_state, AccountCreditState, AccountHealth, AccountLease, AccountPool, PoolError,
};
use tokio::time::Instant;

const GENERIC_FAILURE: &str =
    "Encountered an unexpected error when processing the request, please try again.";
const COOLING_POLL: Duration = Duration::from_millis(250);

/// Owned by the HTTP request, including a possible overflow replan. Only the
/// additional recovery observes this deadline; the original attempt is unbound.
pub(in crate::http) struct ClientSummaryRecovery {
    calls: u32,
    max_calls: u32,
    window: Duration,
    deadline: Option<Instant>,
    claimed: bool,
}

impl ClientSummaryRecovery {
    pub(in crate::http) fn new(max_retries: u32, stream_read_timeout_ms: u64) -> Self {
        Self {
            calls: 0,
            max_calls: max_retries.saturating_add(1),
            window: Duration::from_millis(stream_read_timeout_ms),
            deadline: None,
            claimed: false,
        }
    }

    pub(in crate::http) fn record_generation(&mut self) {
        if self.calls == 0 {
            self.deadline = Instant::now().checked_add(self.window);
        }
        self.calls = self.calls.saturating_add(1);
    }

    pub(in crate::http) fn claim(
        &mut self,
        error: &KiroError,
        live_max_retries: u32,
    ) -> Option<Instant> {
        let maximum = self.max_calls.min(live_max_retries.saturating_add(1));
        if self.claimed || self.calls >= maximum || !is_generic_summary_failure(error) {
            return None;
        }
        let deadline = self
            .deadline
            .filter(|deadline| *deadline > Instant::now())?;
        self.claimed = true;
        Some(deadline)
    }
}

fn is_generic_summary_failure(error: &KiroError) -> bool {
    if error.status != Some(500)
        || !matches!(
            error.endpoint.as_str(),
            "AmazonQ" | "CodeWhisperer" | "KiroRuntime"
        )
        || error.is_auth()
        || error.is_quota()
        || error.is_model_capacity_error()
        || error.is_request_rejection()
    {
        return false;
    }
    let text = error.message.trim();
    if text == GENERIC_FAILURE {
        return true;
    }
    let Ok(serde_json::Value::Object(body)) = serde_json::from_str(text) else {
        return false;
    };
    // Unknown/specialized reasons, including policy declines, must keep their
    // existing handling even if their message happens to use generic wording.
    // AmazonQ also emits reason:null when there is no specialized reason.
    body.iter().all(|(key, value)| {
        matches!(
            key.as_str(),
            "message" | "Message" | "__type" | "_type" | "type"
        ) || (key == "reason" && value.is_null())
    }) && ["message", "Message"]
        .iter()
        .filter_map(|field| body.get(*field))
        .any(|value| value.as_str() == Some(GENERIC_FAILURE))
        && ["message", "Message"].iter().all(|field| {
            body.get(*field)
                .is_none_or(|value| value.as_str() == Some(GENERIC_FAILURE))
        })
        && ["__type", "_type", "type"].iter().all(|field| {
            body.get(*field).is_none_or(|value| {
                value.as_str().and_then(|name| name.rsplit('#').next())
                    == Some("InternalServerException")
            })
        })
}

/// Pool acquire intentionally does not wait for a cooling account. Poll only
/// within the caller's one recovery deadline, and let normal pool eligibility
/// decide when the same account may obtain a fresh concurrency permit.
pub(in crate::http) async fn reacquire_summary_account(
    pool: &AccountPool,
    pool_config: &crosslane_core::config::PoolConfig,
    account_id: &str,
    model: &str,
    estimate: f64,
) -> Option<AccountLease> {
    let preferred = [account_id.to_owned()];
    loop {
        match pool.acquire(model, estimate, &preferred).await {
            Ok(lease) => return Some(lease),
            Err(PoolError::NoAvailableAccount(_)) => {
                let runtime = pool.get(account_id).await?;
                let account = runtime.account.read().await;
                if runtime.health() != AccountHealth::Cooling
                    || !account.enabled
                    || account_credit_state(&account, pool_config) != AccountCreditState::Available
                {
                    return None;
                }
                drop(account);
                tokio::time::sleep(COOLING_POLL).await;
            }
            Err(_) => return None,
        }
    }
}

/// A recovery never expands the original scope, and observes policy changes
/// before sending another request with a freshly acquired account lease.
#[allow(clippy::too_many_arguments)]
pub(in crate::http) fn summary_recovery_permitted(
    state: &crate::state::AppState,
    account_ids: &std::collections::HashSet<String>,
    account: &Account,
    requested_model: &str,
    model: &str,
    model_path: &[String],
    allowed_models: &[String],
    key_id: Option<&str>,
    service_id: Option<&str>,
) -> bool {
    let config = state.config.current();
    if !config
        .effective_providers()
        .iter()
        .any(|provider| provider.id == "kiro" && provider.kind == "kiro" && provider.enabled)
        || !account_ids.contains(&account.id)
        || !account.enabled
        || account_credit_state(account, &config.pool) != AccountCreditState::Available
        || std::iter::once(requested_model)
            .chain(std::iter::once(model))
            .chain(model_path.iter().map(String::as_str))
            .any(|model| config.is_model_disabled(model))
        || !super::super::handlers::model_is_allowed(allowed_models, "kiro", model)
    {
        return false;
    }
    if let Some(id) = key_id {
        let current_key = config.api_key.iter().find(|key| {
            key.enabled
                && state
                    .meter
                    .authenticate(Some(&key.key))
                    .ok()
                    .flatten()
                    .is_some_and(|key| key.id == id)
        });
        let Some(key) = current_key else {
            return false;
        };
        if !config.allowed_providers_for_key(key).contains(&"kiro")
            || !super::super::handlers::model_is_allowed(&key.allowed_models, "kiro", model)
        {
            return false;
        }
    } else if config.has_enabled_api_key() {
        return false;
    }
    if let Some(id) = service_id {
        let Some(service) = config.proxy_service.iter().find(|service| service.id == id) else {
            return false;
        };
        if !service.enabled
            || !service.includes_account(account)
            || !config
                .allowed_providers_for_service(service)
                .contains(&"kiro")
            || (!service.api_key_ids.is_empty()
                && key_id.is_none_or(|id| !service.api_key_ids.iter().any(|allowed| allowed == id)))
        {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn generic() -> KiroError {
        KiroError {
            status: Some(500),
            endpoint: "AmazonQ".into(),
            message: serde_json::json!({"message": GENERIC_FAILURE}).to_string(),
        }
    }

    #[test]
    fn live_generic_failure_with_null_reason_is_eligible() {
        let mut error = generic();
        error.message = serde_json::json!({"message":GENERIC_FAILURE,"reason":null}).to_string();
        assert_eq!(error.message.len(), 106);
        assert_eq!(
            format!("{:x}", Sha256::digest(error.message.as_bytes())),
            "621e325b3c909be32bfa66e3f00235ed0c8985702348b98b1a016dc1ed6e6f30"
        );
        assert!(is_generic_summary_failure(&error));
        for reason in [
            serde_json::json!(""),
            serde_json::json!(false),
            serde_json::json!(0),
            serde_json::json!([]),
            serde_json::json!({}),
        ] {
            error.message =
                serde_json::json!({"message":GENERIC_FAILURE,"reason":reason}).to_string();
            assert!(!is_generic_summary_failure(&error));
        }
        for field in ["code", "stopReason", "stopDetails", "unknown"] {
            error.message = serde_json::json!({"message":GENERIC_FAILURE,field:null}).to_string();
            assert!(!is_generic_summary_failure(&error));
        }
    }

    #[test]
    fn only_exact_generic_pre_response_failure_is_eligible() {
        assert!(is_generic_summary_failure(&generic()));
        let mut error = generic();
        error.message = GENERIC_FAILURE.into();
        assert!(is_generic_summary_failure(&error));
        error.message = serde_json::json!({"Message":GENERIC_FAILURE,
            "__type":"com.amazonaws.kiro#InternalServerException"})
        .to_string();
        assert!(is_generic_summary_failure(&error));
        for status in [
            None,
            Some(200),
            Some(400),
            Some(401),
            Some(403),
            Some(429),
            Some(502),
            Some(503),
        ] {
            error = generic();
            error.status = status;
            assert!(!is_generic_summary_failure(&error));
        }
        for field in ["reason", "code", "stopReason", "stopDetails"] {
            error = generic();
            error.message =
                serde_json::json!({"message":GENERIC_FAILURE, field:"CONTENT_FILTERED"})
                    .to_string();
            assert!(!is_generic_summary_failure(&error));
        }
        for message in ["Internal server error", "", "MODEL_TEMPORARILY_UNAVAILABLE"] {
            error = generic();
            error.message = message.into();
            assert!(!is_generic_summary_failure(&error));
        }
        error = generic();
        error.endpoint = "transport".into();
        assert!(!is_generic_summary_failure(&error));
    }

    #[test]
    fn one_request_budget_keeps_the_first_generation_deadline() {
        let mut budget = ClientSummaryRecovery::new(3, 1_000);
        budget.record_generation();
        let original_deadline = budget.deadline.unwrap();
        budget.record_generation();
        assert_eq!(budget.deadline, Some(original_deadline));
        assert_eq!(budget.claim(&generic(), 3), Some(original_deadline));
        assert_eq!(budget.claim(&generic(), 3), None);
        let mut exhausted = ClientSummaryRecovery::new(1, 1_000);
        exhausted.record_generation();
        exhausted.record_generation();
        assert_eq!(exhausted.claim(&generic(), 1), None);
    }

    #[test]
    fn zero_retries_or_expired_recovery_budget_never_claims() {
        let mut disabled = ClientSummaryRecovery::new(0, 1_000);
        disabled.record_generation();
        assert_eq!(disabled.claim(&generic(), 0), None);
        let mut budget = ClientSummaryRecovery::new(1, 1_000);
        budget.record_generation();
        budget.deadline = Some(Instant::now() - Duration::from_millis(1));
        assert_eq!(budget.claim(&generic(), 1), None);
        // No timeout was installed on the original attempt; expiry only makes
        // the optional recovery unavailable after that attempt returns.
        assert_eq!(budget.calls, 1);
    }

    fn recovery_pool() -> (AccountPool, crosslane_core::config::PoolConfig) {
        let account: Account = serde_json::from_value(serde_json::json!({
            "id":"summary-account", "email":"summary@example.invalid", "machine_id":"0".repeat(64),
            "credentials":{"access_token":"fixture", "region":"us-east-1", "expires_at":4_000_000_000i64,
                "auth_method":"idc"}, "created_at":1
        }))
        .unwrap();
        let mut config = crosslane_core::config::PoolConfig {
            enable_overage: false,
            max_concurrent_per_account: 1,
            ..Default::default()
        };
        config.cooldown.error_cooldown_ms = 10_000;
        (AccountPool::new(vec![account], config.clone()), config)
    }

    #[tokio::test]
    async fn recovery_cooldown_wait_is_bounded_and_cancel_safe() {
        let (pool, config) = recovery_pool();
        pool.record_error("summary-account").await;
        let runtime = pool.get("summary-account").await.unwrap();
        let deadline = Instant::now() + Duration::from_millis(20);
        assert!(tokio::time::timeout_at(
            deadline,
            reacquire_summary_account(&pool, &config, "summary-account", "", 1.0),
        )
        .await
        .is_err());
        assert_eq!(runtime.active(), 0);
        assert_eq!(runtime.health(), AccountHealth::Cooling);
        {
            let pending = reacquire_summary_account(&pool, &config, "summary-account", "", 1.0);
            tokio::pin!(pending);
            tokio::select! {
                _ = &mut pending => panic!("a cooling account must not be acquired"),
                _ = tokio::time::sleep(Duration::from_millis(5)) => {}
            }
            // Dropping the request's future must not retain a slot or spawn
            // detached work that later bypasses cooldown.
        }
        assert_eq!(runtime.active(), 0);
        assert_eq!(runtime.health(), AccountHealth::Cooling);
    }

    #[tokio::test]
    async fn recovery_rechecks_disabled_banned_and_exhausted_accounts() {
        for mode in ["disabled", "banned", "exhausted", "credit_exhausted"] {
            let (pool, config) = recovery_pool();
            pool.record_error("summary-account").await;
            let runtime = pool.get("summary-account").await.unwrap();
            match mode {
                "disabled" => runtime.account.write().await.enabled = false,
                "banned" => runtime.set_health(AccountHealth::Banned),
                "exhausted" => runtime.set_health(AccountHealth::Exhausted),
                "credit_exhausted" => runtime.account.write().await.credit_exhausted = true,
                _ => unreachable!(),
            }
            let result = tokio::time::timeout(
                Duration::from_millis(100),
                reacquire_summary_account(&pool, &config, "summary-account", "", 1.0),
            )
            .await
            .expect("ineligible accounts must stop rather than wait out the recovery budget");
            assert!(result.is_none(), "{mode}");
            assert_eq!(runtime.active(), 0);
        }
    }
}
