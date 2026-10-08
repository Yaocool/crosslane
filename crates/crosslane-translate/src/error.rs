//! Client-facing error normalization. Never expose upstream account-ban triggers.

use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorFormat {
    Claude,
    OpenAi,
}

// Claude Code's client-side risk system treats these upstream phrases as an
// account-ban signal. They must never reach a client, even when they appear in
// an otherwise useful quota/context error. Keep this list synchronized with
// `src/main/proxy/proxy-utils.ts` in Kiro-account-manager.
const BAN_TRIGGER_PHRASES: &[&str] = &[
    "this organization has been disabled",
    "oauth authentication is currently not allowed",
    "account disabled",
    "account is disabled",
    "invalid account",
    "too many active sessions",
];

/// Redacts headless credentials without changing error-classification prose.
/// Shared with upstream error formatting so background logs are safe as well.
pub fn redact_kiro_keys(message: &str) -> std::borrow::Cow<'_, str> {
    static KIRO_KEY: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    KIRO_KEY
        .get_or_init(|| {
            regex::Regex::new(r#"(?i)ksk_[^\s"'<>\x60,;()\[\]{}]+"#).expect("Kiro key pattern")
        })
        .replace_all(message, "[REDACTED]")
}

pub fn sanitize_error_message(message: &str) -> String {
    let normalized = normalize_error_message(message);
    let mut chars = normalized.chars();
    let mut bounded = chars
        .by_ref()
        .take(1_024)
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>();
    if chars.next().is_some() {
        bounded.push('…');
    }
    bounded
}

/// Invalid model hints may actually contain pasted prompts or credentials.
/// Replace the entire value before truncation; never retain a prompt prefix.
pub fn log_model(model: &str) -> String {
    if model.is_empty() {
        return String::new();
    }
    if crate::validate::validate_model(model).is_err() {
        return "[invalid-model]".into();
    }
    redact_kiro_keys(model).into_owned()
}

fn normalize_error_message(message: &str) -> String {
    // Quota/context branches intentionally preserve useful diagnostics. Strip
    // headless credentials before any branch can reflect upstream prose.
    let message = redact_kiro_keys(message);
    let message = message.as_ref();
    let lower = message.to_ascii_lowercase();
    // This is deliberately the first and unconditional classification. Moving
    // it below quota/auth/context handling can leak a CRS trigger through a
    // more specific branch.
    if BAN_TRIGGER_PHRASES
        .iter()
        .any(|phrase| lower.contains(phrase))
    {
        return "service unavailable".into();
    }
    if lower.contains("prompt is too long")
        || lower.contains("context length exceeded")
        || lower.contains("input is too long")
        || lower.contains("context window") && lower.contains("exceed")
    {
        if let Some((actual, maximum)) = context_token_counts(message, &lower) {
            // Claude Code recognizes this stable shape and can explain how far
            // the request exceeded the context window. A locally formatted,
            // strictly validated model identifier is safe to retain as well;
            // arbitrary upstream prose is never reflected.
            if let Some(model) = context_model_name(message, &lower) {
                // The sanitizer is also called for arbitrary upstream text.
                // Do not let the trusted local context shape bypass the
                // endpoint redaction that normally runs at the end.
                if redact_endpoint_names(model) == model {
                    return format!(
                        "prompt is too long for model '{model}': {actual} tokens > {maximum}"
                    );
                }
            }
            return format!("prompt is too long: {actual} tokens > {maximum}");
        }
        return "prompt is too long: context length exceeded".into();
    }
    if is_quota_error(&lower) {
        if lower.contains("throttlingexception") {
            let suffix = message
                .split_once("ThrottlingException")
                .map(|(_, rest)| rest)
                .unwrap_or("");
            return redact_endpoint_names(&format!("ThrottlingException{suffix}"));
        }
        return redact_endpoint_names(message);
    }
    if lower.contains("401") || lower.contains("403") || lower.contains("auth") {
        return "Service temporarily unavailable, please retry".into();
    }
    if lower.contains("internal server error")
        || lower.contains("bad gateway")
        || lower.contains("502")
        || lower.contains("500")
        || lower.contains("503")
        || lower.contains("504")
        || lower.contains("service unavailable")
        || lower.contains("gateway timeout")
    {
        return "Upstream service error, please retry later".into();
    }
    if is_network_error(&lower) {
        return "Service temporarily unavailable, please retry".into();
    }
    redact_endpoint_names(message)
}

fn context_model_name<'a>(message: &'a str, lower: &str) -> Option<&'a str> {
    const MARKERS: [&str; 2] = [
        "prompt is too long for resolved model '",
        "prompt is too long for model '",
    ];
    for marker in MARKERS {
        let Some(offset) = lower.find(marker) else {
            continue;
        };
        let start = offset + marker.len();
        let end = start + message[start..].find('\'')?;
        let model = &message[start..end];
        if !model.is_empty()
            && model.len() <= 128
            && model
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        {
            return Some(model);
        }
    }
    None
}

