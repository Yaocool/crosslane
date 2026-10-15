//! Endpoint ordering and bounded per-account success/failure cache.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crosslane_core::account::{Account, AuthMethod};
use crosslane_core::config::Endpoint;

pub const CODEWHISPERER_URL: &str =
    "https://codewhisperer.us-east-1.amazonaws.com/generateAssistantResponse";
pub const AMAZONQ_URL: &str = "https://q.us-east-1.amazonaws.com/generateAssistantResponse";
const MAX_ENDPOINT_CACHE_SIZE: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EndpointKey {
    Codewhisperer,
    Amazonq,
    Runtime,
}

impl From<Endpoint> for EndpointKey {
    fn from(value: Endpoint) -> Self {
        match value {
            Endpoint::Codewhisperer => Self::Codewhisperer,
            Endpoint::Amazonq => Self::Amazonq,
            Endpoint::Runtime => Self::Runtime,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EndpointPurpose {
    Generation,
    Models,
}

#[derive(Debug, Clone)]
pub struct EndpointDefinition {
    pub key: EndpointKey,
    pub url: String,
    pub origin: &'static str,
    pub amz_target: &'static str,
    pub name: &'static str,
}

impl EndpointDefinition {
    pub fn for_key(key: EndpointKey, overrides: &EndpointOverrides) -> Self {
        match key {
            EndpointKey::Codewhisperer => Self {
                key,
                url: overrides
                    .codewhisperer_url
                    .clone()
                    .unwrap_or_else(|| CODEWHISPERER_URL.into()),
                origin: "AI_EDITOR",
                amz_target: "AmazonCodeWhispererStreamingService.GenerateAssistantResponse",
                name: "CodeWhisperer",
            },
            EndpointKey::Amazonq => Self {
                key,
                url: overrides
                    .amazonq_url
                    .clone()
                    .unwrap_or_else(|| AMAZONQ_URL.into()),
                // The q.* runtime accepts Kiro IDE traffic for both Builder ID and
                // Identity Center accounts. Advertising this request as Amazon Q
                // CLI (and sending its RPC target) makes enterprise profiles fail
                // authorization even when the bearer token itself is valid.
                origin: "AI_EDITOR",
                amz_target: "",
                name: "AmazonQ",
            },
            EndpointKey::Runtime => Self {
                key,
                url: overrides
                    .runtime_url
                    .clone()
                    .unwrap_or_else(|| "https://runtime.us-east-1.kiro.dev/".into()),
                origin: "KIRO_CLI",
                amz_target: "AmazonCodeWhispererStreamingService.GenerateAssistantResponse",
                name: "KiroRuntime",
            },
        }
    }

    pub fn for_account(
        key: EndpointKey,
        overrides: &EndpointOverrides,
        account: &Account,
    ) -> Result<Self, crate::KiroError> {
        let region = account_api_region(account)?;
        let mut endpoint = Self::for_key(key, overrides);
        endpoint.url = match key {
            EndpointKey::Codewhisperer => {
                overrides.codewhisperer_url.clone().unwrap_or_else(|| {
                    format!(
                        "https://codewhisperer.{region}.amazonaws.com/generateAssistantResponse"
                    )
                })
            }
            EndpointKey::Amazonq => overrides.amazonq_url.clone().unwrap_or_else(|| {
                format!("https://q.{region}.amazonaws.com/generateAssistantResponse")
            }),
            EndpointKey::Runtime => overrides
                .runtime_url
                .clone()
                .unwrap_or_else(|| format!("https://runtime.{region}.kiro.dev/")),
        }
        .replace("{region}", &region);
        Ok(endpoint)
    }
}

pub fn account_api_region(account: &Account) -> Result<String, crate::KiroError> {
    let credential_region = account.credentials.region.trim();
    if credential_region.is_empty()
        || !credential_region
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(crate::KiroError {
            status: Some(400),
            endpoint: "region".into(),
            message: "account has an invalid API region".into(),
        });
    }
    let profile_region = account.profile_arn.as_deref().and_then(|arn| {
        let parts = arn.split(':').collect::<Vec<_>>();
        (parts.len() >= 6
            && parts[0] == "arn"
            && matches!(parts[1], "aws" | "aws-us-gov")
            && parts[2] == "codewhisperer")
            .then(|| parts[3])
    });
    // Never route a GovCloud account into the commercial partition because
    // of an imported Builder ID placeholder ARN.
    let region = if credential_region.starts_with("us-gov-")
        || account.credentials.auth_method == AuthMethod::ApiKey
    {
        credential_region
    } else {
        // An IdC login region is not necessarily a Kiro service region.
        profile_region.unwrap_or(if credential_region.starts_with("eu-") {
            "eu-central-1"
        } else {
            "us-east-1"
        })
    };
    if region.is_empty()
        || !region
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(crate::KiroError {
            status: Some(400),
            endpoint: "region".into(),
            message: "account has an invalid API region".into(),
        });
    }
    Ok(region.into())
}

/// IDE 1.2.56 routes both extension REST and agent control-plane calls here.
/// Keep the legacy host only for regions absent from its endpoint table.
pub fn management_base(region: &str) -> String {
    if supports_native_management(region) {
        format!("https://management.{region}.kiro.dev/")
    } else {
        format!("https://q.{region}.amazonaws.com/")
    }
}

pub(crate) fn supports_native_management(region: &str) -> bool {
    matches!(
        region,
        "us-east-1" | "eu-central-1" | "us-gov-east-1" | "us-gov-west-1"
    )
}

#[derive(Debug, Clone, Default)]
pub struct EndpointOverrides {
    pub codewhisperer_url: Option<String>,
    pub amazonq_url: Option<String>,
    pub mcp_url: Option<String>,
    pub runtime_url: Option<String>,
    pub management_url: Option<String>,
}

type PurposeKey = (String, EndpointPurpose);

#[derive(Debug, Default)]
struct EndpointCacheState {
    preferred: HashMap<PurposeKey, EndpointKey>,
    preferred_order: VecDeque<PurposeKey>,
    disabled: HashMap<PurposeKey, HashMap<EndpointKey, Instant>>,
    disabled_order: VecDeque<PurposeKey>,
    revisions: HashMap<String, u64>,
    revision_order: VecDeque<String>,
}

#[derive(Debug)]
pub struct EndpointCache {
    state: Mutex<EndpointCacheState>,
    disabled_ttl: Duration,
}

impl EndpointCache {
    pub fn new(disabled_ttl: Duration) -> Self {
        Self {
            state: Mutex::new(EndpointCacheState::default()),
            disabled_ttl,
        }
    }

