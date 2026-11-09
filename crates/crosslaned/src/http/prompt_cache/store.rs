//! Bounded in-memory entry store. A scope isolates tenants; each fingerprint
//! keeps separate 5m and 1h expiry slots. The lock only guards snapshots,
//! pruning of one scope, updates and eviction; no hashing or tokenizing.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use super::allocate::Ttl;

pub(crate) const MAX_ENTRIES_PER_SCOPE: usize = 200;
pub(crate) const MAX_SCOPES: usize = 512;

/// Tenant and transport identity. API keys are internal stable IDs, never secrets.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Scope {
    pub account_id: String,
    pub service_id: String,
    pub api_key_id: Option<String>,
}

/// One TTL slot: valid from `since` (its first publication in the current
/// continuous validity window) until `expiry`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Slot {
    pub since: Instant,
    pub expiry: Instant,
}

impl Slot {
    /// A hit for a request dispatched at `at` must have existed and been valid
    /// then; entries published after dispatch never make a request warm.
    pub fn valid_at(&self, at: Instant) -> bool {
        self.since <= at && self.expiry > at
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Slots {
    pub short: Option<Slot>,
    pub long: Option<Slot>,
}

impl Slots {
    fn latest(&self) -> Option<Instant> {
        self.short
            .map(|slot| slot.expiry)
            .max(self.long.map(|slot| slot.expiry))
    }

    fn prune(&mut self, now: Instant) {
        self.short = self.short.filter(|slot| slot.expiry > now);
        self.long = self.long.filter(|slot| slot.expiry > now);
    }

    fn is_empty(&self) -> bool {
        self.short.is_none() && self.long.is_none()
    }

    fn extend(&mut self, ttl: Ttl, expiry: Instant, now: Instant) {
        let slot = match ttl {
            Ttl::FiveMinutes => &mut self.short,
            Ttl::OneHour => &mut self.long,
        };
        // A late-finishing older request never shortens a newer entry, and
        // extending a live slot keeps its original start.
        *slot = Some(match *slot {
            Some(current) => Slot {
                since: current.since,
                expiry: current.expiry.max(expiry),
            },
            None => Slot { since: now, expiry },
        });
    }
}

#[derive(Debug, Default)]
struct ScopeEntries {
    entries: HashMap<[u8; 32], Slots>,
    last_used: u64,
}

#[derive(Debug, Default)]
struct Inner {
    scopes: HashMap<Scope, ScopeEntries>,
    clock: u64,
}

#[derive(Debug, Default)]
pub(crate) struct EntryStore {
    inner: Mutex<Inner>,
}

/// One fingerprint update applied at publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Update {
    pub fingerprint: [u8; 32],
    pub ttl: Ttl,
    pub expiry: Instant,
}

impl EntryStore {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Bounded snapshot of one scope, pruned at `now`. Entries published after
    /// this call are invisible to the caller by construction.
    pub fn snapshot(&self, scope: &Scope, now: Instant) -> HashMap<[u8; 32], Slots> {
        let mut inner = self.lock();
        let Some(entries) = inner.scopes.get_mut(scope) else {
            return HashMap::new();
        };
        entries.entries.retain(|_, slots| {
            slots.prune(now);
            !slots.is_empty()
        });
        let snapshot = entries.entries.clone();
        if entries.entries.is_empty() {
            inner.scopes.remove(scope);
        }
        snapshot
    }

    pub fn publish(&self, scope: &Scope, updates: &[Update], now: Instant) {
        if updates.is_empty() {
            return;
        }
        let mut inner = self.lock();
        inner.clock = inner.clock.wrapping_add(1);
        let clock = inner.clock;
        if !inner.scopes.contains_key(scope) && inner.scopes.len() >= MAX_SCOPES {
            evict_scope(&mut inner, now);
        }
        let entries = inner.scopes.entry(scope.clone()).or_default();
        entries.last_used = clock;
        entries.entries.retain(|_, slots| {
            slots.prune(now);
            !slots.is_empty()
        });
        for update in updates.iter().filter(|update| update.expiry > now) {
            entries
                .entries
                .entry(update.fingerprint)
                .or_default()
                .extend(update.ttl, update.expiry, now);
        }
        while entries.entries.len() > MAX_ENTRIES_PER_SCOPE {
            let Some(oldest) = entries
                .entries
                .iter()
                .min_by_key(|(fingerprint, slots)| (slots.latest(), **fingerprint))
                .map(|(fingerprint, _)| *fingerprint)
            else {
                break;
            };
            entries.entries.remove(&oldest);
        }
    }

    #[cfg(test)]
    pub fn scope_count(&self) -> usize {
        self.lock().scopes.len()
    }

    #[cfg(test)]
    pub fn entry_count(&self, scope: &Scope) -> usize {
        self.lock()
            .scopes
            .get(scope)
            .map_or(0, |entries| entries.entries.len())
    }