fn context_token_counts(message: &str, lower: &str) -> Option<(u64, u64)> {
    if !(lower.contains("tokens >")
        || lower.contains("tokens exceeds")
        || lower.contains("input tokens exceed"))
    {
        return None;
    }
    let numbers = message
        .split(|character: char| !character.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u64>().ok())
        .collect::<Vec<_>>();
    let maximum = *numbers.last()?;
    let actual = *numbers.get(numbers.len().checked_sub(2)?)?;
    Some((actual, maximum))
}

fn is_quota_error(lower: &str) -> bool {
    [
        "402",
        "429",
        "quota",
        "credit",
        "throttlingexception",
        "reached the limit",
        "payment required",
    ]
    .iter()
    .any(|word| lower.contains(word))
}

fn is_network_error(lower: &str) -> bool {
    [
        "network",
        "fetch failed",
        "socket",
        "econnreset",
        "econnrefused",
        "etimedout",
        "timeout",
        "connection reset",
        "premature close",
        "other side closed",
        "terminated",
        "operation was aborted",
    ]
    .iter()
    .any(|word| lower.contains(word))
}

fn redact_endpoint_names(message: &str) -> String {
    static ENDPOINT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let filtered = ENDPOINT.get_or_init(|| {
        regex::Regex::new(r"(?i)(?:(?:codewhisperer|q)\.[a-z0-9-]+\.amazonaws\.com|(?:runtime|management)\.[a-z0-9-]+\.kiro\.dev)\S*")
            .expect("regional Kiro endpoint pattern")
    }).replace_all(message, "upstream");
    let filtered = replace_ascii_case_insensitive(&filtered, "CodeWhisperer", "endpoint");
    replace_ascii_case_insensitive(&filtered, "AmazonQ", "endpoint")
}

fn replace_ascii_case_insensitive(message: &str, needle: &str, replacement: &str) -> String {
    let lower = message.to_ascii_lowercase();
    let needle = needle.to_ascii_lowercase();
    let mut output = String::with_capacity(message.len());
    let mut cursor = 0;
    while let Some(relative) = lower[cursor..].find(&needle) {
        let start = cursor + relative;
        output.push_str(&message[cursor..start]);
        output.push_str(replacement);
        cursor = start + needle.len();
    }
    output.push_str(&message[cursor..]);
    output
}

pub fn error_envelope(
    format: ErrorFormat,
    status: u16,
    message: &str,
    request_id: Option<&str>,
) -> Value {
    let safe = sanitize_error_message(message);
    let mut envelope = match format {
        ErrorFormat::Claude => json!({
            "type": "error",
            "error": {"type": claude_error_type(status), "message": safe}
        }),
        ErrorFormat::OpenAi => json!({
            "error": {"message": safe, "type": openai_error_type(status), "code": Value::Null}
        }),
    };
    if let Some(request_id) = request_id.filter(|request_id| !request_id.is_empty()) {
        envelope["request_id"] = json!(request_id);
    }
    envelope
}

fn claude_error_type(status: u16) -> &'static str {
    match status {
        400 | 405 | 422 => "invalid_request_error",
        401 | 403 => "authentication_error",
        404 => "not_found_error",
        408 | 504 => "timeout_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        500 | 502 => "api_error",
        _ => "overloaded_error",
    }
}

