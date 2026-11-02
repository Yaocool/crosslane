use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use super::{EnterpriseError, EnterpriseScope};

#[derive(Hash, PartialEq, Eq)]
struct UserKey {
    account: String,
    region: String,
    user: String,
}

/// Shared by clients serving the same administrator API. A username and its
/// resolved UUID must not race each other during a subscription change.
#[derive(Default)]
pub struct MutationCoordinator {
    users: Mutex<HashMap<UserKey, Weak<AsyncMutex<()>>>>,
}

impl MutationCoordinator {
    pub(super) fn try_begin(
        &self,
        scope: &EnterpriseScope,
        user: &str,
    ) -> Result<OwnedMutexGuard<()>, EnterpriseError> {
        let key = UserKey {
            account: scope.account_id.clone(),
            region: scope.kiro_region.clone(),
            user: user.to_ascii_lowercase(),
        };
        let lock = {
            let mut users = self.users.lock().map_err(|_| {
                EnterpriseError::new(
                    "client_unavailable",
                    503,
                    "Kiro mutation coordinator is unavailable",
                )
            })?;
            // The guard owns the lock. Weak entries avoid retaining a
            // permanent registry of directory users in this process.
            users.retain(|_, lock| lock.strong_count() != 0);
            if let Some(lock) = users.get(&key).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(AsyncMutex::new(()));
                users.insert(key, Arc::downgrade(&lock));
                lock
            }
        };
        lock.try_lock_owned().map_err(|_| EnterpriseError::conflict("another subscription operation is in progress for this user; query state before retrying"))
    }
}