    pub fn order(
        &self,
        account: &Account,
        explicit: Option<EndpointKey>,
        purpose: EndpointPurpose,
    ) -> Vec<EndpointKey> {
        let inferred = endpoint_for_auth(account.credentials.auth_method);
        let fallback = other(inferred);
        let mut state = self.lock_state();
        let cache_key = (account.id.clone(), purpose);
        let cached = state.preferred.get(&cache_key).copied();
        let mut order = Vec::with_capacity(2);
        if account.credentials.auth_method == AuthMethod::ApiKey
            || account_api_region(account).is_ok_and(|region| region.starts_with("us-gov-"))
        {
            // API keys use the CLI runtime. GovCloud credentials must never
            // fall back to a commercial endpoint.
            if !Self::is_disabled(&mut state, &cache_key, EndpointKey::Runtime) {
                order.push(EndpointKey::Runtime);
            }
            return order;
        }
        for candidate in [
            explicit,
            explicit.is_none().then_some(cached).flatten(),
            Some(inferred),
            Some(fallback),
        ]
        .into_iter()
        .flatten()
        {
            if !order.contains(&candidate) && !Self::is_disabled(&mut state, &cache_key, candidate)
            {
                order.push(candidate);
            }
        }
        order
    }

    pub fn mark_success(&self, account_id: &str, purpose: EndpointPurpose, endpoint: EndpointKey) {
        let mut state = self.lock_state();
        Self::mark_success_locked(&mut state, account_id, purpose, endpoint);
    }