fn openai_error_type(status: u16) -> &'static str {
    match status {
        400 | 405 | 413 | 422 => "invalid_request_error",
        401 | 403 => "authentication_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        _ => "server_error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_logs_discard_invalid_values_and_redact_keys() {
        for model in [
            "SELECT secret\nFROM private",
            "claude\u{001b}[31m",
            &"x".repeat(257),
        ] {
            assert_eq!(log_model(model), "[invalid-model]");
        }
        assert_eq!(log_model("ksk_test-credential"), "[REDACTED]");
        assert_eq!(
            log_model("anthropic/claude:latest[1m]"),
            "anthropic/claude:latest[1m]"
        );
    }

    #[test]
    fn error_logs_are_bounded_without_splitting_utf8_or_retaining_control_characters() {
        let safe = sanitize_error_message(&format!("reason\n{} ksk_hidden", "消息".repeat(2_000)));
        assert_eq!(safe.chars().count(), 1_025);
        assert!(safe.ends_with('…'));
        assert!(!safe.chars().any(char::is_control));
        assert!(!safe.contains("ksk_hidden"));
    }

    #[test]
    fn ports_quota_cases_from_typescript() {
        assert_eq!(
            sanitize_error_message("429 Quota exhausted on CodeWhisperer"),
            "429 Quota exhausted on endpoint"
        );
        assert_eq!(
            sanitize_error_message("429 on CodeWhisperer: ThrottlingException: rate exceeded"),
            "ThrottlingException: rate exceeded"
        );
    }

    #[test]
    fn ports_auth_cases_from_typescript() {
        for text in ["401 Unauthorized", "Auth error 403"] {
            assert_eq!(
                sanitize_error_message(text),
                "Service temporarily unavailable, please retry"
            );
        }
    }

    #[test]
    fn ports_server_cases_from_typescript() {
        for text in [
            "500 Internal Server Error",
            "Internal Server Error",
            "502 Bad Gateway",
        ] {
            assert_eq!(
                sanitize_error_message(text),
                "Upstream service error, please retry later"
            );
        }
    }

    #[test]
    fn ports_network_cases_from_typescript() {
        for text in [
            "Network connection failed",
            "fetch failed",
            "SocketError: other side closed",
            "ECONNRESET",
        ] {
            assert_eq!(
                sanitize_error_message(text),
                "Service temporarily unavailable, please retry"
            );
        }
    }

    #[test]
    fn ports_context_length_case_from_typescript() {
        assert_eq!(
            sanitize_error_message(
                "API error 400: Service temporarily unavailable: prompt is too long; context length exceeded"
            ),
            "prompt is too long: context length exceeded"
        );
        assert_eq!(
            sanitize_error_message(
                "prompt is too long for resolved model claude-sonnet-4.6: 196000 input tokens exceeds 190000"
            ),
            "prompt is too long: 196000 tokens > 190000"
        );
        assert_eq!(
            sanitize_error_message(
                "prompt is too long for resolved model 'claude-sonnet-4.6': 196000 input tokens exceeds 190000"
            ),
            "prompt is too long for model 'claude-sonnet-4.6': 196000 tokens > 190000"
        );
        assert_eq!(
            sanitize_error_message(
                "prompt is too long for model 'bad model<script>': 196000 input tokens exceeds 190000"
            ),
            "prompt is too long: 196000 tokens > 190000"
        );
        assert_eq!(
            sanitize_error_message(
                "prompt is too long for model 'q.us-east-1.amazonaws.com/private': 196000 input tokens exceeds 190000"
            ),
            "prompt is too long: 196000 tokens > 190000"
        );
        assert_eq!(
            sanitize_error_message(
                "prompt is too long for model 'provider/private-model': 196000 input tokens exceeds 190000"
            ),
            "prompt is too long: 196000 tokens > 190000"
        );
    }

    #[test]
    fn claude_error_envelopes_preserve_request_ids_and_protocol_types() {
        let context = error_envelope(
            ErrorFormat::Claude,
            400,
            "prompt is too long: 196000 tokens > 190000",
            Some("trace_context"),
        );
        assert_eq!(context["error"]["type"], "invalid_request_error");
        assert_eq!(
            context["error"]["message"],
            "prompt is too long: 196000 tokens > 190000"
        );
        assert_eq!(context["request_id"], "trace_context");

        let too_large = error_envelope(
            ErrorFormat::Claude,
            413,
            "request body exceeds the limit",
            None,
        );
        assert_eq!(too_large["error"]["type"], "request_too_large");

        let upstream = error_envelope(ErrorFormat::Claude, 502, "Internal Server Error", None);
        assert_eq!(upstream["error"]["type"], "api_error");
        assert_eq!(
            upstream["error"]["message"],
            "Upstream service error, please retry later"
        );
    }

    #[test]
    fn ports_every_crs_blocking_case_from_typescript() {
        for text in [
            "Error: this organization has been disabled by admin",
            "OAuth authentication is currently not allowed",
            "API error 400: account disabled",
            "API error 400: account is disabled",
            "API error 400: invalid account",
            "API error 400: too many active sessions",
            "429 quota exhausted because account is disabled",
        ] {
            assert_eq!(sanitize_error_message(text), "service unavailable");
        }
    }

    #[test]
    fn ports_endpoint_redaction_cases_from_typescript() {
        assert_eq!(
            sanitize_error_message("CodeWhisperer request failed"),
            "endpoint request failed"
        );
        assert_eq!(
            sanitize_error_message("AmazonQ request failed"),
            "endpoint request failed"
        );
        assert_eq!(
            sanitize_error_message(
                "request to codewhisperer.us-east-1.amazonaws.com/generateAssistantResponse failed"
            ),
            "request to upstream failed"
        );
        assert_eq!(
            sanitize_error_message("request to q.us-east-1.amazonaws.com/private?token=x failed"),
            "request to upstream failed"
        );
        assert_eq!(
            sanitize_error_message("AMAZONQ and CODEWHISPERER failed"),
            "endpoint and endpoint failed"
        );
    }

    #[test]
    fn kiro_api_keys_and_regional_endpoints_are_redacted_before_classification() {
        for message in [
            "quota exhausted for ksk_synthetic-secret_123",
            "invalid request: key=ksk_synthetic-secret_123",
            "ThrottlingException: ksk_synthetic-secret_123 reached the limit",
            "prompt is too long for model 'ksk_synthetic-secret_123': 200 tokens > 100",
        ] {
            let safe = sanitize_error_message(message);
            assert!(!safe.contains("ksk_synthetic"), "{safe}");
            assert!(!safe.contains("secret_123"), "{safe}");
        }
        for host in [
            "runtime.eu-central-1.kiro.dev",
            "management.us-gov-west-1.kiro.dev",
            "q.eu-central-1.amazonaws.com",
            "codewhisperer.us-gov-east-1.amazonaws.com",
        ] {
            assert_eq!(
                sanitize_error_message(&format!("request to {host}/private?token=secret failed")),
                "request to upstream failed"
            );
        }
    }
}
