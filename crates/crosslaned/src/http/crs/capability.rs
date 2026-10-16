//! One bounded recovery for an explicit unsupported output_config.effort.
//! Never changes thinking history, schemas, tool calls, or generic HTTP 400s.

use std::sync::Arc;

use crosslane_provider::{
    ProviderAdapter, ProviderError, ProviderRequest, ProviderResponse, ProviderResponseBody,
};

use super::*;

fn without_rejected_effort(body: &[u8], status: StatusCode, message: &str) -> Option<Bytes> {
    if status != StatusCode::BAD_REQUEST {
        return None;
    }
    let lower = message.to_ascii_lowercase();
    if !lower.contains("output_config.effort")
        || ![
            "extra inputs are not permitted",
            "not supported",
            "unsupported",
            "unknown field",
            "unrecognized",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        return None;
    }
    let mut value: Value = serde_json::from_slice(body).ok()?;
    let config = value.get_mut("output_config")?.as_object_mut()?;
    config.remove("effort")?;
    if config.is_empty() {
        value.as_object_mut()?.remove("output_config");
    }
    Some(Bytes::from(serde_json::to_vec(&value).ok()?))
}

async fn inspect_rejection(response: &mut ProviderResponse) -> Option<Value> {
    let original = std::mem::replace(&mut response.body, ProviderResponseBody::Full(Bytes::new()));
    let ProviderResponseBody::Stream(mut source) = original else {
        let ProviderResponseBody::Full(bytes) = &original else {
            unreachable!()
        };
        let value = (bytes.len() <= MAX_ERROR_BODY_BYTES)
            .then(|| serde_json::from_slice(bytes).ok())
            .flatten();
        response.body = original;
        return value;
    };
    let mut prefix = Vec::new();
    let deadline = tokio::time::Instant::now() + PREFLIGHT_WAIT;
    loop {
        match tokio::time::timeout_at(deadline, source.next()).await {
            Ok(None) => {
                let value = serde_json::from_slice(&prefix).ok();
                response.body = ProviderResponseBody::Full(Bytes::from(prefix));
                return value;
            }
            Ok(Some(Ok(bytes)))
                if prefix.len().saturating_add(bytes.len()) <= MAX_ERROR_BODY_BYTES =>
            {
                prefix.extend_from_slice(&bytes)
            }
            next => {
                let stream = async_stream::stream! {
                    if !prefix.is_empty() { yield Ok(Bytes::from(prefix)); }
                    if let Ok(Some(chunk)) = next { yield chunk; }
                    while let Some(chunk) = source.next().await { yield chunk; }
                };
                response.body = ProviderResponseBody::Stream(Box::pin(stream));
                return None;
            }
        }
    }
}

pub(crate) async fn execute_with_effort_recovery(
    adapter: &Arc<dyn ProviderAdapter>,
    request: ProviderRequest,
    count_tokens: bool,
) -> Result<ProviderResponse, ProviderError> {
    if count_tokens {
        return adapter.count_tokens(request).await;
    }
    if request.protocol != crosslane_core::provider::ProviderProtocol::ClaudeMessages
        || request
            .account_pool
            .as_ref()
            .is_some_and(|pool| pool.crs_relay == Some(crosslane_core::config::CrsRelayMode::Ccr))
    {
        return adapter.execute(request).await;
    }
    let mut response = adapter.execute(request.clone()).await?;
    if response.status != StatusCode::BAD_REQUEST {
        return Ok(response);
    }
    let Some(value) = inspect_rejection(&mut response).await else {
        return Ok(response);
    };
    let Some(body) = without_rejected_effort(
        &request.body,
        response.status,
        error_message(&value).unwrap_or_default(),
    ) else {
        return Ok(response);
    };
    let mut retry = request;
    retry.body = body;
    // Retain the exact account that rejected the field. Changing accounts would
    // infer capabilities from a different supplier and could escape its scope.
    let Some(account_id) = response.account_id.as_ref() else {
        return Ok(response);
    };
    let Some(pool) = retry.account_pool.as_mut() else {
        return Ok(response);
    };
    let provider_id = adapter.descriptor().id;
    // The adapter selected this account using the original pool. Recheck
    // membership when it is independent of account tags, so an unexpected
    // response cannot introduce an account outside a frozen explicit scope.
    if pool.selected_account_tags().is_empty()
        && !pool.includes_provider_account(provider_id.as_str(), account_id, &[])
    {
        return Ok(response);
    }
    // `None` inherits the global pool, and bare IDs only identify Kiro
    // accounts. Disable the base pool and use the provider instance ID, while
    // retaining exclusions and all other service/provider permissions.
    pool.account_tag = Some(crosslane_core::config::ServiceAccountTags::Many(Vec::new()));
    pool.account_ids = vec![format!("{provider_id}/{account_id}")];
    if !pool.includes_provider_account(provider_id.as_str(), account_id, &[]) {
        return Ok(response);
    }
    tracing::info!(event = "crs.capability.recovery", trace_id = %retry.trace_id, service_id = %retry.service_id, model = %retry.model, account_id, field = "output_config.effort", "retrying explicit unsupported effort once before output");
    drop(response);
    adapter.execute(retry).await
}