    pub fn mark_success_if_revision(
        &self,
        account_id: &str,
        purpose: EndpointPurpose,
        endpoint: EndpointKey,
        expected_revision: u64,
    ) -> bool {
        let mut state = self.lock_state();
        if Self::revision_locked(&state, account_id) != expected_revision {
            return false;
        }
        Self::mark_success_locked(&mut state, account_id, purpose, endpoint);
        true
    }

    pub fn preferred(&self, account_id: &str, purpose: EndpointPurpose) -> Option<EndpointKey> {
        self.lock_state()
            .preferred
            .get(&(account_id.into(), purpose))
            .copied()
    }

    pub fn mark_disabled(&self, account_id: &str, purpose: EndpointPurpose, endpoint: EndpointKey) {
        let mut state = self.lock_state();
        self.mark_disabled_locked(&mut state, account_id, purpose, endpoint);
    }

    pub fn mark_disabled_if_revision(
        &self,
        account_id: &str,
        purpose: EndpointPurpose,
        endpoint: EndpointKey,
        expected_revision: u64,
    ) -> bool {
        let mut state = self.lock_state();
        if Self::revision_locked(&state, account_id) != expected_revision {
            return false;
        }
        self.mark_disabled_locked(&mut state, account_id, purpose, endpoint);
        true
    }

    pub fn revision(&self, account_id: &str) -> u64 {
        Self::revision_locked(&self.lock_state(), account_id)
    }

    /// Clears temporary 403 rejections after credentials change while retaining
    /// the last successful endpoint preference.
    pub fn clear_failures(&self, account_id: &str) {
        let mut state = self.lock_state();
        Self::bump_revision(&mut state, account_id);
        Self::remove_failures(&mut state, account_id);
    }

    /// Clears all state when an account is removed or replaced.
    pub fn clear_account(&self, account_id: &str) {
        let mut state = self.lock_state();
        Self::bump_revision(&mut state, account_id);
        state.preferred.retain(|(id, _), _| id != account_id);
        state
            .preferred_order
            .retain(|(id, _)| id.as_str() != account_id);
        Self::remove_failures(&mut state, account_id);
    }

    fn lock_state(&self) -> MutexGuard<'_, EndpointCacheState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn mark_success_locked(
        state: &mut EndpointCacheState,
        account_id: &str,
        purpose: EndpointPurpose,
        endpoint: EndpointKey,
    ) {
        let cache_key = (account_id.to_owned(), purpose);
        Self::clear_endpoint_failure(state, &cache_key, endpoint);
        if !state.preferred.contains_key(&cache_key) {
            Self::evict_oldest_preferred_if_full(state);
        }
        state.preferred_order.retain(|key| key != &cache_key);
        state.preferred_order.push_back(cache_key.clone());
        state.preferred.insert(cache_key, endpoint);
    }

    fn mark_disabled_locked(
        &self,
        state: &mut EndpointCacheState,
        account_id: &str,
        purpose: EndpointPurpose,
        endpoint: EndpointKey,
    ) {
        let cache_key = (account_id.to_owned(), purpose);
        if !state.disabled.contains_key(&cache_key) {
            Self::evict_oldest_disabled_if_full(state);
        }
        state.disabled_order.retain(|key| key != &cache_key);
        state.disabled_order.push_back(cache_key.clone());
        state
            .disabled
            .entry(cache_key.clone())
            .or_default()
            .insert(endpoint, Instant::now() + self.disabled_ttl);
        if state.preferred.get(&cache_key) == Some(&endpoint) {
            state.preferred.remove(&cache_key);
            state.preferred_order.retain(|key| key != &cache_key);
        }
    }

