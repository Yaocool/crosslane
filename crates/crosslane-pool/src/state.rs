use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crosslane_core::account::Account;
use tokio::sync::{Mutex, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AccountHealth {
    Available = 0,
    Cooling = 1,
    Exhausted = 2,
    Banned = 3,
    Refreshing = 4,
}

impl AccountHealth {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Cooling,
            2 => Self::Exhausted,
            3 => Self::Banned,
            4 => Self::Refreshing,
            _ => Self::Available,
        }
    }
}

pub struct AccountState {
    pub account: RwLock<Account>,
    pub(crate) active: AtomicUsize,
    health: AtomicU8,
    pub(crate) refresh_lock: Mutex<()>,
    pub(crate) last_used_ms: AtomicU64,
    pub(crate) consecutive_errors: AtomicU32,
    pub(crate) cooling_until: Mutex<Option<Instant>>,
    pub(crate) quota_errors: Mutex<VecDeque<Instant>>,
    pub(crate) supported_models: RwLock<Option<HashSet<String>>>,
}

impl AccountState {
    pub fn new(account: Account, _max_concurrent: usize) -> Arc<Self> {
        Arc::new(Self {
            account: RwLock::new(account),
            active: AtomicUsize::new(0),
            health: AtomicU8::new(AccountHealth::Available as u8),
            refresh_lock: Mutex::new(()),
            last_used_ms: AtomicU64::new(0),
            consecutive_errors: AtomicU32::new(0),
            cooling_until: Mutex::new(None),
            quota_errors: Mutex::new(VecDeque::new()),
            supported_models: RwLock::new(None),
        })
    }

    pub fn health(&self) -> AccountHealth {
        AccountHealth::from_u8(self.health.load(Ordering::Acquire))
    }

    pub fn set_health(&self, health: AccountHealth) {
        self.health.store(health as u8, Ordering::Release);
    }

    pub(crate) fn restore_health_after_refresh(&self, previous: AccountHealth) {
        let restored = match previous {
            AccountHealth::Cooling | AccountHealth::Exhausted => previous,
            AccountHealth::Available | AccountHealth::Banned | AccountHealth::Refreshing => {
                AccountHealth::Available
            }
        };
        // A request that was already in flight can update health while the token
        // refresh is running. Preserve that newer state instead of overwriting it.
        let _result = self.health.compare_exchange(
            AccountHealth::Refreshing as u8,
            restored as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    pub fn active(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    pub(crate) fn try_acquire_slot(&self, maximum: usize) -> bool {
        let maximum = if maximum == 0 { usize::MAX } else { maximum };
        self.active
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |current| {
                (current < maximum).then_some(current + 1)
            })
            .is_ok()
    }

    pub(crate) fn release_slot(&self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }

    pub fn last_used_ms(&self) -> u64 {
        self.last_used_ms.load(Ordering::Relaxed)
    }

    pub(crate) fn mark_used_now(&self) {
        self.last_used_ms.store(now_ms(), Ordering::Relaxed);
    }

    pub async fn reset_health(&self) {
        self.set_health(AccountHealth::Available);
        self.consecutive_errors.store(0, Ordering::Relaxed);
        *self.cooling_until.lock().await = None;
        self.quota_errors.lock().await.clear();
        self.account.write().await.credit_exhausted = false;
    }

    pub async fn set_supported_models(&self, models: impl IntoIterator<Item = String>) {
        *self.supported_models.write().await = Some(models.into_iter().collect());
    }

    /// Resolve a client model against this account's latest discovery cache.
    /// `None` means either the cache is not populated or no compatible model exists.
    pub async fn resolve_model(&self, model: &str) -> Option<String> {
        let models = self.supported_models.read().await;
        let models = models.as_ref()?.iter().cloned().collect::<Vec<_>>();
        crosslane_translate::model::resolve_dynamic_model(model, &models)
    }

    pub async fn has_model_cache(&self) -> bool {
        self.supported_models.read().await.is_some()
    }

    pub async fn supported_models(&self) -> Vec<String> {
        let mut models: Vec<String> = self
            .supported_models
            .read()
            .await
            .as_ref()
            .map(|models| models.iter().cloned().collect())
            .unwrap_or_default();
        models.sort();
        models
    }

    pub(crate) async fn cooling_expired(&self) -> bool {
        if self.health() != AccountHealth::Cooling {
            return false;
        }
        self.cooling_until
            .lock()
            .await
            .is_some_and(|until| until <= Instant::now())
    }

    pub(crate) async fn cool_for(&self, duration: Duration) {
        self.set_health(AccountHealth::Cooling);
        *self.cooling_until.lock().await = Some(Instant::now() + duration);
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
