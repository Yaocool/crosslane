//! AWS credentials are changed only through the owner-only Unix socket.
use std::sync::Arc;

use anyhow::{anyhow, Result};
use crosslane_core::{kiro_admin::KiroAdminCredentials, paths::Paths};
use crosslane_ipc::protocol::{KiroAdminCredentialsView, RpcError};
use crosslane_kiro::enterprise::{validate_region, AwsCredentials};
use crosslane_store::kiro_admin::{CredentialStore, StoredCredentials, WriteStatus};

use super::{to_value, Handled};
use crate::state::AppState;

pub(crate) fn environment_with(
    read: &impl Fn(&str) -> Result<Option<String>>,
) -> Result<KiroAdminCredentials> {
    let optional =
        |name| -> Result<Option<String>> { Ok(read(name)?.filter(|value| !value.is_empty())) };
    Ok(KiroAdminCredentials {
        access_key_id: read("CROSSLANE_KIRO_ADMIN_ACCESS_KEY_ID")?.unwrap_or_default(),
        secret_access_key: read("CROSSLANE_KIRO_ADMIN_SECRET_ACCESS_KEY")?.unwrap_or_default(),
        session_token: optional("CROSSLANE_KIRO_ADMIN_SESSION_TOKEN")?,
        region: optional("CROSSLANE_KIRO_ADMIN_REGION")?.unwrap_or_else(|| "us-east-1".into()),
        profile_arn: optional("CROSSLANE_KIRO_ADMIN_PROFILE_ARN")?,
        application_arn: optional("CROSSLANE_KIRO_ADMIN_APPLICATION_ARN")?,
        identity_center_region: optional("CROSSLANE_KIRO_ADMIN_IDENTITY_CENTER_REGION")?,
    })
}

pub(crate) async fn effective(
    paths: &Paths,
) -> Result<(Option<KiroAdminCredentials>, &'static str)> {
    match CredentialStore::new(&paths.data_dir).load().await? {
        Some(StoredCredentials::Configured(settings)) => Ok((Some(*settings), "encrypted_file")),
        Some(StoredCredentials::Disabled) => Ok((None, "disabled")),
        None | Some(StoredCredentials::Environment) => {
            let settings = environment()?;
            let source =
                if settings.access_key_id.is_empty() && settings.secret_access_key.is_empty() {
                    "none"
                } else {
                    "environment"
                };
            Ok((Some(settings), source))
        }
    }
}

fn environment() -> Result<KiroAdminCredentials> {
    environment_with(&|name| match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => Err(anyhow!(
            "Kiro credential environment contains invalid encoding"
        )),
    })
}

pub(crate) fn aws_credentials(settings: &KiroAdminCredentials) -> Result<AwsCredentials> {
    AwsCredentials::new(settings.access_key_id.clone(), settings.secret_access_key.clone(), settings.session_token.clone())
        .map_err(|_| anyhow!("complete AWS access key, secret key and, for STS credentials, session token are required"))
}

fn validate(settings: &KiroAdminCredentials) -> Result<()> {
    let _ = aws_credentials(settings)?;
    validate_region(&settings.region)
        .map_err(|_| anyhow!("a valid commercial AWS region is required"))?;
    if let Some(region) = &settings.identity_center_region {
        validate_region(region)
            .map_err(|_| anyhow!("a valid Identity Center region is required"))?;
    }
    if let Some(value) = &settings.profile_arn {
        let parts = arn_parts(value, "codewhisperer", 2048)
            .filter(|parts| {
                parts[5]
                    .strip_prefix("profile/")
                    .is_some_and(profile_identifier)
            })
            .ok_or_else(|| anyhow!("a valid CodeWhisperer profile ARN is required"))?;
        validate_region(parts[3])
            .map_err(|_| anyhow!("profile ARN must contain a commercial AWS region"))?;
    }
    if let Some(value) = &settings.application_arn {
        let valid = arn_parts(value, "sso", 1224).is_some_and(|parts| {
            if !parts[3].is_empty() {
                return false;
            }
            let resource: Vec<_> = parts[5].split('/').collect();
            if resource.len() != 3 || resource[0] != "application" {
                return false;
            }
            let instance = resource[1]
                .strip_prefix("ssoins-")
                .or_else(|| resource[1].strip_prefix("ins-"));
            instance.is_some_and(|id| {
                id.len() == 16
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'))
            }) && resource[2]
                .strip_prefix("apl-")
                .is_some_and(|id| id.len() == 16 && id.bytes().all(|b| b.is_ascii_alphanumeric()))
        });
        if !valid {
            return Err(anyhow!(
                "a valid IAM Identity Center application ARN is required"
            ));
        }
    }
    Ok(())
}

fn profile_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn arn_parts<'a>(value: &'a str, service: &str, maximum: usize) -> Option<[&'a str; 6]> {
    if value.len() > maximum || !value.bytes().all(|b| b.is_ascii_graphic()) {
        return None;
    }
    let parts: [&str; 6] = value.splitn(6, ':').collect::<Vec<_>>().try_into().ok()?;
    (parts[0] == "arn"
        && parts[1] == "aws"
        && parts[2] == service
        && parts[4].len() == 12
        && parts[4].bytes().all(|b| b.is_ascii_digit()))
    .then_some(parts)
}

fn metadata(
    paths: &Paths,
    settings: Option<&KiroAdminCredentials>,
    source: &str,
    configured: bool,
) -> KiroAdminCredentialsView {
    let visible = settings.filter(|_| configured);
    KiroAdminCredentialsView {
        configured,
        source: source.into(),
        credentials_file: CredentialStore::new(&paths.data_dir)
            .path()
            .display()
            .to_string(),
        region: visible.map(|value| value.region.clone()),
        has_session_token: settings.is_some_and(|value| value.session_token.is_some()),
        profile_arn: visible.and_then(|value| value.profile_arn.clone()),
        application_arn: visible.and_then(|value| value.application_arn.clone()),
        identity_center_region: visible.and_then(|value| value.identity_center_region.clone()),
        persistence_warning: None,
    }
}