    #[cfg(test)]
    pub fn expire_all(&self, at: Instant) {
        for entries in self.lock().scopes.values_mut() {
            for slots in entries.entries.values_mut() {
                for slot in [&mut slots.short, &mut slots.long].into_iter().flatten() {
                    slot.expiry = at;
                }
            }
        }
    }
}

/// Prefer a scope whose entries are all expired; otherwise the least recently used.
fn evict_scope(inner: &mut Inner, now: Instant) {
    let expired = inner
        .scopes
        .iter()
        .find(|(_, entries)| {
            entries
                .entries
                .values()
                .all(|slots| slots.latest().is_none_or(|expiry| expiry <= now))
        })
        .map(|(scope, _)| scope.clone());
    let victim = expired.or_else(|| {
        inner
            .scopes
            .iter()
            .min_by_key(|(_, entries)| entries.last_used)
            .map(|(scope, _)| scope.clone())
    });
    if let Some(victim) = victim {
        inner.scopes.remove(&victim);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn scope(account: &str, key: Option<&str>) -> Scope {
        Scope {
            account_id: account.into(),
            service_id: "service".into(),
            api_key_id: key.map(str::to_owned),
        }
    }

    fn update(byte: u8, ttl: Ttl, expiry: Instant) -> Update {
        Update {
            fingerprint: [byte; 32],
            ttl,
            expiry,
        }
    }

    #[test]
    fn slots_never_shrink_and_ttls_are_independent() {
        let store = EntryStore::default();
        let now = Instant::now();
        let a = scope("a", None);
        let late = now + Duration::from_secs(300);
        store.publish(
            &a,
            &[update(1, Ttl::OneHour, now + Duration::from_secs(3600))],
            now,
        );
        store.publish(&a, &[update(1, Ttl::FiveMinutes, late)], now);
        store.publish(
            &a,
            &[update(1, Ttl::FiveMinutes, now + Duration::from_secs(10))],
            now,
        );
        let slots = store.snapshot(&a, now)[&[1; 32]];
        assert_eq!(slots.short.map(|slot| slot.expiry), Some(late));
        assert_eq!(
            slots.long.map(|slot| slot.expiry),
            Some(now + Duration::from_secs(3600))
        );
        assert!(slots.short.unwrap().valid_at(now));
        assert!(!slots
            .short
            .unwrap()
            .valid_at(now - Duration::from_millis(1)));
    }

    #[test]
    fn scopes_isolate_keys_and_bounds_hold() {
        let store = EntryStore::default();
        let now = Instant::now();
        let expiry = now + Duration::from_secs(300);
        store.publish(
            &scope("a", Some("k1")),
            &[update(1, Ttl::FiveMinutes, expiry)],
            now,
        );
        assert!(store.snapshot(&scope("a", Some("k2")), now).is_empty());
        assert!(store.snapshot(&scope("a", None), now).is_empty());
        let a = scope("a", Some("k1"));
        let updates: Vec<_> = (0..=255u8)
            .map(|byte| update(byte, Ttl::FiveMinutes, expiry))
            .collect();
        store.publish(&a, &updates, now);
        assert_eq!(store.entry_count(&a), MAX_ENTRIES_PER_SCOPE);
        for index in 0..MAX_SCOPES + 10 {
            store.publish(
                &scope(&format!("s{index}"), None),
                &[update(1, Ttl::FiveMinutes, expiry)],
                now,
            );
        }
        assert_eq!(store.scope_count(), MAX_SCOPES);
    }

    #[test]
    fn expired_updates_and_entries_are_dropped() {
        let store = EntryStore::default();
        let now = Instant::now();
        let a = scope("a", None);
        store.publish(&a, &[update(1, Ttl::FiveMinutes, now)], now);
        assert!(store.snapshot(&a, now).is_empty());
        store.publish(
            &a,
            &[update(2, Ttl::FiveMinutes, now + Duration::from_secs(1))],
            now,
        );
        store.expire_all(now);
        assert!(store.snapshot(&a, now).is_empty());
        assert_eq!(store.scope_count(), 0);
    }
}

#[cfg(test)]
mod performance {
    use super::*;
    #[test]
    #[ignore = "manual lock-duration upper bound and bounded-store memory fixture"]
    fn performance_store_snapshot_budget() {
        let store = EntryStore::default();
        let now = Instant::now();
        let updates = (0..MAX_ENTRIES_PER_SCOPE)
            .map(|index| {
                let mut fingerprint = [0; 32];
                fingerprint[..8].copy_from_slice(&(index as u64).to_le_bytes());
                Update {
                    fingerprint,
                    ttl: Ttl::FiveMinutes,
                    expiry: now + std::time::Duration::from_secs(300),
                }
            })
            .collect::<Vec<_>>();
        let scope = |n| Scope {
            account_id: format!("account-{n}"),
            service_id: "fixture".into(),
            api_key_id: None,
        };
        for n in 0..MAX_SCOPES {
            store.publish(&scope(n), &updates, now);
        }
        let mut samples = Vec::new();
        for _ in 0..1000 {
            let started = Instant::now();
            let snapshot = store.snapshot(&scope(0), now);
            samples.push(started.elapsed().as_nanos());
            assert_eq!(snapshot.len(), MAX_ENTRIES_PER_SCOPE);
        }
        samples.sort_unstable();
        println!("PERF store scopes={} entries={} snapshot_p99_us={} max_us={} (includes lock acquisition, copy and release)", MAX_SCOPES, MAX_SCOPES * MAX_ENTRIES_PER_SCOPE, samples[989] / 1000, samples[999] / 1000);
        let mut publication = Vec::new();
        for n in MAX_SCOPES..MAX_SCOPES + 200 {
            let next = scope(n);
            let started = Instant::now();
            store.publish(&next, &updates, now);
            publication.push(started.elapsed().as_nanos());
            assert_eq!(store.scope_count(), MAX_SCOPES);
            assert_eq!(store.entry_count(&next), MAX_ENTRIES_PER_SCOPE);
        }
        publication.sort_unstable();
        println!("PERF store full_capacity_publish_p99_us={} max_us={} (includes scope eviction and 200 writes; upper bound on lock hold)", publication[197] / 1000, publication[199] / 1000);
    }
}
