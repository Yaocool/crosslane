//! Provider runtime registration and provider-specific management handles.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use crosslane_copilot::{CopilotProvider, CopilotSettings};
use crosslane_core::config::{Config, ProviderConfig};
use crosslane_core::provider::{
    CapabilitySupport, ProviderCapabilities, ProviderDescriptor, ProviderId, ProviderModel,
    ProviderProtocol,
};
use crosslane_provider::{
    ProviderAdapter, ProviderError, ProviderRegistry, ProviderRequest, ProviderResponse,
};
use tokio::sync::RwLock;
use tracing::warn;

use crate::stats::ModelCache;

pub(crate) mod browser_login;

pub struct ProviderManager {
    model_policy: Arc<crosslane_provider::ModelPolicy>,
    registry: Arc<ProviderRegistry>,
    copilots: RwLock<BTreeMap<ProviderId, CopilotEntry>>,
    kiro_models: Arc<ModelCache>,
    pub browser_logins: browser_login::BrowserLoginManager,
}

struct CopilotEntry {
    config: ProviderConfig,
    runtime: Arc<CopilotProvider>,
}

impl ProviderManager {
    pub fn new(kiro_models: Arc<ModelCache>) -> Self {
        Self {
            model_policy: Arc::new(crosslane_provider::ModelPolicy::default()),
            registry: Arc::new(ProviderRegistry::new()),
            copilots: RwLock::new(BTreeMap::new()),
            kiro_models,
            browser_logins: browser_login::BrowserLoginManager::default(),
        }
    }

    pub fn registry(&self) -> Arc<ProviderRegistry> {
        Arc::clone(&self.registry)
    }

    pub fn with_model_policy(mut self, policy: Arc<crosslane_provider::ModelPolicy>) -> Self {
        self.model_policy = policy;
        self
    }

    pub async fn reconcile(&self, config: &Config, data_dir: &Path) -> Result<(), String> {
        let provider_configs = config.effective_providers();
        let existing = self.copilots.read().await;
        let mut adapters = Vec::<Arc<dyn ProviderAdapter>>::new();
        let mut next_copilots = BTreeMap::new();
        for provider in provider_configs {
            let id = ProviderId::parse(provider.id.clone()).map_err(|error| error.to_string())?;
            match provider.kind.as_str() {
                "kiro" => adapters.push(Arc::new(KiroProvider {
                    id,
                    enabled: provider.enabled,
                    models: Arc::clone(&self.kiro_models),
                    config: config.clone(),
                })),
                "copilot" => {
                    let runtime = if let Some(current) = existing.get(&id) {
                        if current.config == provider {
                            Ok(Arc::clone(&current.runtime))
                        } else {
                            CopilotProvider::load(
                                id.clone(),
                                provider.enabled,
                                CopilotSettings::from_config(&provider),
                                provider_accounts_path(data_dir, &id),
                            )
                            .await
                            .map(|runtime| {
                                Arc::new(runtime.with_model_policy(Arc::clone(&self.model_policy)))
                            })
                        }
                    } else {
                        CopilotProvider::load(
                            id.clone(),
                            provider.enabled,
                            CopilotSettings::from_config(&provider),
                            provider_accounts_path(data_dir, &id),
                        )
                        .await
                        .map(|runtime| {
                            Arc::new(runtime.with_model_policy(Arc::clone(&self.model_policy)))
                        })
                    };
                    match runtime {
                        Ok(runtime) => {
                            adapters.push(runtime.clone());
                            next_copilots.insert(
                                id,
                                CopilotEntry {
                                    config: provider,
                                    runtime,
                                },
                            );
                        }
                        Err(error) => {
                            let message = error.to_string();
                            warn!(
                                provider_id = %provider.id,
                                provider_kind = %provider.kind,
                                error = %message,
                                "provider runtime is unavailable; other providers remain active"
                            );
                            adapters.push(Arc::new(UnavailableProvider {
                                config: provider,
                                id,
                                error: message,
                            }));
                        }
                    }
                }
                _ => adapters.push(Arc::new(UnsupportedProvider {
                    config: provider,
                    id,
                })),
            }
        }
        drop(existing);
        self.browser_logins.reconcile(
            &next_copilots
                .values()
                .map(|entry| Arc::clone(&entry.runtime))
                .collect::<Vec<_>>(),
        );
        *self.copilots.write().await = next_copilots;
        self.registry.replace_all(adapters).await;
        Ok(())
    }

    pub async fn copilot(&self, id: &ProviderId) -> Option<Arc<CopilotProvider>> {
        self.copilots
            .read()
            .await
            .get(id)
            .map(|entry| Arc::clone(&entry.runtime))
    }

    pub async fn descriptors(&self) -> Vec<ProviderDescriptor> {
        self.registry.descriptors().await
    }

    pub async fn models(
        &self,
        provider_ids: &[ProviderId],
        refresh: bool,
    ) -> (Vec<ProviderModel>, BTreeMap<String, String>) {
        let mut models = Vec::new();
        let mut errors = BTreeMap::new();
        for id in provider_ids {
            let Some(adapter) = self.registry.get(id).await else {
                errors.insert(id.to_string(), "provider is not registered".into());
                continue;
            };
            match adapter.models(refresh).await {
                Ok(mut discovered) => models.append(&mut discovered),
                Err(error) => {
                    errors.insert(id.to_string(), error.to_string());
                }
            }
        }
        models.sort_by(|left, right| {
            left.provider_id
                .cmp(&right.provider_id)
                .then_with(|| left.id.cmp(&right.id))
        });
        (models, errors)
    }
}