#[cfg(test)]
mod tests {
    use super::*;

    use crosslane_core::config::{CrsRelayMode, ProxyServiceConfig, ServiceAccountTags};
    use crosslane_core::provider::{
        ProviderCapabilities, ProviderDescriptor, ProviderId, ProviderModel, ProviderProtocol,
    };
    use std::sync::Mutex;

    const EFFORT_REJECTION: &[u8] = br#"{"error":{"type":"invalid_request_error","message":"output_config.effort: Extra inputs are not permitted"}}"#;

    struct EffortProbe {
        provider_id: &'static str,
        account_id: Option<&'static str>,
        requests: Mutex<Vec<ProviderRequest>>,
        count_requests: Mutex<Vec<ProviderRequest>>,
    }

    impl EffortProbe {
        fn new(provider_id: &'static str, account_id: Option<&'static str>) -> Arc<Self> {
            Arc::new(Self {
                provider_id,
                account_id,
                requests: Mutex::new(Vec::new()),
                count_requests: Mutex::new(Vec::new()),
            })
        }

        fn rejection(&self) -> ProviderResponse {
            ProviderResponse {
                status: StatusCode::BAD_REQUEST,
                headers: Default::default(),
                body: ProviderResponseBody::Full(Bytes::from_static(EFFORT_REJECTION)),
                account_id: self.account_id.map(str::to_owned),
                upstream_request_id: None,
            }
        }
    }

    #[async_trait::async_trait]
    impl ProviderAdapter for EffortProbe {
        fn descriptor(&self) -> ProviderDescriptor {
            ProviderDescriptor {
                id: ProviderId::parse(self.provider_id).unwrap(),
                kind: "copilot".into(),
                enabled: true,
                status: "ready".into(),
                error: None,
                capabilities: ProviderCapabilities::default(),
            }
        }

        async fn models(&self, _: bool) -> Result<Vec<ProviderModel>, ProviderError> {
            Ok(Vec::new())
        }

        async fn execute(
            &self,
            request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            self.requests.lock().unwrap().push(request);
            // Reject the retry as well to assert that recovery is bounded.
            Ok(self.rejection())
        }

        async fn count_tokens(
            &self,
            request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            self.count_requests.lock().unwrap().push(request);
            Ok(self.rejection())
        }
    }