    fn is_disabled(
        state: &mut EndpointCacheState,
        cache_key: &PurposeKey,
        endpoint: EndpointKey,
    ) -> bool {
        let Some(until) = state
            .disabled
            .get(cache_key)
            .and_then(|failures| failures.get(&endpoint))
            .copied()
        else {
            return false;
        };
        if until > Instant::now() {
            return true;
        }
        Self::clear_endpoint_failure(state, cache_key, endpoint);
        false
    }

    fn clear_endpoint_failure(
        state: &mut EndpointCacheState,
        cache_key: &PurposeKey,
        endpoint: EndpointKey,
    ) {
        let remove_state = if let Some(failures) = state.disabled.get_mut(cache_key) {
            failures.remove(&endpoint);
            failures.is_empty()
        } else {
            false
        };
        if remove_state {
            state.disabled.remove(cache_key);
            state.disabled_order.retain(|key| key != cache_key);
        }
    }

    fn remove_failures(state: &mut EndpointCacheState, account_id: &str) {
        state.disabled.retain(|(id, _), _| id != account_id);
        state
            .disabled_order
            .retain(|(id, _)| id.as_str() != account_id);
    }

    fn revision_locked(state: &EndpointCacheState, account_id: &str) -> u64 {
        state.revisions.get(account_id).copied().unwrap_or(0)
    }

    fn bump_revision(state: &mut EndpointCacheState, account_id: &str) {
        if !state.revisions.contains_key(account_id) {
            if state.revisions.len() >= MAX_ENDPOINT_CACHE_SIZE {
                if let Some(oldest) = state.revision_order.pop_front() {
                    state.revisions.remove(&oldest);
                }
            }
            state.revision_order.push_back(account_id.to_owned());
        }
        let revision = state.revisions.entry(account_id.to_owned()).or_default();
        *revision = revision.wrapping_add(1);
    }

    fn evict_oldest_preferred_if_full(state: &mut EndpointCacheState) {
        if state.preferred.len() >= MAX_ENDPOINT_CACHE_SIZE {
            if let Some(oldest) = state.preferred_order.pop_front() {
                state.preferred.remove(&oldest);
            }
        }
    }

    fn evict_oldest_disabled_if_full(state: &mut EndpointCacheState) {
        if state.disabled.len() >= MAX_ENDPOINT_CACHE_SIZE {
            if let Some(oldest) = state.disabled_order.pop_front() {
                state.disabled.remove(&oldest);
            }
        }
    }
}

impl Default for EndpointCache {
    fn default() -> Self {
        Self::new(Duration::from_secs(600))
    }
}

pub fn endpoint_for_auth(method: AuthMethod) -> EndpointKey {
    match method {
        AuthMethod::Idc => EndpointKey::Amazonq,
        AuthMethod::Social => EndpointKey::Codewhisperer,
        AuthMethod::ApiKey => EndpointKey::Runtime,
    }
}

