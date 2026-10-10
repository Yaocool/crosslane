//! Service-scoped provider scheduling and circuit recovery.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const COOLDOWN: Duration = Duration::from_secs(5);
const MAX_COOLDOWN: Duration = Duration::from_secs(60);
const MAX_ROUTES: usize = 4_096;

#[derive(Clone)]
pub(crate) struct ProviderLoad {
    pub id: String,
    pub capacity: usize,
    pub active: usize,
}

#[derive(Default)]
struct Health {
    active: usize,
    failures: u32,
    epoch: u64,
    until: Option<Instant>,
}

#[derive(Default)]
struct Schedule {
    cursor: usize,
    providers: HashMap<String, Arc<Mutex<Health>>>,
}

#[derive(Default)]
pub(crate) struct ProviderScheduler {
    routes: Mutex<HashMap<(String, String), Schedule>>,
}

pub(crate) struct ProviderLease {
    health: Arc<Mutex<Health>>,
    epoch: u64,
}

impl ProviderScheduler {
    /// Reserve before dispatch so simultaneous requests cannot all choose the
    /// same idle provider. Existing account load also includes pinned traffic.
    pub fn acquire(
        &self,
        service: &str,
        model: &str,
        candidates: &[ProviderLoad],
    ) -> Option<(usize, ProviderLease)> {
        self.acquire_at(service, model, candidates, Instant::now())
    }

    fn acquire_at(
        &self,
        service: &str,
        model: &str,
        candidates: &[ProviderLoad],
        now: Instant,
    ) -> Option<(usize, ProviderLease)> {
        if candidates.is_empty() {
            return None;
        }
        let mut routes = lock(&self.routes);
        let key = (service.to_owned(), model.to_owned());
        if routes.len() >= MAX_ROUTES && !routes.contains_key(&key) {
            routes.retain(|_, schedule| {
                schedule.providers.values().any(|health| {
                    let health = lock(health);
                    health.active > 0 || health.until.is_some_and(|until| until > now)
                })
            });
            if routes.len() >= MAX_ROUTES {
                return None;
            }
        }
        let schedule = routes.entry(key).or_default();
        let mut selected = None;
        for offset in 0..candidates.len() {
            let index = (schedule.cursor + offset) % candidates.len();
            let candidate = &candidates[index];
            let health = schedule.providers.entry(candidate.id.clone()).or_default();
            let health = lock(health);
            if health.until.is_some_and(|until| until > now)
                || (health.failures > 0 && health.active > 0)
            {
                continue;
            }
            let pressure =
                health.active.max(candidate.active) as f64 / candidate.capacity.max(1) as f64;
            if selected.is_none_or(|(_, best)| pressure < best) {
                selected = Some((index, pressure));
            }
        }
        let (index, _) = selected?;
        schedule.cursor = (index + 1) % candidates.len();
        let health = Arc::clone(&schedule.providers[&candidates[index].id]);
        let epoch = {
            let mut health = lock(&health);
            health.active += 1;
            health.epoch
        };
        Some((index, ProviderLease { health, epoch }))
    }
}

impl ProviderLease {
    pub fn failure(&self) {
        self.failure_at(Instant::now());
    }

    fn failure_at(&self, now: Instant) {
        let mut health = lock(&self.health);
        health.failures = health.failures.saturating_add(1);
        health.epoch = health.epoch.wrapping_add(1);
        let delay = COOLDOWN
            .saturating_mul(1u32 << health.failures.saturating_sub(1).min(4))
            .min(MAX_COOLDOWN);
        health.until = Some(now + delay);
    }

    pub fn success(&self) {
        let mut health = lock(&self.health);
        // An older in-flight success must not reopen a newly failed circuit.
        if health.epoch == self.epoch {
            health.failures = 0;
            health.until = None;
        }
    }
}

impl Drop for ProviderLease {
    fn drop(&mut self) {
        lock(&self.health).active -= 1;
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates() -> Vec<ProviderLoad> {
        ["kiro", "copilot"]
            .map(|id| ProviderLoad {
                id: id.into(),
                capacity: 10,
                active: 0,
            })
            .to_vec()
    }

    #[test]
    fn sequential_requests_balance_and_active_requests_avoid_the_busy_side() {
        let scheduler = ProviderScheduler::default();
        let candidates = candidates();
        for expected in [0, 1, 0, 1] {
            let (index, lease) = scheduler.acquire("service", "model", &candidates).unwrap();
            assert_eq!(index, expected);
            lease.success();
        }
        let (_, held) = scheduler.acquire("service", "model", &candidates).unwrap();
        let (index, _) = scheduler.acquire("service", "model", &candidates).unwrap();
        assert_eq!(index, 1);
        drop(held);
    }

    #[test]
    fn capacity_and_pinned_account_load_affect_selection() {
        let scheduler = ProviderScheduler::default();
        let mut candidates = candidates();
        candidates[0].active = 9;
        candidates[1].active = 5;
        assert_eq!(
            scheduler
                .acquire("service", "model", &candidates)
                .unwrap()
                .0,
            1
        );
        candidates[0].capacity = 100;
        assert_eq!(
            scheduler
                .acquire("service", "model", &candidates)
                .unwrap()
                .0,
            0
        );
    }

    #[test]
    fn failed_provider_cools_then_allows_only_one_recovery_probe() {
        let scheduler = ProviderScheduler::default();
        let candidates = candidates();
        let now = Instant::now();
        let (_, failed) = scheduler
            .acquire_at("service", "model", &candidates, now)
            .unwrap();
        failed.failure_at(now);
        drop(failed);
        let (index, _) = scheduler
            .acquire_at("service", "model", &candidates, now)
            .unwrap();
        assert_eq!(index, 1);
        let (_, probe) = scheduler
            .acquire_at("service", "model", &candidates[..1], now + COOLDOWN)
            .unwrap();
        assert!(scheduler
            .acquire_at("service", "model", &candidates[..1], now + COOLDOWN)
            .is_none());
        probe.success();
        drop(probe);
        assert!(scheduler
            .acquire_at("service", "model", &candidates[..1], now + COOLDOWN)
            .is_some());
    }

    #[test]
    fn health_is_scoped_and_older_successes_do_not_clear_a_failure() {
        let scheduler = ProviderScheduler::default();
        let candidates = candidates();
        let now = Instant::now();
        let (_, old) = scheduler
            .acquire_at("service", "model", &candidates[..1], now)
            .unwrap();
        let (_, failed) = scheduler
            .acquire_at("service", "model", &candidates[..1], now)
            .unwrap();
        failed.failure_at(now);
        old.success();
        drop((old, failed));
        assert!(scheduler
            .acquire_at("service", "model", &candidates[..1], now)
            .is_none());
        assert!(scheduler
            .acquire_at("another-service", "model", &candidates[..1], now)
            .is_some());
        assert!(scheduler
            .acquire_at("service", "another-model", &candidates[..1], now)
            .is_some());
    }
}
