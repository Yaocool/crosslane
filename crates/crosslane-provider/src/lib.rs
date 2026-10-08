//! Provider adapter contract and concurrent runtime registry.

mod model_policy;
pub use model_policy::ModelPolicy;

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use crosslane_core::provider::{ProviderDescriptor, ProviderId, ProviderModel, ProviderProtocol};
use futures::Stream;
use http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::RwLock;

pub type ProviderByteStream =
    Pin<Box<dyn Stream<Item = Result<Bytes, ProviderError>> + Send + 'static>>;

/// A request after shared authentication and routing have completed.
#[derive(Debug, Clone)]
pub struct ProviderRequest {
    pub protocol: ProviderProtocol,
    /// Provider-local model ID after shared mapping and authorization.
    pub model: String,
    /// Original unqualified model, retained across mappings for live deny checks.
    pub original_model: Option<String>,
    pub path: String,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub trace_id: String,
    pub service_id: String,
    /// Immutable service membership rules for account selection and failover.
    pub account_pool: Option<crosslane_core::config::ProxyServiceConfig>,
    pub api_key_id: Option<String>,
}

/// Response body returned by a provider. Streaming stays streaming end to end.
pub enum ProviderResponseBody {
    Full(Bytes),
    Stream(ProviderByteStream),
}

pub struct ProviderResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: ProviderResponseBody,
    pub account_id: Option<String>,
    pub upstream_request_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    Authentication,
    Authorization,
    RateLimited,
    Unavailable,
    InvalidRequest,
    Unsupported,
    Transport,
    Upstream,
    Internal,
}

#[derive(Debug, Clone, Error)]
#[error("{message}")]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub message: String,
    pub status: StatusCode,
    pub retryable: bool,
    pub account_error: bool,
    pub upstream_code: Option<String>,
}

impl ProviderError {
    pub fn new(kind: ProviderErrorKind, status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status,
            retryable: false,
            account_error: false,
            upstream_code: None,
        }
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(
            ProviderErrorKind::Unsupported,
            StatusCode::BAD_REQUEST,
            message,
        )
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        let mut error = Self::new(
            ProviderErrorKind::Unavailable,
            StatusCode::SERVICE_UNAVAILABLE,
            message,
        );
        error.retryable = true;
        error
    }
}

/// Adapter boundary implemented by Kiro, Copilot, and future providers.
#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;

    async fn models(&self, refresh: bool) -> Result<Vec<ProviderModel>, ProviderError>;

    async fn execute(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError>;

    async fn count_tokens(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        let _ = request;
        Err(ProviderError::unsupported(
            "this provider does not support token counting",
        ))
    }
}

/// Thread-safe registry keyed by provider instance ID.
#[derive(Default)]
pub struct ProviderRegistry {
    adapters: RwLock<BTreeMap<ProviderId, Arc<dyn ProviderAdapter>>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn register(
        &self,
        adapter: Arc<dyn ProviderAdapter>,
    ) -> Option<Arc<dyn ProviderAdapter>> {
        let id = adapter.descriptor().id;
        self.adapters.write().await.insert(id, adapter)
    }

    pub async fn unregister(&self, id: &ProviderId) -> Option<Arc<dyn ProviderAdapter>> {
        self.adapters.write().await.remove(id)
    }

    pub async fn get(&self, id: &ProviderId) -> Option<Arc<dyn ProviderAdapter>> {
        self.adapters.read().await.get(id).cloned()
    }

    pub async fn descriptors(&self) -> Vec<ProviderDescriptor> {
        self.adapters
            .read()
            .await
            .values()
            .map(|adapter| adapter.descriptor())
            .collect()
    }

    pub async fn replace_all(&self, adapters: Vec<Arc<dyn ProviderAdapter>>) {
        let mut next = BTreeMap::new();
        for adapter in adapters {
            next.insert(adapter.descriptor().id, adapter);
        }
        *self.adapters.write().await = next;
    }

    pub async fn contains(&self, id: &ProviderId) -> bool {
        self.adapters.read().await.contains_key(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosslane_core::provider::{CapabilitySupport, ProviderCapabilities};

    struct MockAdapter {
        id: ProviderId,
    }

    #[async_trait]
    impl ProviderAdapter for MockAdapter {
        fn descriptor(&self) -> ProviderDescriptor {
            ProviderDescriptor {
                id: self.id.clone(),
                kind: "mock-third-provider".into(),
                enabled: true,
                status: "ready".into(),
                error: None,
                capabilities: ProviderCapabilities {
                    protocols: vec![ProviderProtocol::OpenAiChat],
                    model_discovery: true,
                    usage: CapabilitySupport::Native,
                    ..ProviderCapabilities::default()
                },
            }
        }

        async fn models(&self, _refresh: bool) -> Result<Vec<ProviderModel>, ProviderError> {
            Ok(vec![ProviderModel {
                provider_id: self.id.clone(),
                id: "mock-model".into(),
                display_name: "Mock model".into(),
                vendor: Some("mock".into()),
                max_input_tokens: Some(10_000),
                max_output_tokens: Some(2_000),
                protocols: vec![ProviderProtocol::OpenAiChat],
                capabilities: serde_json::Value::Null,
            }])
        }

        async fn execute(
            &self,
            _request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            Err(ProviderError::unsupported("test adapter"))
        }
    }

    #[tokio::test]
    async fn a_third_driver_registers_without_registry_changes() {
        let registry = ProviderRegistry::new();
        let id = ProviderId::parse("mock-a").unwrap();
        registry
            .register(Arc::new(MockAdapter { id: id.clone() }))
            .await;
        assert!(registry.contains(&id).await);
        assert_eq!(registry.descriptors().await[0].kind, "mock-third-provider");
        assert_eq!(
            registry
                .get(&id)
                .await
                .unwrap()
                .models(false)
                .await
                .unwrap()
                .len(),
            1
        );
    }
}