    fn effort_request(pool: Option<ProxyServiceConfig>) -> ProviderRequest {
        ProviderRequest {
            protocol: ProviderProtocol::ClaudeMessages,
            model: "claude-sonnet-4-6".into(),
            original_model: Some("client-model".into()),
            path: "/v1/messages".into(),
            headers: Default::default(),
            body: Bytes::from_static(br#"{"model":"claude-sonnet-4-6","max_tokens":32,"output_config":{"effort":"high"},"messages":[{"role":"user","content":"hello"}]}"#),
            trace_id: "effort-scope-test".into(),
            service_id: "restricted-service".into(),
            account_pool: pool,
            api_key_id: Some("restricted-key".into()),
            client_context: Default::default(),
        }
    }

    #[tokio::test]
    async fn effort_recovery_pins_the_selected_provider_account_without_expanding_scope() {
        for tags in [
            Some(ServiceAccountTags::Many(Vec::new())),
            Some(ServiceAccountTags::One("team-a".into())),
            None,
        ] {
            let pool = ProxyServiceConfig {
                id: "restricted-service".into(),
                allowed_providers: vec!["copilot-team-a".into(), "kiro".into()],
                account_tag: tags,
                account_ids: vec![
                    "copilot-team-a/selected".into(),
                    "copilot-team-a/other-allowed".into(),
                ],
                excluded_account_ids: vec!["copilot-team-a/forbidden".into()],
                ..Default::default()
            };
            let probe = EffortProbe::new("copilot-team-a", Some("selected"));
            let adapter: Arc<dyn ProviderAdapter> = probe.clone();
            let original = effort_request(Some(pool.clone()));
            let response = execute_with_effort_recovery(&adapter, original.clone(), false)
                .await
                .unwrap();
            assert_eq!(response.status, StatusCode::BAD_REQUEST);
            let requests = probe.requests.lock().unwrap();
            assert_eq!(requests.len(), 2, "only one effort retry is permitted");
            let retry = &requests[1];
            let narrowed = retry.account_pool.as_ref().unwrap();
            assert_eq!(narrowed.account_ids, ["copilot-team-a/selected"]);
            assert!(narrowed.includes_provider_account("copilot-team-a", "selected", &[]));
            for id in ["other-allowed", "outside", "forbidden"] {
                assert!(!narrowed.includes_provider_account(
                    "copilot-team-a",
                    id,
                    &["team-a".into()]
                ));
            }
            assert!(!narrowed.includes_provider_account("copilot-team-b", "selected", &[]));
            assert!(!narrowed.includes_provider_account("kiro", "selected", &[]));
            let mut expected = pool;
            expected.account_tag = Some(ServiceAccountTags::Many(Vec::new()));
            expected.account_ids = vec!["copilot-team-a/selected".into()];
            assert_eq!(narrowed, &expected, "retain every other service constraint");
            assert_eq!(retry.trace_id, original.trace_id);
            assert_eq!(retry.service_id, original.service_id);
            assert_eq!(retry.api_key_id, original.api_key_id);
            assert_eq!(retry.model, original.model);
            assert_eq!(retry.original_model, original.original_model);
            let body: Value = serde_json::from_slice(&retry.body).unwrap();
            assert!(body.get("output_config").is_none());
        }
    }

    #[tokio::test]
    async fn effort_recovery_can_pin_an_account_selected_by_its_service_tag() {
        let probe = EffortProbe::new("copilot", Some("tagged-account"));
        let adapter: Arc<dyn ProviderAdapter> = probe.clone();
        let pool = ProxyServiceConfig {
            allowed_providers: vec!["copilot".into()],
            account_tag: Some(ServiceAccountTags::One("team-a".into())),
            excluded_account_ids: vec!["copilot/forbidden".into()],
            ..Default::default()
        };
        execute_with_effort_recovery(&adapter, effort_request(Some(pool)), false)
            .await
            .unwrap();
        let requests = probe.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let retry = requests[1].account_pool.as_ref().unwrap();
        assert!(retry.includes_provider_account("copilot", "tagged-account", &[]));
        assert!(!retry.includes_provider_account("copilot", "other-tagged", &["team-a".into()]));
        assert!(!retry.includes_provider_account("copilot", "forbidden", &["team-a".into()]));
    }

    #[tokio::test]
    async fn effort_recovery_does_not_override_exclusions_or_frozen_provider_scope() {
        for pool in [
            ProxyServiceConfig {
                allowed_providers: vec!["copilot".into()],
                excluded_account_ids: vec!["copilot/selected".into()],
                ..Default::default()
            },
            ProxyServiceConfig {
                allowed_providers: vec!["copilot".into()],
                account_tag: Some(ServiceAccountTags::One("team-a".into())),
                excluded_account_ids: vec!["copilot/selected".into()],
                ..Default::default()
            },
            ProxyServiceConfig {
                allowed_providers: vec!["another-provider".into()],
                ..Default::default()
            },
            ProxyServiceConfig {
                allowed_providers: vec!["copilot".into()],
                account_tag: Some(ServiceAccountTags::Many(Vec::new())),
                account_ids: vec!["copilot/other-account".into()],
                ..Default::default()
            },
        ] {
            let probe = EffortProbe::new("copilot", Some("selected"));
            let adapter: Arc<dyn ProviderAdapter> = probe.clone();
            let response =
                execute_with_effort_recovery(&adapter, effort_request(Some(pool)), false)
                    .await
                    .unwrap();
            assert_eq!(probe.requests.lock().unwrap().len(), 1);
            let ProviderResponseBody::Full(body) = response.body else {
                panic!("original rejection must remain available");
            };
            assert_eq!(body, Bytes::from_static(EFFORT_REJECTION));
        }
    }

    #[tokio::test]
    async fn effort_recovery_preserves_ccr_counting_and_missing_account_behavior() {
        for (pool, account_id, count_tokens, protocol) in [
            (
                Some(ProxyServiceConfig {
                    crs_relay: Some(CrsRelayMode::Ccr),
                    ..Default::default()
                }),
                Some("selected"),
                false,
                ProviderProtocol::ClaudeMessages,
            ),
            (
                Some(ProxyServiceConfig::default()),
                Some("selected"),
                true,
                ProviderProtocol::ClaudeMessages,
            ),
            (
                Some(ProxyServiceConfig::default()),
                None,
                false,
                ProviderProtocol::ClaudeMessages,
            ),
            (
                None,
                Some("selected"),
                false,
                ProviderProtocol::ClaudeMessages,
            ),
            (
                Some(ProxyServiceConfig::default()),
                Some("selected"),
                false,
                ProviderProtocol::OpenAiChat,
            ),
        ] {
            let probe = EffortProbe::new("kiro", account_id);
            let adapter: Arc<dyn ProviderAdapter> = probe.clone();
            let mut request = effort_request(pool);
            request.protocol = protocol;
            execute_with_effort_recovery(&adapter, request, count_tokens)
                .await
                .unwrap();
            assert_eq!(
                probe.requests.lock().unwrap().len(),
                usize::from(!count_tokens)
            );
            assert_eq!(
                probe.count_requests.lock().unwrap().len(),
                usize::from(count_tokens)
            );
        }
    }

    #[test]
    fn only_explicit_effort_rejection_is_repaired() {
        let body = br#"{"output_config":{"effort":"high","format":{"type":"json_object"}},"thinking":{"type":"adaptive"},"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"kept","signature":"kept"}]}]}"#;
        let repaired = without_rejected_effort(
            body,
            StatusCode::BAD_REQUEST,
            "output_config.effort: Extra inputs are not permitted",
        )
        .unwrap();
        let original: Value = serde_json::from_slice(body).unwrap();
        let repaired: Value = serde_json::from_slice(&repaired).unwrap();
        assert!(repaired["output_config"].get("effort").is_none());
        assert_eq!(
            repaired["output_config"]["format"],
            original["output_config"]["format"]
        );
        assert_eq!(repaired["messages"], original["messages"]);
        assert_eq!(repaired["thinking"], original["thinking"]);
        for (status, message) in [
            (400, "thinking signature is unsupported"),
            (400, "invalid output_config.effort budget"),
            (429, "output_config.effort unsupported"),
        ] {
            assert!(
                without_rejected_effort(body, StatusCode::from_u16(status).unwrap(), message)
                    .is_none()
            );
        }
    }
}
