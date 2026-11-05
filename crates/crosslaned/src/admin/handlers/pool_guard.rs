//! Rejects edits that would leave an enabled proxy service without any
//! available account. Callers bypass the check by passing `force`.

use std::collections::HashMap;
use std::sync::Arc;

use crosslane_core::config::{Config, ProxyServiceConfig};
use crosslane_ipc::protocol::{ProviderAccountListResult, RpcError};

use crate::state::AppState;

/// Provider-qualified account key mapped to the tags it will carry.
type TagOverrides = HashMap<(String, String), Vec<String>>;

/// One account's tags before and after a requested tag edit.
pub(super) struct TagChange {
    pub provider: String,
    pub id: String,
    pub before: Vec<String>,
    pub after: Vec<String>,
}

pub(super) use crate::state::same_account_tags as same_tags;

/// Checks enabled services whose `account_tag` differs between `current` and
/// `next`. Rejected daemon reloads restore the running file, so `next` never
/// carries someone else's unapplied tag edit.
pub(super) async fn service_tag_change_error(
    state: &Arc<AppState>,
    current: &Config,
    next: &Config,
) -> Result<Option<String>, RpcError> {
    let transitions = next
        .proxy_service
        .iter()
        .filter_map(|after| {
            current
                .proxy_service
                .iter()
                .find(|before| before.id == after.id)
                .filter(|before| {
                    !same_tags(
                        before.selected_account_tags(),
                        after.selected_account_tags(),
                    )
                })
                .map(|before| (before.clone(), after.clone()))
        })
        .collect::<Vec<_>>();
    check(state, next, &transitions, &TagOverrides::new()).await
}

/// Checks enabled services that would lose at least one of the edited accounts.
pub(super) async fn account_tag_change_error(
    state: &Arc<AppState>,
    changes: &[TagChange],
) -> Result<Option<String>, RpcError> {
    let config = state.config.current();
    let transitions = config
        .proxy_service
        .iter()
        .filter(|service| {
            changes.iter().any(|change| {
                service.includes_provider_account(&change.provider, &change.id, &change.before)
                    && !service.includes_provider_account(
                        &change.provider,
                        &change.id,
                        &change.after,
                    )
            })
        })
        .map(|service| (service.clone(), service.clone()))
        .collect::<Vec<_>>();
    let overrides = changes
        .iter()
        .map(|change| {
            (
                (change.provider.clone(), change.id.clone()),
                change.after.clone(),
            )
        })
        .collect();
    check(state, &config, &transitions, &overrides).await
}

/// A service fails the check only when it has an available account now and
/// none afterwards; an already empty pool is not made worse by the edit.
async fn check(
    state: &Arc<AppState>,
    config: &Config,
    transitions: &[(ProxyServiceConfig, ProxyServiceConfig)],
    overrides: &TagOverrides,
) -> Result<Option<String>, RpcError> {
    if !transitions.iter().any(|(_, after)| after.enabled) {
        return Ok(None);
    }
    let listing: ProviderAccountListResult = serde_json::from_value(
        super::providers::cached_account_list(state, serde_json::Value::Null).await?,
    )
    .map_err(|error| RpcError::internal(error.to_string()))?;
    let unchanged = TagOverrides::new();
    let emptied = transitions
        .iter()
        .filter(|(before, after)| {
            after.enabled
                && has_available(&listing, config, before, &unchanged) == Some(true)
                && has_available(&listing, config, after, overrides) == Some(false)
        })
        .map(|(_, after)| format!("{} ({})", after.name, after.id))
        .collect::<Vec<_>>();
    Ok((!emptied.is_empty()).then(|| {
        format!(
            "proxy service(s) {} would have no available account after this change; \
             retry with force (CLI: --force) to apply it anyway",
            emptied.join(", ")
        )
    }))
}

/// `None` means a provider in the service scope could not be listed, so an
/// empty pool cannot be proven.
fn has_available(
    listing: &ProviderAccountListResult,
    config: &Config,
    service: &ProxyServiceConfig,
    overrides: &TagOverrides,
) -> Option<bool> {
    let scope = config.allowed_providers_for_service(service);
    if scope
        .iter()
        .any(|provider| listing.errors.contains_key(*provider))
    {
        return None;
    }
    Some(listing.accounts.iter().any(|account| {
        let tags = overrides
            .get(&(account.provider_id.clone(), account.id.clone()))
            .unwrap_or(&account.tags);
        account.enabled
            && account.health == "available"
            && service.includes_provider_account(&account.provider_id, &account.id, tags)
    }))
}