fn committed(mut view: KiroAdminCredentialsView, status: WriteStatus) -> Handled {
    if !status.durable {
        view.persistence_warning = Some("Kiro 管理凭证修改已热生效，但磁盘目录同步失败，重启后的持久性无法确认；请检查存储后重试本次操作。".into());
        tracing::warn!(event = "admin.kiro.credentials.durability_unconfirmed", source = %view.source,
            "credential change is active but durable directory sync was not confirmed");
    }
    to_value(view)
}

fn storage_error(_: anyhow::Error) -> RpcError {
    RpcError::internal("cannot access private Kiro credentials; check daemon ownership, directory mode 0700, file mode 0600 and absence of additional macOS ACL grants; corrupt records never fall back to environment credentials")
}

pub(super) async fn show(state: &Arc<AppState>) -> Handled {
    let (settings, source) = effective(&state.paths).await.map_err(storage_error)?;
    let configured = settings
        .as_ref()
        .is_some_and(|settings| validate(settings).is_ok());
    to_value(metadata(
        &state.paths,
        settings.as_ref(),
        source,
        configured,
    ))
}

pub(super) async fn configure(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    // Serde's diagnostic can quote input values. Never return it for secrets.
    let mut settings: KiroAdminCredentials = serde_json::from_value(params)
        .map_err(|_| RpcError::bad_params("invalid Kiro credential JSON; access_key_id and secret_access_key are required; unknown fields are rejected"))?;
    settings.session_token = settings
        .session_token
        .take()
        .filter(|value| !value.is_empty());
    validate(&settings).map_err(|error| RpcError::bad_params(error.to_string()))?;
    let result = metadata(&state.paths, Some(&settings), "encrypted_file", true);
    let status = CredentialStore::new(&state.paths.data_dir)
        .save(StoredCredentials::Configured(Box::new(settings)))
        .await
        .map_err(storage_error)?;
    tracing::info!(
        event = "admin.kiro.credentials.updated",
        source = "encrypted_file",
        "Kiro management credentials updated"
    );
    committed(result, status)
}

pub(super) async fn clear(state: &Arc<AppState>) -> Handled {
    let status = CredentialStore::new(&state.paths.data_dir)
        .save(StoredCredentials::Disabled)
        .await
        .map_err(storage_error)?;
    tracing::info!(
        event = "admin.kiro.credentials.cleared",
        "Kiro management credentials disabled"
    );
    committed(metadata(&state.paths, None, "disabled", false), status)
}

pub(super) async fn use_environment(state: &Arc<AppState>) -> Handled {
    let settings = environment().map_err(|_| {
        RpcError::bad_params(
            "Kiro credential environment has invalid encoding; stored credentials were retained",
        )
    })?;
    validate(&settings).map_err(|_| RpcError::bad_params("Kiro credential environment is incomplete or invalid; stored credentials were retained"))?;
    let result = metadata(&state.paths, Some(&settings), "environment", true);
    let status = CredentialStore::new(&state.paths.data_dir)
        .use_environment()
        .await
        .map_err(storage_error)?;
    tracing::info!(
        event = "admin.kiro.credentials.environment",
        "Kiro management credentials restored to environment source"
    );
    committed(result, status)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> KiroAdminCredentials {
        serde_json::from_value(serde_json::json!({
            "access_key_id":"AKIA000000000EXAMPLE", "secret_access_key":"fixture-private-secret",
            "profile_arn":"arn:aws:codewhisperer:us-east-1:123456789012:profile/fixture",
            "application_arn":"arn:aws:sso::123456789012:application/ssoins-1111222233334444/apl-1111222233334444"
        })).unwrap()
    }

    #[test]
    fn configured_scope_rejects_malformed_or_wrong_resource_arns_without_echoing_them() {
        validate(&settings()).unwrap();
        for (application, value) in [
            (false, "arn:aws:"),
            (false, "arn:aws:sns:us-east-1:123456789012:profile/fixture"),
            (false, "arn:aws:codewhisperer:us-east-1:not-an-account:profile/fixture"),
            (false, "arn:aws:codewhisperer:us-east-1:123456789012:profile/private fixture"),
            (false, "arn:aws:codewhisperer:invalid:123456789012:profile/fixture"),
            (true, "arn:aws:codewhisperer:us-east-1:123456789012:profile/fixture"),
            (true, "arn:aws:sso:us-east-1:123456789012:application/ssoins-1111222233334444/apl-1111222233334444"),
            (true, "arn:aws:sso::123456789012:application/ssoins-short/apl-short"),
        ] {
            let mut input = settings();
            if application { input.application_arn = Some(value.into()); }
            else { input.profile_arn = Some(value.into()); }
            let error = validate(&input).unwrap_err().to_string();
            assert!(!error.contains(value));
            assert!(!error.contains("fixture-private-secret"));
        }
    }

    #[test]
    fn publication_warning_describes_the_submitted_snapshot_without_credentials() {
        let paths = Paths::from_env_values(Some("/fixture"), None, None, None);
        let input = settings();
        let snapshot = metadata(&paths, Some(&input), "encrypted_file", true);
        let result = committed(snapshot, WriteStatus { durable: false }).unwrap();
        assert_eq!(result["configured"], true);
        assert!(result["persistence_warning"]
            .as_str()
            .unwrap()
            .contains("已热生效"));
        assert!(!result.to_string().contains("fixture-private-secret"));
        assert!(!result.to_string().contains("AKIA000000000EXAMPLE"));
    }
}
