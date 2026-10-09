use std::cmp::Ordering as CmpOrdering;
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crosslane_core::account::{Account, Usage};
use crosslane_core::config::PoolConfig;
use indexmap::IndexMap;
use rand::Rng;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{Notify, RwLock};

use crate::state::{AccountHealth, AccountState};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PoolError {
    #[error("no account can serve model {0}")]
    NoAvailableAccount(String),
    #[error("account request queue is full")]
    QueueFull,
    #[error("timed out waiting for an account permit")]
    QueueTimeout,
    #[error("all matching accounts have exhausted their credit allowance")]
    CreditsExhausted,
}

/// 账号额度相对于当前池保护配置的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountCreditState {
    /// 额度充足，可以参与调度。
    Available,
    /// 尚有额度，但已触发低额度保护。
    Protected,
    /// 已被上游判定耗尽，或已使用额度达到总额度。
    Exhausted,
}

/// 账号池按实际调度条件汇总的互斥状态计数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AccountPoolCounts {
    pub total: usize,
    pub enabled: usize,
    pub available: usize,
    pub protected: usize,
    pub cooling: usize,
    pub exhausted: usize,
    pub banned: usize,
    pub refreshing: usize,
    pub disabled: usize,
}

/// 使用与调度器相同的规则判断账号额度状态。
pub fn account_credit_state(account: &Account, config: &PoolConfig) -> AccountCreditState {
    if config.enable_overage {
        if config.max_overage_credits_per_account.is_some()
            && account
                .usage
                .as_ref()
                .is_some_and(|usage| usage_credit_exhausted(usage, config))
        {
            return AccountCreditState::Exhausted;
        }
        return AccountCreditState::Available;
    }
    if account.credit_exhausted {
        return AccountCreditState::Exhausted;
    }
    let Some(usage) = &account.usage else {
        return AccountCreditState::Available;
    };
    let limit = usage.limit_without_overage();
    if limit <= 0.0 && usage.overage_cap.is_some() {
        return AccountCreditState::Exhausted;
    }
    if limit <= 0.0 {
        return AccountCreditState::Available;
    }
    let remaining = (limit - usage.current).max(0.0);
    if remaining <= 0.0 {
        return AccountCreditState::Exhausted;
    }
    if config.low_credit_min_remaining > 0.0 && remaining <= config.low_credit_min_remaining {
        AccountCreditState::Protected
    } else {
        AccountCreditState::Available
    }
}

/// 代理当前允许单个账号使用的总 credits，包括上游额度内的超额部分。
pub fn effective_credit_limit(usage: &Usage, config: &PoolConfig) -> f64 {
    let base = usage.limit_without_overage();
    if !config.enable_overage {
        return base;
    }
    match (usage.overage_cap, config.max_overage_credits_per_account) {
        (Some(upstream_cap), Some(local_cap)) => base + upstream_cap.min(local_cap),
        _ => usage.limit,
    }
}

/// 按代理当前允许的总额度计算剩余百分比。
pub fn remaining_credit_percent(usage: &Usage, config: &PoolConfig) -> Option<f64> {
    let limit = effective_credit_limit(usage, config);
    (limit > 0.0).then(|| ((limit - usage.current) / limit * 100.0).clamp(0.0, 100.0))
}

