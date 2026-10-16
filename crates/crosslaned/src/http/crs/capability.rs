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
    pool.account_tag = None;
    pool.account_ids = vec![account_id.clone()];
    pool.excluded_account_ids.clear();
    tracing::info!(event = "crs.capability.recovery", trace_id = %retry.trace_id, service_id = %retry.service_id, model = %retry.model, account_id, field = "output_config.effort", "retrying explicit unsupported effort once before output");
    drop(response);
    adapter.execute(retry).await
}

#[cfg(test)]
mod tests {
    use super::*;

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