fn provider_accounts_path(data_dir: &Path, id: &ProviderId) -> std::path::PathBuf {
    data_dir
        .join("providers")
        .join(id.as_str())
        .join("accounts.json")
}

struct KiroProvider {
    id: ProviderId,
    enabled: bool,
    models: Arc<ModelCache>,
    config: Config,
}

#[async_trait]
impl ProviderAdapter for KiroProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: self.id.clone(),
            kind: "kiro".into(),
            enabled: self.enabled,
            status: if self.enabled { "ready" } else { "disabled" }.into(),
            error: None,
            capabilities: ProviderCapabilities {
                protocols: vec![
                    ProviderProtocol::ClaudeMessages,
                    ProviderProtocol::OpenAiChat,
                    ProviderProtocol::OpenAiResponses,
                ],
                device_flow: false,
                token_import: true,
                model_discovery: true,
                token_counting: CapabilitySupport::Adapted,
                usage: CapabilitySupport::Native,
            },
        }
    }

    async fn models(&self, _refresh: bool) -> Result<Vec<ProviderModel>, ProviderError> {
        let (cached, _) = self.models.get(u64::MAX);
        let models = if cached.is_empty() {
            crate::http::fallback_models(&self.config)
        } else {
            cached
        };
        Ok(kiro_provider_models(&self.id, models))
    }

    async fn execute(&self, _request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        Err(ProviderError::unsupported(
            "Kiro execution uses the compatibility executor",
        ))
    }
}

pub(crate) fn kiro_provider_models(
    provider_id: &ProviderId,
    models: Vec<crosslane_kiro::ModelInfo>,
) -> Vec<ProviderModel> {
    models
        .into_iter()
        .map(|model| {
            let (max_input_tokens, max_output_tokens) = model
                .token_limits
                .map(|limits| {
                    (
                        limits.max_input_tokens.map(u64::from),
                        limits.max_output_tokens.map(u64::from),
                    )
                })
                .unwrap_or_default();
            ProviderModel {
                provider_id: provider_id.clone(),
                id: model.model_id,
                display_name: model.model_name,
                vendor: Some("kiro".into()),
                max_input_tokens,
                max_output_tokens,
                protocols: vec![
                    ProviderProtocol::ClaudeMessages,
                    ProviderProtocol::OpenAiChat,
                    ProviderProtocol::OpenAiResponses,
                ],
                capabilities: serde_json::Value::Null,
            }
        })
        .collect()
}

struct UnsupportedProvider {
    config: ProviderConfig,
    id: ProviderId,
}

struct UnavailableProvider {
    config: ProviderConfig,
    id: ProviderId,
    error: String,
}

#[async_trait]
impl ProviderAdapter for UnavailableProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: self.id.clone(),
            kind: self.config.kind.clone(),
            enabled: self.config.enabled,
            status: if self.config.enabled {
                "unavailable"
            } else {
                "disabled"
            }
            .into(),
            error: Some(self.error.clone()),
            capabilities: ProviderCapabilities::default(),
        }
    }

    async fn models(&self, _refresh: bool) -> Result<Vec<ProviderModel>, ProviderError> {
        Err(ProviderError::unavailable(self.error.clone()))
    }

    async fn execute(&self, _request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        Err(ProviderError::unavailable(self.error.clone()))
    }

    async fn count_tokens(
        &self,
        _request: ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        Err(ProviderError::unavailable(self.error.clone()))
    }
}

#[async_trait]
impl ProviderAdapter for UnsupportedProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor {
            id: self.id.clone(),
            kind: self.config.kind.clone(),
            enabled: self.config.enabled,
            status: "unsupported".into(),
            error: Some(format!(
                "provider driver {} is not installed",
                self.config.kind
            )),
            capabilities: ProviderCapabilities::default(),
        }
    }

    async fn models(&self, _refresh: bool) -> Result<Vec<ProviderModel>, ProviderError> {
        Err(ProviderError::unsupported(format!(
            "provider driver {} is not installed",
            self.config.kind
        )))
    }

    async fn execute(&self, _request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        Err(ProviderError::unsupported(format!(
            "provider driver {} is not installed",
            self.config.kind
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn broken_copilot_state_does_not_remove_the_kiro_runtime() {
        let directory = tempfile::tempdir().expect("tempdir");
        let copilot_dir = directory.path().join("providers/copilot");
        tokio::fs::create_dir_all(&copilot_dir)
            .await
            .expect("provider directory");
        tokio::fs::write(copilot_dir.join("accounts.json"), "{broken-json")
            .await
            .expect("broken account state");

        let config = Config {
            provider: vec![
                ProviderConfig::default(),
                ProviderConfig {
                    id: "copilot".into(),
                    kind: "copilot".into(),
                    ..ProviderConfig::default()
                },
            ],
            ..Config::default()
        };
        let manager = ProviderManager::new(Arc::new(ModelCache::default()));

        manager
            .reconcile(&config, directory.path())
            .await
            .expect("one provider failure must not fail reconciliation");

        let descriptors = manager.descriptors().await;
        let kiro = descriptors
            .iter()
            .find(|provider| provider.id.as_str() == "kiro")
            .expect("Kiro descriptor");
        assert_eq!(kiro.status, "ready");
        assert!(kiro.error.is_none());
        let copilot = descriptors
            .iter()
            .find(|provider| provider.id.as_str() == "copilot")
            .expect("Copilot descriptor");
        assert_eq!(copilot.status, "unavailable");
        assert!(copilot
            .error
            .as_deref()
            .is_some_and(|error| error.contains("parse")));
        assert!(manager
            .registry()
            .get(&ProviderId::parse("kiro").expect("provider ID"))
            .await
            .is_some());
    }
}