/// 根据当前额度策略更新持久化的耗尽标记。
pub fn usage_credit_exhausted(usage: &Usage, config: &PoolConfig) -> bool {
    let limit = effective_credit_limit(usage, config);
    (limit > 0.0 && usage.current >= limit) || (limit <= 0.0 && usage.overage_cap.is_some())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoreExplanation {
    pub account_id: String,
    pub score: f64,
    pub active_factor: f64,
    pub credit_factor: f64,
    pub idle_factor: f64,
    pub eligible: bool,
    pub reason: String,
}

#[derive(Clone)]
pub struct AccountPool {
    accounts: Arc<RwLock<IndexMap<String, Arc<AccountState>>>>,
    config: Arc<StdRwLock<PoolConfig>>,
    notify: Arc<Notify>,
    queued: Arc<AtomicUsize>,
}

pub struct AccountLease {
    state: Arc<AccountState>,
    account_id: String,
    notify: Arc<Notify>,
}

struct QueueGuard {
    queued: Arc<AtomicUsize>,
}

impl QueueGuard {
    fn enter(queued: Arc<AtomicUsize>) -> (Self, usize) {
        let position = queued.fetch_add(1, Ordering::AcqRel);
        (Self { queued }, position)
    }
}

impl Drop for QueueGuard {
    fn drop(&mut self) {
        self.queued.fetch_sub(1, Ordering::AcqRel);
    }
}

impl AccountLease {
    pub async fn account(&self) -> Account {
        self.state.account.read().await.clone()
    }

    pub fn account_id(&self) -> String {
        self.account_id.clone()
    }

    pub async fn settle_credits(&mut self, actual: f64) {
        if !actual.is_finite() || actual <= 0.0 {
            return;
        }
        // This optimistic increment only feeds account-selection scoring. The
        // service/API-key ledgers remain owned by crosslaned::meter, and the next
        // upstream status refresh replaces this estimate with authoritative
        // usage instead of accumulating it a second time.
        let mut account = self.state.account.write().await;
        let Some(usage) = account.usage.as_mut() else {
            return;
        };
        let optimistic = usage.current + actual;
        usage.current = if usage.limit > 0.0 {
            optimistic.min(usage.limit)
        } else {
            optimistic
        };
        usage.percent_used = if usage.limit > 0.0 {
            (usage.current / usage.limit * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        };
        usage.updated_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(usage.updated_at);
    }
}

impl Drop for AccountLease {
    fn drop(&mut self) {
        self.state.release_slot();
        self.state.mark_used_now();
        self.notify.notify_waiters();
    }
}

impl AccountPool {
    pub fn new(accounts: Vec<Account>, config: PoolConfig) -> Self {
        let states = accounts
            .into_iter()
            .map(|account| {
                let id = account.id.clone();
                let state = AccountState::new(account, config.max_concurrent_per_account);
                (id, state)
            })
            .collect();
        Self {
            accounts: Arc::new(RwLock::new(states)),
            config: Arc::new(StdRwLock::new(config)),
            notify: Arc::new(Notify::new()),
            queued: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub async fn acquire(
        &self,
        model: &str,
        estimated_credits: f64,
        prefer_ids: &[String],
    ) -> Result<AccountLease, PoolError> {
        if self.candidates(model, prefer_ids).await.is_empty() {
            return Err(PoolError::NoAvailableAccount(model.into()));
        }
        if let Some(lease) = self.try_acquire(model, estimated_credits, prefer_ids).await {
            return Ok(lease);
        }
        let config = self.config();
        let (_queue_guard, queue_position) = QueueGuard::enter(Arc::clone(&self.queued));
        let queue_disabled = config.max_queue_size == 0;
        let overflow = queue_disabled || queue_position >= config.max_queue_size;
        if overflow && config.queue_full_wait_ms == 0 {
            return Err(PoolError::QueueFull);
        }
        let wait = if queue_disabled {
            0
        } else if overflow {
            config.queue_full_wait_ms
        } else {
            config.max_queue_wait_ms
        };
        let deadline = Instant::now() + Duration::from_millis(wait);
        loop {
            if wait == 0 {
                return Err(if overflow {
                    PoolError::QueueFull
                } else {
                    PoolError::NoAvailableAccount(model.into())
                });
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(PoolError::QueueTimeout);
            }
            if tokio::time::timeout(deadline - now, self.notify.notified())
                .await
                .is_err()
            {
                return Err(PoolError::QueueTimeout);
            }
            if let Some(lease) = self.try_acquire(model, estimated_credits, prefer_ids).await {
                return Ok(lease);
            }
        }
    }

    /// Acquires an eligible account while guaranteeing that accounts already
    /// attempted by the caller cannot be selected again.
    pub async fn acquire_excluding(
        &self,
        model: &str,
        estimated_credits: f64,
        excluded_ids: &HashSet<String>,
    ) -> Result<AccountLease, PoolError> {
        let candidate_ids = self
            .snapshot()
            .await
            .into_iter()
            .filter(|account| !excluded_ids.contains(&account.id))
            .map(|account| account.id)
            .collect::<Vec<_>>();
        if candidate_ids.is_empty() {
            return Err(PoolError::NoAvailableAccount(model.into()));
        }
        self.acquire(model, estimated_credits, &candidate_ids).await
    }

    /// Acquires from one service's fixed account scope while excluding
    /// accounts already attempted by the current request.
    pub async fn acquire_scoped_excluding(
        &self,
        model: &str,
        estimated_credits: f64,
        allowed_ids: &HashSet<String>,
        excluded_ids: &HashSet<String>,
    ) -> Result<AccountLease, PoolError> {
        let candidate_ids = allowed_ids
            .iter()
            .filter(|account_id| !excluded_ids.contains(*account_id))
            .cloned()
            .collect::<Vec<_>>();
        if candidate_ids.is_empty() {
            return Err(PoolError::NoAvailableAccount(model.into()));
        }
        self.acquire(model, estimated_credits, &candidate_ids).await
    }

    async fn try_acquire(
        &self,
        model: &str,
        _estimated_credits: f64,
        prefer_ids: &[String],
    ) -> Option<AccountLease> {
        let mut candidates = self.candidates(model, prefer_ids).await;
        candidates
            .sort_by(|left, right| left.0.partial_cmp(&right.0).unwrap_or(CmpOrdering::Equal));
        let maximum = self.config().max_concurrent_per_account;
        for (_, state) in candidates {
            // Resolve identity before reserving a slot: cancellation while
            // waiting for the account lock must not leak active concurrency.
            let account_id = state.account.read().await.id.clone();
            if !state.try_acquire_slot(maximum) {
                continue;
            }
            state.mark_used_now();
            return Some(AccountLease {
                state,
                account_id,
                notify: Arc::clone(&self.notify),
            });
        }
        None
    }

    async fn candidates(
        &self,
        model: &str,
        prefer_ids: &[String],
    ) -> Vec<(f64, Arc<AccountState>)> {
        let accounts = self.accounts.read().await;
        let mut output = Vec::new();
        for state in accounts.values() {
            if !prefer_ids.is_empty() {
                let id = state.account.read().await.id.clone();
                if !prefer_ids.contains(&id) {
                    continue;
                }
            }
            if self.eligible(state, model).await {
                let jitter = rand::thread_rng().gen_range(0.0..f64::EPSILON * 1024.0);
                output.push((self.score(state).await.score + jitter, Arc::clone(state)));
            }
        }
        output
    }

    async fn eligible(&self, state: &Arc<AccountState>, model: &str) -> bool {
        if state.cooling_expired().await {
            state.set_health(AccountHealth::Available);
        }
        if state.health() != AccountHealth::Available {
            return false;
        }
        let account = state.account.read().await;
        if !account.enabled {
            return false;
        }
        if account_credit_state(&account, &self.config()) != AccountCreditState::Available {
            return false;
        }
        if model.is_empty() {
            return true;
        }
        if let Some(models) = state.supported_models.read().await.as_ref() {
            let models = models.iter().cloned().collect::<Vec<_>>();
            return crosslane_translate::model::can_resolve_dynamic_model(model, &models);
        }
        can_serve_subscription(&account, model)
    }

    async fn score(&self, state: &Arc<AccountState>) -> ScoreExplanation {
        let account = state.account.read().await;
        let config = self.config();
        let active_divisor = if config.max_concurrent_per_account == 0 {
            10.0
        } else {
            config.max_concurrent_per_account as f64
        };
        let active_factor = (state.active() as f64 / active_divisor).min(1.0);
        let credit_factor = account
            .usage
            .as_ref()
            .map(|usage| {
                let limit = effective_credit_limit(usage, &config);
                if limit > 0.0 {
                    (usage.current / limit).clamp(0.0, 1.0)
                } else {
                    0.0
                }
            })
            .unwrap_or(0.0);
        let idle_ms = now_ms().saturating_sub(state.last_used_ms());
        let idle_factor =
            1.0 - (idle_ms as f64 / config.balance.idle_window_ms.max(1) as f64).min(1.0);
        let score = config.balance.weight_active * active_factor
            + config.balance.weight_credit * credit_factor
            + config.balance.weight_idle * idle_factor;
        ScoreExplanation {
            account_id: account.id.clone(),
            score,
            active_factor,
            credit_factor,
            idle_factor,
            eligible: true,
            reason: "available".into(),
        }
    }

    pub async fn explain(&self, model: &str) -> Vec<ScoreExplanation> {
        let accounts = self.accounts.read().await;
        let mut explanations = Vec::new();
        for state in accounts.values() {
            if self.eligible(state, model).await {
                explanations.push(self.score(state).await);
            } else {
                let account = state.account.read().await;
                let health = state.health();
                let reason = if !account.enabled {
                    "disabled".into()
                } else if matches!(
                    health,
                    AccountHealth::Cooling | AccountHealth::Banned | AccountHealth::Refreshing
                ) {
                    format!("{health:?}").to_ascii_lowercase()
                } else {
                    match account_credit_state(&account, &self.config()) {
                        AccountCreditState::Exhausted => "exhausted".into(),
                        AccountCreditState::Protected => "low_credit".into(),
                        AccountCreditState::Available if health != AccountHealth::Available => {
                            format!("{health:?}").to_ascii_lowercase()
                        }
                        AccountCreditState::Available => "model_unavailable".into(),
                    }
                };
                explanations.push(ScoreExplanation {
                    account_id: account.id.clone(),
                    score: f64::INFINITY,
                    active_factor: 0.0,
                    credit_factor: 0.0,
                    idle_factor: 0.0,
                    eligible: false,
                    reason,
                });
            }
        }
        explanations.sort_by(|left, right| {
            left.score
                .partial_cmp(&right.score)
                .unwrap_or(CmpOrdering::Equal)
        });
        explanations
    }

    pub async fn record_success(&self, account_id: &str) {
        if let Some(state) = self.get(account_id).await {
            state.consecutive_errors.store(0, Ordering::Relaxed);
            if state.health() == AccountHealth::Cooling {
                state.set_health(AccountHealth::Available);
            }
        }
    }

    pub async fn record_error(&self, account_id: &str) {
        let Some(state) = self.get(account_id).await else {
            return;
        };
        let count = state.consecutive_errors.fetch_add(1, Ordering::Relaxed) + 1;
        let config = self.config();
        let duration = if count >= config.cooldown.max_error_count {
            Duration::from_millis(config.cooldown.cooldown_ms)
        } else {
            Duration::from_millis(config.cooldown.error_cooldown_ms)
        };
        state.cool_for(duration).await;
    }

    pub async fn record_quota_error(&self, account_id: &str) {
        let Some(state) = self.get(account_id).await else {
            return;
        };
        let now = Instant::now();
        let config = self.config();
        let window = Duration::from_millis(config.cooldown.quota_error_window_ms);
        let mut errors = state.quota_errors.lock().await;
        while errors
            .front()
            .is_some_and(|time| now.duration_since(*time) > window)
        {
            errors.pop_front();
        }
        errors.push_back(now);
        if errors.len() as u32 >= config.cooldown.quota_error_threshold {
            state.set_health(AccountHealth::Exhausted);
            state.account.write().await.credit_exhausted = true;
        } else {
            state
                .cool_for(Duration::from_millis(config.cooldown.error_cooldown_ms))
                .await;
        }
    }

    pub async fn mark_banned(&self, account_id: &str) {
        if let Some(state) = self.get(account_id).await {
            state.set_health(AccountHealth::Banned);
        }
    }

    pub async fn reset_health(&self, account_id: &str) -> bool {
        let Some(state) = self.get(account_id).await else {
            return false;
        };
        state.reset_health().await;
        self.notify.notify_waiters();
        true
    }

    pub async fn get(&self, account_id: &str) -> Option<Arc<AccountState>> {
        self.accounts.read().await.get(account_id).cloned()
    }

    pub async fn snapshot(&self) -> Vec<Account> {
        let states = self
            .accounts
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut output = Vec::with_capacity(states.len());
        for state in states {
            output.push(state.account.read().await.clone());
        }
        output
    }

    /// Returns the IDs of accounts matching a caller-provided scope without
    /// cloning credentials or the rest of each account record.
    pub async fn matching_account_ids(
        &self,
        matches: impl Fn(&Account) -> bool,
    ) -> HashSet<String> {
        let states = self
            .accounts
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut output = HashSet::with_capacity(states.len());
        for state in states {
            let account = state.account.read().await;
            if matches(&account) {
                output.insert(account.id.clone());
            }
        }
        output
    }

    /// Returns true only when at least one enabled account can serve the model
    /// and every such account has a persisted, usage-derived, or upstream quota stop.
    pub async fn all_matching_credit_exhausted(&self, model: &str) -> bool {
        self.all_matching_credit_exhausted_in(model, None).await
    }

    /// Scoped variant used by one proxy service's effective account pool.
    pub async fn all_matching_credit_exhausted_scoped(
        &self,
        model: &str,
        allowed_ids: &HashSet<String>,
    ) -> bool {
        self.all_matching_credit_exhausted_in(model, Some(allowed_ids))
            .await
    }

    async fn all_matching_credit_exhausted_in(
        &self,
        model: &str,
        allowed_ids: Option<&HashSet<String>>,
    ) -> bool {
        let states = self
            .accounts
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let config = self.config();
        let mut matched = false;
        for state in states {
            let account = state.account.read().await;
            if allowed_ids.is_some_and(|allowed| !allowed.contains(&account.id)) {
                continue;
            }
            if !account.enabled {
                continue;
            }
            let model_matches = if model.is_empty() {
                true
            } else if let Some(models) = state.supported_models.read().await.as_ref() {
                let models = models.iter().cloned().collect::<Vec<_>>();
                crosslane_translate::model::can_resolve_dynamic_model(model, &models)
            } else {
                can_serve_subscription(&account, model)
            };
            if !model_matches {
                continue;
            }
            matched = true;
            if state.health() != AccountHealth::Exhausted
                && account_credit_state(&account, &config) == AccountCreditState::Available
            {
                return false;
            }
        }
        matched
    }

    /// Returns true when there is at least one enabled account and every
    /// enabled account is fully exhausted. Low-credit protection does not
    /// count as exhaustion for operator alerts.
    pub async fn all_enabled_credit_exhausted(&self) -> bool {
        let states = self
            .accounts
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let config = self.config();
        let mut enabled = false;
        for state in states {
            let account = state.account.read().await;
            if !account.enabled {
                continue;
            }
            enabled = true;
            if state.health() != AccountHealth::Exhausted
                && account_credit_state(&account, &config) != AccountCreditState::Exhausted
            {
                return false;
            }
        }
        enabled
    }

    pub async fn replace_accounts(&self, accounts: Vec<Account>) {
        let mut next = IndexMap::new();
        let mut current = self.accounts.write().await;
        for account in accounts {
            let id = account.id.clone();
            if let Some(existing) = current.get(&id) {
                *existing.account.write().await = account;
                next.insert(id, Arc::clone(existing));
            } else {
                next.insert(
                    id,
                    AccountState::new(account, self.config().max_concurrent_per_account),
                );
            }
        }
        *current = next;
        self.notify.notify_waiters();
    }

    /// Count accounts using the same health and credit gates as scheduling.
    ///
    /// Credit protection is kept separate from true exhaustion.
    pub async fn scheduling_counts(&self) -> AccountPoolCounts {
        self.scheduling_counts_in(None).await
    }

    /// Count only accounts belonging to one proxy service's effective pool.
    pub async fn scheduling_counts_scoped(
        &self,
        allowed_ids: &HashSet<String>,
    ) -> AccountPoolCounts {
        self.scheduling_counts_in(Some(allowed_ids)).await
    }

    async fn scheduling_counts_in(
        &self,
        allowed_ids: Option<&HashSet<String>>,
    ) -> AccountPoolCounts {
        let states = self
            .accounts
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let config = self.config();
        let mut counts = AccountPoolCounts::default();
        for state in states {
            if state.cooling_expired().await {
                state.set_health(AccountHealth::Available);
            }
            let account = state.account.read().await;
            if allowed_ids.is_some_and(|allowed| !allowed.contains(&account.id)) {
                continue;
            }
            counts.total += 1;
            if !account.enabled {
                counts.disabled += 1;
                continue;
            }
            counts.enabled += 1;
            match account_credit_state(&account, &config) {
                AccountCreditState::Protected => counts.protected += 1,
                AccountCreditState::Exhausted => counts.exhausted += 1,
                AccountCreditState::Available => match state.health() {
                    AccountHealth::Available => counts.available += 1,
                    AccountHealth::Cooling => counts.cooling += 1,
                    AccountHealth::Exhausted => counts.exhausted += 1,
                    AccountHealth::Banned => counts.banned += 1,
                    AccountHealth::Refreshing => counts.refreshing += 1,
                },
            }
        }
        counts
    }

    pub fn queued(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }

    pub async fn active(&self) -> usize {
        self.accounts
            .read()
            .await
            .values()
            .map(|state| state.active())
            .sum()
    }

    pub async fn reset_daily_credits(&self) {
        self.notify.notify_waiters();
    }

    pub fn update_config(&self, config: PoolConfig) {
        match self.config.write() {
            Ok(mut current) => *current = config,
            Err(poisoned) => *poisoned.into_inner() = config,
        }
        self.notify.notify_waiters();
    }

    fn config(&self) -> PoolConfig {
        self.config
            .read()
            .map(|config| config.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }
}

fn can_serve_subscription(account: &Account, model: &str) -> bool {
    if account.credentials.auth_method == crosslane_core::account::AuthMethod::ApiKey {
        return true;
    }
    crosslane_kiro::static_subscription_can_serve(
        account
            .subscription
            .as_ref()
            .map(|subscription| subscription.kind),
        model,
    )
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use crosslane_core::account::{
        Account, AuthMethod, Credentials, Subscription, SubscriptionKind, Usage,
    };

    use super::*;

    fn account(id: &str, used: f64, subscription: SubscriptionKind) -> Account {
        Account {
            id: id.into(),
            email: format!("{id}@example.com"),
            label: None,
            enabled: true,
            machine_id: "a".repeat(64),
            profile_arn: None,
            upstream_user_id: None,
            credentials: Credentials {
                access_token: "at".into(),
                refresh_token: None,
                client_id: None,
                client_secret: None,
                region: "us-east-1".into(),
                expires_at: i64::MAX,
                auth_method: AuthMethod::Idc,
            },
            usage: Some(Usage {
                current: used,
                limit: 100.0,
                overage_cap: None,
                percent_used: used,
                next_reset_date: None,
                updated_at: 0,
            }),
            subscription: Some(Subscription {
                kind: subscription,
                title: None,
                raw_type: None,
                expires_at: None,
                days_remaining: None,
            }),
            tags: Vec::new(),
            created_at: 0,
            credit_exhausted: false,
        }
    }

    fn immediate_config() -> PoolConfig {
        PoolConfig {
            max_queue_size: 0,
            queue_full_wait_ms: 0,
            max_queue_wait_ms: 0,
            ..PoolConfig::default()
        }
    }

    #[tokio::test]
    async fn lease_account_id_is_stable_while_account_is_being_updated() {
        let pool = AccountPool::new(
            vec![account("stable", 0.0, SubscriptionKind::Pro)],
            immediate_config(),
        );
        let lease = pool.acquire("", 0.0, &[]).await.expect("account lease");
        let _write = lease.state.account.write().await;
        assert_eq!(lease.account_id(), "stable");
    }

    #[test]
    fn credit_state_distinguishes_protection_from_exhaustion() {
        let config = PoolConfig::default();
        assert_eq!(
            account_credit_state(&account("ready", 95.0, SubscriptionKind::Pro), &config),
            AccountCreditState::Available
        );
        assert_eq!(
            account_credit_state(&account("protected", 97.0, SubscriptionKind::Pro), &config),
            AccountCreditState::Protected
        );
        assert_eq!(
            account_credit_state(&account("exhausted", 100.0, SubscriptionKind::Pro), &config),
            AccountCreditState::Exhausted
        );
    }

    #[test]
    fn overage_switch_bypasses_local_credit_gates() {
        let mut account = account("overage", 95.0, SubscriptionKind::Enterprise);
        let usage = account.usage.as_mut().expect("usage");
        usage.limit = 200.0;
        usage.overage_cap = Some(100.0);
        let mut config = PoolConfig::default();

        assert_eq!(
            account_credit_state(&account, &config),
            AccountCreditState::Available
        );
        account.usage.as_mut().expect("usage").current = 97.0;
        assert_eq!(
            account_credit_state(&account, &config),
            AccountCreditState::Protected
        );
        account.usage.as_mut().expect("usage").current = 100.0;
        assert_eq!(
            account_credit_state(&account, &config),
            AccountCreditState::Exhausted
        );
        assert!(usage_credit_exhausted(
            account.usage.as_ref().expect("usage"),
            &config
        ));

        config.enable_overage = true;
        account.credit_exhausted = true;
        assert_eq!(
            account_credit_state(&account, &config),
            AccountCreditState::Available
        );
        assert!(!usage_credit_exhausted(
            account.usage.as_ref().expect("usage"),
            &config
        ));
        account.usage.as_mut().expect("usage").current = 200.0;
        assert_eq!(
            account_credit_state(&account, &config),
            AccountCreditState::Available
        );
        assert!(usage_credit_exhausted(
            account.usage.as_ref().expect("usage"),
            &config
        ));
    }

    #[test]
    fn configured_overage_cap_limits_each_account_without_low_credit_protection() {
        let mut account = account("overage", 10_000.0, SubscriptionKind::Enterprise);
        account.usage = Some(Usage {
            current: 10_000.0,
            limit: 20_000.0,
            overage_cap: Some(10_000.0),
            percent_used: 50.0,
            next_reset_date: None,
            updated_at: 0,
        });
        let mut config = PoolConfig {
            enable_overage: true,
            max_overage_credits_per_account: Some(500.0),
            ..PoolConfig::default()
        };
        let usage = account.usage.as_ref().expect("usage");
        assert_eq!(effective_credit_limit(usage, &config), 10_500.0);
        assert!(
            (remaining_credit_percent(usage, &config).expect("remaining")
                - 500.0 / 10_500.0 * 100.0)
                .abs()
                < f64::EPSILON
        );
        assert_eq!(
            account_credit_state(&account, &config),
            AccountCreditState::Available
        );

        account.usage.as_mut().expect("usage").current = 10_499.0;
        account.credit_exhausted = true;
        assert_eq!(
            account_credit_state(&account, &config),
            AccountCreditState::Available
        );
        account.usage.as_mut().expect("usage").current = 10_500.0;
        assert_eq!(
            account_credit_state(&account, &config),
            AccountCreditState::Exhausted
        );
        assert!(usage_credit_exhausted(
            account.usage.as_ref().expect("usage"),
            &config
        ));

        config.max_overage_credits_per_account = Some(20_000.0);
        assert_eq!(
            effective_credit_limit(account.usage.as_ref().expect("usage"), &config),
            20_000.0
        );
        assert_eq!(
            account_credit_state(&account, &config),
            AccountCreditState::Available
        );
        config.max_overage_credits_per_account = Some(0.0);
        assert_eq!(
            effective_credit_limit(account.usage.as_ref().expect("usage"), &config),
            10_000.0
        );
        assert_eq!(
            account_credit_state(&account, &config),
            AccountCreditState::Exhausted
        );
    }

    #[tokio::test]
    async fn configured_overage_cap_is_applied_to_each_account_in_the_pool() {
        let mut available = account("available", 10_499.0, SubscriptionKind::Enterprise);
        let mut exhausted = account("exhausted", 10_500.0, SubscriptionKind::Enterprise);
        for account in [&mut available, &mut exhausted] {
            let usage = account.usage.as_mut().expect("usage");
            usage.limit = 20_000.0;
            usage.overage_cap = Some(10_000.0);
        }
        let pool = AccountPool::new(
            vec![available, exhausted],
            PoolConfig {
                enable_overage: true,
                max_overage_credits_per_account: Some(500.0),
                ..immediate_config()
            },
        );
        let explanation = pool.explain("claude-sonnet").await;
        assert_eq!(
            explanation
                .iter()
                .filter(|account| account.eligible)
                .count(),
            1
        );
        assert_eq!(
            pool.acquire("claude-sonnet", 0.0, &[])
                .await
                .expect("available account")
                .account()
                .await
                .id,
            "available"
        );
    }

    #[tokio::test]
    async fn overage_switch_schedules_persisted_exhausted_account() {
        let mut exhausted = account("overage", 100.0, SubscriptionKind::Enterprise);
        exhausted.credit_exhausted = true;
        let pool = AccountPool::new(vec![exhausted], immediate_config());
        assert!(matches!(
            pool.acquire("claude-sonnet", 0.0, &[]).await,
            Err(PoolError::NoAvailableAccount(_))
        ));

        pool.update_config(PoolConfig {
            enable_overage: true,
            ..immediate_config()
        });
        assert!(pool.acquire("claude-sonnet", 0.0, &[]).await.is_ok());
        assert!(!pool.all_matching_credit_exhausted("claude-sonnet").await);

        pool.get("overage")
            .await
            .expect("account")
            .set_health(AccountHealth::Exhausted);
        assert!(matches!(
            pool.acquire("claude-sonnet", 0.0, &[]).await,
            Err(PoolError::NoAvailableAccount(_))
        ));
        assert!(pool.all_matching_credit_exhausted("claude-sonnet").await);
        assert!(pool.all_enabled_credit_exhausted().await);
    }

    #[test]
    fn credit_protection_uses_absolute_remaining_credits_only() {
        let config = PoolConfig::default();
        let mut account = account("low-percent", 0.0, SubscriptionKind::Pro);
        account.usage = Some(Usage {
            current: 995.0,
            limit: 1_000.0,
            overage_cap: None,
            percent_used: 99.5,
            next_reset_date: None,
            updated_at: 0,
        });

        assert_eq!(
            account_credit_state(&account, &config),
            AccountCreditState::Available
        );
    }

    #[tokio::test]
    async fn credit_protection_does_not_poison_runtime_health() {
        let protected = account("protected", 97.0, SubscriptionKind::Pro);
        let pool = AccountPool::new(vec![protected], immediate_config());
        let runtime = pool.get("protected").await.expect("protected account");

        assert!(matches!(
            pool.acquire("claude-sonnet", 0.0, &[]).await,
            Err(PoolError::NoAvailableAccount(_))
        ));
        assert_eq!(runtime.health(), AccountHealth::Available);

        pool.update_config(PoolConfig {
            low_credit_min_remaining: 2.0,
            ..immediate_config()
        });
        assert!(pool.acquire("claude-sonnet", 0.0, &[]).await.is_ok());
    }

    #[tokio::test]
    async fn scheduling_counts_apply_credit_protection_and_runtime_health() {
        let ready = account("ready", 10.0, SubscriptionKind::Pro);
        let protected = account("protected", 97.0, SubscriptionKind::Pro);
        let exhausted = account("exhausted", 100.0, SubscriptionKind::Pro);
        let cooling = account("cooling", 10.0, SubscriptionKind::Pro);
        let banned = account("banned", 10.0, SubscriptionKind::Pro);
        let refreshing = account("refreshing", 10.0, SubscriptionKind::Pro);
        let mut disabled = account("disabled", 10.0, SubscriptionKind::Pro);
        disabled.enabled = false;
        let pool = AccountPool::new(
            vec![
                ready, protected, exhausted, cooling, banned, refreshing, disabled,
            ],
            immediate_config(),
        );
        pool.get("cooling")
            .await
            .expect("cooling")
            .set_health(AccountHealth::Cooling);
        pool.get("banned")
            .await
            .expect("banned")
            .set_health(AccountHealth::Banned);
        pool.get("refreshing")
            .await
            .expect("refreshing")
            .set_health(AccountHealth::Refreshing);

        let counts = pool.scheduling_counts().await;
        assert_eq!(counts.total, 7);
        assert_eq!(counts.enabled, 6);
        assert_eq!(counts.available, 1);
        assert_eq!(counts.protected, 1);
        assert_eq!(counts.cooling, 1);
        assert_eq!(counts.exhausted, 1);
        assert_eq!(counts.banned, 1);
        assert_eq!(counts.refreshing, 1);
        assert_eq!(counts.disabled, 1);
    }

    #[tokio::test]
    async fn service_exhaustion_does_not_treat_low_credit_protection_as_exhausted() {
        let protected = account("protected", 97.0, SubscriptionKind::Pro);
        let pool = AccountPool::new(vec![protected], immediate_config());
        assert!(!pool.all_enabled_credit_exhausted().await);

        pool.get("protected")
            .await
            .expect("account")
            .account
            .write()
            .await
            .credit_exhausted = true;
        assert!(pool.all_enabled_credit_exhausted().await);
    }

    #[tokio::test]
    async fn credit_weight_prefers_the_less_used_account() {
        let pool = AccountPool::new(
            vec![
                account("high", 90.0, SubscriptionKind::Pro),
                account("low", 10.0, SubscriptionKind::Pro),
            ],
            immediate_config(),
        );
        let lease = pool
            .acquire("claude-sonnet", 0.0, &[])
            .await
            .expect("lease");
        assert_eq!(lease.account().await.id, "low");
    }

    #[tokio::test]
    async fn free_account_is_skipped_for_opus() {
        let pool = AccountPool::new(
            vec![
                account("free", 0.0, SubscriptionKind::Free),
                account("pro", 80.0, SubscriptionKind::Pro),
            ],
            immediate_config(),
        );
        let explanations = pool.explain("claude-opus-4").await;
        let free = explanations
            .iter()
            .find(|explanation| explanation.account_id == "free")
            .expect("free account explanation");
        assert!(!free.eligible);
        assert_eq!(free.reason, "model_unavailable");
        let lease = pool
            .acquire("claude-opus-4", 0.0, &[])
            .await
            .expect("lease");
        assert_eq!(lease.account().await.id, "pro");
    }

    #[tokio::test]
    async fn free_account_is_skipped_for_non_opus_premium_model() {
        let pool = AccountPool::new(
            vec![
                account("free", 0.0, SubscriptionKind::Free),
                account("pro", 80.0, SubscriptionKind::Pro),
            ],
            immediate_config(),
        );
        let lease = pool
            .acquire("claude-sonnet-4.6", 0.0, &[])
            .await
            .expect("lease");
        assert_eq!(lease.account().await.id, "pro");
    }

    #[tokio::test]
    async fn settlement_optimistically_updates_account_usage_and_score() {
        let pool = AccountPool::new(
            vec![account("only", 10.0, SubscriptionKind::Pro)],
            immediate_config(),
        );
        let before = pool.explain("claude-sonnet-4.6").await.remove(0);
        let mut lease = pool
            .acquire("claude-sonnet-4.6", 5.5, &[])
            .await
            .expect("lease");
        lease.settle_credits(5.5).await;
        let account = lease.account().await;
        let usage = account.usage.expect("usage");
        assert_eq!(usage.current, 15.5);
        assert_eq!(usage.percent_used, 15.5);
        drop(lease);
        let after = pool.explain("claude-sonnet-4.6").await.remove(0);
        assert!(after.credit_factor > before.credit_factor);
    }

    #[tokio::test]
    async fn exclusions_prevent_retrying_the_same_failed_account() {
        let pool = AccountPool::new(
            vec![
                account("preferred", 0.0, SubscriptionKind::Pro),
                account("fallback", 50.0, SubscriptionKind::Pro),
            ],
            immediate_config(),
        );
        let excluded = HashSet::from(["preferred".to_string()]);
        let lease = pool
            .acquire_excluding("claude-sonnet", 0.0, &excluded)
            .await
            .expect("fallback lease");
        assert_eq!(lease.account().await.id, "fallback");
        let all = HashSet::from(["preferred".to_string(), "fallback".to_string()]);
        assert!(matches!(
            pool.acquire_excluding("claude-sonnet", 0.0, &all).await,
            Err(PoolError::NoAvailableAccount(_))
        ));
    }

    #[tokio::test]
    async fn scoped_acquire_never_uses_an_account_outside_the_service_pool() {
        let pool = AccountPool::new(
            vec![
                account("service-a", 80.0, SubscriptionKind::Pro),
                account("service-b", 0.0, SubscriptionKind::Pro),
            ],
            immediate_config(),
        );
        let allowed = HashSet::from(["service-a".to_string()]);
        let lease = pool
            .acquire_scoped_excluding("claude-sonnet", 0.0, &allowed, &HashSet::new())
            .await
            .expect("scoped lease");
        assert_eq!(lease.account().await.id, "service-a");
        drop(lease);

        let counts = pool.scheduling_counts_scoped(&allowed).await;
        assert_eq!(counts.total, 1);
        assert_eq!(counts.available, 1);
    }

    #[tokio::test]
    async fn scoped_credit_exhaustion_ignores_healthy_accounts_in_other_services() {
        let pool = AccountPool::new(
            vec![
                account("exhausted", 100.0, SubscriptionKind::Pro),
                account("healthy", 0.0, SubscriptionKind::Pro),
            ],
            immediate_config(),
        );
        let allowed = HashSet::from(["exhausted".to_string()]);
        assert!(
            pool.all_matching_credit_exhausted_scoped("claude-sonnet", &allowed)
                .await
        );
        assert!(!pool.all_matching_credit_exhausted("claude-sonnet").await);
    }

    #[tokio::test]
    async fn dropping_a_lease_releases_permit_and_credit_reservation() {
        let mut config = immediate_config();
        config.max_concurrent_per_account = 1;
        config.daily_credit_limit = 1.0;
        let pool = AccountPool::new(vec![account("only", 0.0, SubscriptionKind::Pro)], config);
        let first = pool
            .acquire("claude-sonnet", 0.8, &[])
            .await
            .expect("first");
        assert!(matches!(
            pool.acquire("claude-sonnet", 0.1, &[]).await,
            Err(PoolError::QueueFull)
        ));
        drop(first);
        tokio::task::yield_now().await;
        let next = pool.acquire("claude-sonnet", 0.3, &[]).await.expect("next");
        assert_eq!(next.account().await.id, "only");
    }

    #[tokio::test]
    async fn cancelling_a_waiter_releases_its_queue_slot() {
        let config = PoolConfig {
            max_concurrent_per_account: 1,
            max_queue_size: 1,
            max_queue_wait_ms: 2_000,
            ..PoolConfig::default()
        };
        let pool = AccountPool::new(vec![account("only", 0.0, SubscriptionKind::Pro)], config);
        let held = pool.acquire("claude-sonnet", 0.0, &[]).await.expect("held");
        let waiting_pool = pool.clone();
        let waiter =
            tokio::spawn(async move { waiting_pool.acquire("claude-sonnet", 0.0, &[]).await });

        tokio::time::timeout(Duration::from_secs(1), async {
            while pool.queued() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("waiter entered queue");
        waiter.abort();
        let _cancelled = waiter.await;

        assert_eq!(pool.queued(), 0);
        drop(held);
    }

    #[tokio::test]
    async fn releasing_one_account_wakes_a_compatible_waiter() {
        let config = PoolConfig {
            max_concurrent_per_account: 1,
            max_queue_size: 4,
            max_queue_wait_ms: 2_000,
            ..PoolConfig::default()
        };
        let pool = AccountPool::new(
            vec![
                account("a", 0.0, SubscriptionKind::Pro),
                account("b", 0.0, SubscriptionKind::Pro),
            ],
            config,
        );
        pool.get("a")
            .await
            .expect("a")
            .set_supported_models(["model-a".to_string()])
            .await;
        pool.get("b")
            .await
            .expect("b")
            .set_supported_models(["model-b".to_string()])
            .await;
        let held = pool.acquire("model-a", 0.0, &[]).await.expect("held");
        let waiting_pool = pool.clone();
        let waiter = tokio::spawn(async move { waiting_pool.acquire("model-a", 0.0, &[]).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(held);
        let lease = waiter.await.expect("join").expect("woken");
        assert_eq!(lease.account().await.id, "a");
    }

    #[tokio::test]
    async fn service_daily_credit_limit_is_not_multiplied_inside_account_pool() {
        let config = PoolConfig {
            daily_credit_limit: 1.0,
            max_queue_size: 0,
            queue_full_wait_ms: 0,
            ..PoolConfig::default()
        };
        let pool = AccountPool::new(vec![account("only", 0.0, SubscriptionKind::Pro)], config);
        assert!(pool.acquire("claude-sonnet", 1.1, &[]).await.is_ok());
    }

    #[tokio::test]
    async fn client_model_alias_is_matched_against_discovered_kiro_models() {
        let pool = AccountPool::new(
            vec![account("alias", 0.0, SubscriptionKind::Pro)],
            immediate_config(),
        );
        pool.get("alias")
            .await
            .expect("account")
            .set_supported_models(["claude-sonnet-4.6".into()])
            .await;
        let lease = pool
            .acquire("claude-4.6-sonnet", 0.0, &[])
            .await
            .expect("client alias should resolve through the discovered catalog");
        assert_eq!(lease.account().await.id, "alias");
    }

    #[tokio::test]
    async fn reports_exhaustion_only_when_every_matching_account_is_out_of_credit() {
        let mut exhausted = account("out", 100.0, SubscriptionKind::Pro);
        exhausted.credit_exhausted = true;
        let pool = AccountPool::new(
            vec![exhausted, account("ready", 10.0, SubscriptionKind::Pro)],
            immediate_config(),
        );
        for id in ["out", "ready"] {
            pool.get(id)
                .await
                .expect("account")
                .set_supported_models(["claude-sonnet-4-20250514".into()])
                .await;
        }
        assert!(!pool.all_matching_credit_exhausted("claude-sonnet-4").await);
        pool.get("ready")
            .await
            .expect("ready")
            .account
            .write()
            .await
            .credit_exhausted = true;
        assert!(pool.all_matching_credit_exhausted("claude-sonnet-4").await);
        assert!(!pool.all_matching_credit_exhausted("claude-opus-4").await);
    }
}
