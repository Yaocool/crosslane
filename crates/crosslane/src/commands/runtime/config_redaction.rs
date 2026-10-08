//! Safe, comment-free rendering for `config show`.

use anyhow::{Context, Result};
use crosslane_ipc::protocol::ConfigShowResult;

pub(super) fn redact(show: &mut ConfigShowResult) -> Result<()> {
    redact_raw(&mut show.raw)?;
    redact_effective(&mut show.effective_json);
    Ok(())
}

pub(super) fn redact_raw(raw: &mut String) -> Result<()> {
    if !raw.trim().is_empty() {
        let mut value: toml::Value = raw.parse().context(
            "配置文件无法安全解析；请运行 config validate，或显式使用 --show-secrets 查看原文",
        )?;
        redact_toml(&mut value, None);
        // Re-rendering drops comments, which may themselves contain credentials.
        *raw = toml::to_string_pretty(&value)?;
    }
    Ok(())
}

pub(super) fn redact_effective(value: &mut serde_json::Value) {
    redact_json(value, None);
}

fn sensitive_key(key: &str, parent: Option<&str>) -> bool {
    let key = key.to_ascii_lowercase().replace('-', "_");
    const PUBLIC_PROVIDER_SETTINGS: &[&str] = &[
        "client_id",
        "github_host",
        "github_api_base",
        "oauth_base",
        "api_token_refresh_before_secs",
        "api_endpoint_fallback",
        "allowed_endpoint_hosts",
        "allow_insecure_http",
        "editor_version",
        "editor_plugin_version",
        "integration_id",
        "user_agent",
    ];
    if parent == Some("settings") && !PUBLIC_PROVIDER_SETTINGS.contains(&key.as_str()) {
        return true;
    }
    key == "key"
        || (key == "api_key" && parent.is_some())
        || key == "token"
        || key == "authorization"
        || key == "cookie"
        || key == "dingtalk_sign"
        || key == "webhook_url"
        || key.ends_with("_token")
        || key.ends_with("_api_key")
        || key.contains("secret")
        || key.contains("password")
        || key.contains("credential")
        || (key == "url" && parent == Some("webhook"))
}

fn sensitive_url(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.starts_with("http")
        && [
            "token=",
            "api_key=",
            "secret=",
            "signature=",
            "password=",
            "code=",
        ]
        .iter()
        .any(|parameter| lower.contains(parameter))
}

fn redact_toml(value: &mut toml::Value, parent: Option<&str>) {
    match value {
        toml::Value::Table(table) => {
            for (key, child) in table {
                if sensitive_key(key, parent) {
                    *child = toml::Value::String("<redacted>".into());
                } else {
                    redact_toml(child, Some(key));
                }
            }
        }
        toml::Value::Array(values) => {
            for child in values {
                redact_toml(child, parent);
            }
        }
        toml::Value::String(text) if sensitive_url(text) => {
            *text = "<redacted>".into();
        }
        _ => {}
    }
}

fn redact_json(value: &mut serde_json::Value, parent: Option<&str>) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, child) in object {
                if sensitive_key(key, parent) {
                    *child = serde_json::Value::String("<redacted>".into());
                } else {
                    redact_json(child, Some(key));
                }
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                redact_json(child, parent);
            }
        }
        serde_json::Value::String(text) if sensitive_url(text) => {
            *text = "<redacted>".into();
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_credentials_in_raw_and_effective_config_without_leaking_comments() {
        let raw = r#"
# old token=comment-secret
[[api_key]]
id = "ak_public"
key = "sk-secret"
[[webhook]]
url = "https://example.test/hook?access_token=url-secret"
dingtalk_sign = "sign-secret"
[[provider]]
id = "copilot"
[provider.settings]
client_id = "public-client-id"
client_secret = "oauth-secret"
personal_access = "unknown-secret"
"#;
        let mut show = ConfigShowResult {
            path: "/tmp/config.toml".into(),
            raw: raw.into(),
            effective_json: serde_json::json!({
                "api_key":[{"id":"ak_public","key":"sk-secret"}],
                "webhook":[{"url":"https://example.test/hook?access_token=url-secret"}],
                "provider":[{"settings":{"client_id":"public-client-id","client_secret":"oauth-secret","personal_access":"unknown-secret"}}]
            }),
        };
        redact(&mut show).unwrap();
        let output = serde_json::to_string(&show).unwrap();
        for secret in [
            "comment-secret",
            "sk-secret",
            "url-secret",
            "sign-secret",
            "oauth-secret",
            "unknown-secret",
        ] {
            assert!(!output.contains(secret), "{output}");
        }
        assert!(output.contains("<redacted>"));
        assert!(output.contains("public-client-id"));
        assert!(output.contains("ak_public"));
    }
}