pub fn other(endpoint: EndpointKey) -> EndpointKey {
    match endpoint {
        EndpointKey::Codewhisperer => EndpointKey::Amazonq,
        EndpointKey::Amazonq => EndpointKey::Codewhisperer,
        EndpointKey::Runtime => EndpointKey::Amazonq,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosslane_core::account::{Credentials, Subscription, SubscriptionKind, Usage};

    fn account(id: &str, method: AuthMethod) -> Account {
        Account {
            id: id.into(),
            email: "a@example.com".into(),
            label: None,
            enabled: true,
            machine_id: "0".repeat(64),
            profile_arn: None,
            upstream_user_id: None,
            credentials: Credentials {
                access_token: "token".into(),
                refresh_token: None,
                client_id: None,
                client_secret: None,
                region: "us-east-1".into(),
                expires_at: i64::MAX,
                auth_method: method,
            },
            usage: Some(Usage {
                current: 0.0,
                limit: 100.0,
                overage_cap: None,
                percent_used: 0.0,
                next_reset_date: None,
                updated_at: 0,
            }),
            subscription: Some(Subscription {
                kind: SubscriptionKind::Pro,
                title: None,
                raw_type: None,
                expires_at: None,
                days_remaining: None,
            }),
            tags: vec![],
            created_at: 0,
            credit_exhausted: false,
        }
    }

    #[test]
    fn explicit_then_cache_then_auth_inference() {
        let cache = EndpointCache::default();
        let account = account("acc_1", AuthMethod::Idc);
        assert_eq!(
            cache.order(&account, None, EndpointPurpose::Generation),
            vec![EndpointKey::Amazonq, EndpointKey::Codewhisperer]
        );
        cache.mark_success(
            &account.id,
            EndpointPurpose::Generation,
            EndpointKey::Codewhisperer,
        );
        assert_eq!(
            cache.order(&account, None, EndpointPurpose::Generation)[0],
            EndpointKey::Codewhisperer
        );
        assert_eq!(
            cache.order(
                &account,
                Some(EndpointKey::Amazonq),
                EndpointPurpose::Generation
            )[0],
            EndpointKey::Amazonq
        );
    }

    #[test]
    fn token_refresh_clears_failures_but_preserves_preference_and_rejects_stale_writes() {
        let cache = EndpointCache::default();
        let account = account("acc_1", AuthMethod::Idc);
        cache.mark_success(
            &account.id,
            EndpointPurpose::Models,
            EndpointKey::Codewhisperer,
        );
        cache.mark_disabled(
            &account.id,
            EndpointPurpose::Generation,
            EndpointKey::Amazonq,
        );
        let revision = cache.revision(&account.id);

        cache.clear_failures(&account.id);

        assert_eq!(
            cache.preferred(&account.id, EndpointPurpose::Models),
            Some(EndpointKey::Codewhisperer)
        );
        assert_eq!(
            cache.order(&account, None, EndpointPurpose::Generation),
            vec![EndpointKey::Amazonq, EndpointKey::Codewhisperer]
        );
        assert!(!cache.mark_disabled_if_revision(
            &account.id,
            EndpointPurpose::Models,
            EndpointKey::Amazonq,
            revision,
        ));
    }

    #[test]
    fn generation_and_model_failures_are_isolated() {
        let cache = EndpointCache::default();
        let account = account("acc_1", AuthMethod::Idc);
        cache.mark_disabled(
            &account.id,
            EndpointPurpose::Generation,
            EndpointKey::Amazonq,
        );
        assert_eq!(
            cache.order(&account, None, EndpointPurpose::Generation),
            vec![EndpointKey::Codewhisperer]
        );
        assert_eq!(
            cache.order(&account, None, EndpointPurpose::Models),
            vec![EndpointKey::Amazonq, EndpointKey::Codewhisperer]
        );
    }

    #[test]
    fn amazon_q_uses_the_kiro_ide_protocol() {
        let endpoint =
            EndpointDefinition::for_key(EndpointKey::Amazonq, &EndpointOverrides::default());

        assert_eq!(endpoint.origin, "AI_EDITOR");
        assert!(endpoint.amz_target.is_empty());
    }

    #[test]
    fn preferred_cache_is_bounded_and_evicts_oldest_entry() {
        let cache = EndpointCache::default();
        for index in 0..=MAX_ENDPOINT_CACHE_SIZE {
            cache.mark_success(
                &format!("acc_{index}"),
                EndpointPurpose::Generation,
                EndpointKey::Amazonq,
            );
        }
        assert_eq!(cache.preferred("acc_0", EndpointPurpose::Generation), None);
        assert_eq!(
            cache.preferred("acc_500", EndpointPurpose::Generation),
            Some(EndpointKey::Amazonq)
        );
    }
}
