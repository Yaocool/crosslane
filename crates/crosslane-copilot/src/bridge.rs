use bytes::Bytes;
use crosslane_core::provider::ProviderProtocol;
use crosslane_provider::{
    ProviderError, ProviderErrorKind, ProviderResponse, ProviderResponseBody,
};
use crosslane_translate::bridge::{translate_error, translate_response, StreamBridge, ToolNames};
use futures::StreamExt;
use http::{HeaderValue, StatusCode};

pub(super) async fn adapt_response(
    mut response: ProviderResponse,
    from: ProviderProtocol,
    to: ProviderProtocol,
    streaming: bool,
    model: &str,
    tool_names: ToolNames,
) -> Result<ProviderResponse, ProviderError> {
    let is_sse = response
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"));
    let mut upstream = match response.body {
        ProviderResponseBody::Stream(stream) => stream,
        ProviderResponseBody::Full(bytes) => Box::pin(futures::stream::once(async { Ok(bytes) })),
    };
    response.headers.remove("content-length");
    response.headers.remove("content-encoding");
    if streaming && response.status.is_success() && is_sse {
        let mut bridge = StreamBridge::new(from, to, model).with_tool_names(tool_names);
        let stream = async_stream::stream! {
            let mut failed = false;
            while let Some(chunk) = upstream.next().await {
                let frames = match chunk {
                    Ok(bytes) => bridge.push(&bytes).map_err(|error| error.to_string()),
                    Err(error) => Err(error.to_string()),
                };
                match frames {
                    Ok(frames) => for frame in frames { yield Ok(Bytes::from(frame)); },
                    Err(error) => {
                        yield Ok(Bytes::from(bridge.error(&error)));
                        yield Err(upstream_error(error));
                        failed = true;
                        break;
                    }
                }
            }
            if !failed {
                match bridge.finish() {
                    Ok(frames) => for frame in frames { yield Ok(Bytes::from(frame)); },
                    Err(error) => {
                        yield Ok(Bytes::from(bridge.error(&error.to_string())));
                        yield Err(upstream_error(error.to_string()));
                    },
                }
            }
        };
        response.body = ProviderResponseBody::Stream(Box::pin(stream));
    } else {
        let mut buffer = Vec::new();
        while let Some(chunk) = upstream.next().await {
            let chunk = chunk?;
            if buffer.len() + chunk.len() > 64 * 1024 * 1024 {
                return Err(upstream_error(
                    "upstream JSON response exceeds the adapter limit",
                ));
            }
            buffer.extend_from_slice(&chunk);
        }
        let mut value: serde_json::Value = match serde_json::from_slice(&buffer) {
            Ok(value) => value,
            Err(_) if !response.status.is_success() => serde_json::json!({
                "error":{"message":format!("Copilot upstream returned HTTP {}",response.status.as_u16()),"type":"api_error"}
            }),
            Err(_) => return Err(upstream_error("upstream returned invalid JSON")),
        };
        if response.status.is_success() && value.is_object() && value.get("model").is_none() {
            value["model"] = serde_json::json!(model);
        }
        let mut value = if response.status.is_success() {
            translate_response(&value, from, to)
                .map_err(|error| upstream_error(error.to_string()))?
        } else {
            translate_error(&value, to)
        };
        if to == ProviderProtocol::OpenAiResponses {
            tool_names.restore(&mut value);
        }
        response
            .headers
            .insert("content-type", HeaderValue::from_static("application/json"));
        response.body = ProviderResponseBody::Full(Bytes::from(value.to_string()));
    }
    Ok(response)
}

fn upstream_error(message: impl Into<String>) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Upstream,
        StatusCode::BAD_GATEWAY,
        message,
    )
}
