//! Shared live transport barrier. Cloned clients retain this same policy.

use kproxy_core::config::ModelsConfig;
use tokio::sync::{RwLock, RwLockReadGuard};

use crate::{ProviderError, ProviderErrorKind};

#[derive(Default)]
pub struct ModelPolicy {
    rules: RwLock<ModelsConfig>,
}

impl ModelPolicy {
    pub fn new(rules: ModelsConfig) -> Self {
        Self {
            rules: RwLock::new(rules),
        }
    }

    /// Wait for sends already admitted to receive headers. Writer priority
    /// stops queued sends; response bodies can finish, but continuations and
    /// retries must acquire a new permit against the current rules.
    pub async fn replace(&self, rules: &ModelsConfig) {
        *self.rules.write().await = rules.clone();
    }

    /// Keep the permit until the outbound send resolves, not for the stream.
    pub async fn admit<'a>(
        &'a self,
        models: &[&str],
    ) -> Result<RwLockReadGuard<'a, ModelsConfig>, ProviderError> {
        let rules = self.rules.read().await;
        if let Some(model) = models.iter().find(|model| rules.is_disabled(model)) {
            let mut error = ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                http::StatusCode::BAD_REQUEST,
                format!("model_disabled: model {model} is globally disabled"),
            );
            error.upstream_code = Some("model_disabled".into());
            return Err(error);
        }
        Ok(rules)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disabled_models_barrier_blocks_queued_sends_and_reenables() {
        let policy = ModelPolicy::default();
        let in_flight = policy.admit(&["gpt-test"]).await.unwrap();
        let disabled = ModelsConfig {
            disabled: vec!["gpt-test".into()],
            ..Default::default()
        };
        let update = policy.replace(&disabled);
        tokio::pin!(update);
        assert!(futures::poll!(&mut update).is_pending());
        let next = policy.admit(&["gpt-test"]);
        tokio::pin!(next);
        assert!(futures::poll!(&mut next).is_pending());
        drop(in_flight);
        update.await;
        let error = next.await.unwrap_err();
        assert!(!error.retryable && !error.account_error);
        assert_eq!(error.upstream_code.as_deref(), Some("model_disabled"));
        assert!(policy.admit(&["allowed", "gpt-test"]).await.is_err());
        policy.replace(&ModelsConfig::default()).await;
        assert!(policy.admit(&["gpt-test"]).await.is_ok());
    }
}
