//! Weighted multi-account scheduler with RAII permits and credit reservations.

mod pool;
mod refresh;
mod state;

pub use pool::{
    account_credit_state, effective_credit_limit, remaining_credit_percent, usage_credit_exhausted,
    AccountCreditState, AccountLease, AccountPool, AccountPoolCounts, PoolError, ScoreExplanation,
};
pub use refresh::{
    RefreshError, RefreshOutcome, RefreshedCredentials, ReloadedCredentials, TokenRefresher,
};
pub use state::{AccountHealth, AccountState};
