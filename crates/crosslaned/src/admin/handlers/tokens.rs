//! Admin REST credentials are managed over the local control socket only.

use crosslane_core::config::{AdminRestConfig, AdminRestTokenConfig, AdminTokenRole};
use crosslane_ipc::protocol::{
    AdminTokenListResult, AdminTokenResult, AdminTokenView, CreatedAdminToken,
};
use serde::Deserialize;

use super::*;

fn view(token: &AdminRestTokenConfig) -> AdminTokenView {
    AdminTokenView {
        id: token.id.clone(),
        name: token.name.clone(),
        role: token.role,
        enabled: token.enabled,
        created_at: token.created_at,
    }
}

fn legacy(id: &str, digest: String, role: AdminTokenRole) -> AdminRestTokenConfig {
    AdminRestTokenConfig {
        id: id.into(),
        name: id.into(),
        role,
        token_sha256: digest,
        enabled: true,
        created_at: None,
    }
}

fn metadata(rest: &AdminRestConfig) -> Vec<AdminTokenView> {
    let mut tokens = rest.tokens.iter().map(view).collect::<Vec<_>>();
    if !rest.token_sha256.is_empty() {
        tokens.push(view(&legacy(
            "legacy-admin",
            rest.token_sha256.clone(),
            AdminTokenRole::Admin,
        )));
    }
    if let Some(digest) = &rest.read_token_sha256 {
        tokens.push(view(&legacy(
            "legacy-read-only",
            digest.clone(),
            AdminTokenRole::ReadOnly,
        )));
    }
    tokens.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    tokens
}

fn select(rest: &AdminRestConfig, selector: &str) -> Result<AdminTokenView, RpcError> {
    let tokens = metadata(rest);
    index_by_id_or_name(&tokens, selector, |token| {
        (Some(token.id.as_str()), Some(token.name.as_str()))
    })
    .map(|index| tokens[index].clone())
    .ok_or_else(|| RpcError {
        code: 404,
        message: "admin token not found".into(),
    })
}

fn managed_index(rest: &mut AdminRestConfig, selector: &str) -> Result<usize, RpcError> {
    let token = select(rest, selector)?;
    // Materialize only the legacy credential explicitly being changed. Its ID
    // remains stable and the other legacy credential continues to work.
    if token.id == "legacy-admin" && !rest.token_sha256.is_empty() {
        let digest = std::mem::take(&mut rest.token_sha256);
        rest.tokens
            .push(legacy("legacy-admin", digest, AdminTokenRole::Admin));
    } else if token.id == "legacy-read-only" {
        if let Some(digest) = rest.read_token_sha256.take() {
            rest.tokens
                .push(legacy("legacy-read-only", digest, AdminTokenRole::ReadOnly));
        }
    }
    rest.tokens
        .iter()
        .position(|entry| entry.id == token.id)
        .ok_or_else(|| RpcError::internal("admin token is unavailable"))
}

pub(super) fn list(state: &Arc<AppState>) -> Handled {
    let config = state.config.current();
    to_value(AdminTokenListResult {
        tokens: metadata(&config.admin.rest),
        rest_enabled: config.admin.rest.enabled,
        listener_enabled: config.status_http.enabled,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selector {
    id: String,
}

pub(super) fn show(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: Selector = parse_params(params)?;
    let config = state.config.current();
    to_value(AdminTokenResult {
        token: select(&config.admin.rest, params.id.trim())?,
        rest_enabled: config.admin.rest.enabled,
        listener_enabled: config.status_http.enabled,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    name: String,
    #[serde(default)]
    role: AdminTokenRole,
    #[serde(default)]
    enable_rest: bool,
}

enum Change {
    Create(Create),
    Rotate(String),
    Revoke(String),
    Enabled(String, bool),
}

pub(super) async fn create(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    mutate(state, Change::Create(parse_params(params)?)).await
}
pub(super) async fn rotate(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: Selector = parse_params(params)?;
    mutate(state, Change::Rotate(params.id.trim().into())).await
}
pub(super) async fn revoke(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    let params: Selector = parse_params(params)?;
    mutate(state, Change::Revoke(params.id.trim().into())).await
}
pub(super) async fn set_enabled(state: &Arc<AppState>, params: serde_json::Value) -> Handled {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Params {
        id: String,
        enabled: bool,
    }
    let params: Params = parse_params(params)?;
    mutate(
        state,
        Change::Enabled(params.id.trim().into(), params.enabled),
    )
    .await
}

fn secret_and_digest() -> (String, String) {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let random = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let secret = format!("sk-admin-{random}");
    let digest = format!("{:x}", Sha256::digest(secret.as_bytes()));
    (secret, digest)
}

async fn mutate(state: &Arc<AppState>, change: Change) -> Handled {
    let _file = crosslane_store::atomic::lock_file_exclusive(&state.paths.config_file)
        .await
        .map_err(|_| RpcError::internal("could not lock the configuration file"))?;
    let _mutation = state.lock_config_mutation().await;
    let raw = tokio::fs::read_to_string(&state.paths.config_file)
        .await
        .map_err(|_| RpcError::internal("could not read the configuration file"))?;
    let mut disk: Config = toml::from_str(&raw).map_err(|_| {
        RpcError::bad_params("configuration could not be parsed; run crosslane config validate")
    })?;
    let current = state.config.current();
    let listener_ready = disk.status_http.enabled && disk.status_http == current.status_http;
    let rest = &mut disk.admin.rest;
    let mut plaintext = None;
    let event;
    let changed = match change {
        Change::Create(params) => {
            if params.enable_rest && !listener_ready {
                return Err(RpcError::bad_params(
                    "use `crosslane config edit admin` to enable [status_http] and apply its address/port with `crosslane config reload` before using --enable-rest",
                ));
            }
            let name = params.name.trim().to_owned();
            if metadata(rest).iter().any(|token| token.name == name) {
                return Err(RpcError {
                    code: 409,
                    message: "admin token name already exists".into(),
                });
            }
            let (secret, digest) = secret_and_digest();
            let token = AdminRestTokenConfig {
                id: format!("adm_{}", &uuid::Uuid::new_v4().simple().to_string()[..16]),
                name,
                role: params.role,
                enabled: true,
                created_at: Some(now_secs()),
                token_sha256: digest,
            };
            let result = view(&token);
            rest.tokens.push(token);
            if params.enable_rest {
                rest.enabled = true;
            }
            plaintext = Some(secret);
            event = "created";
            result
        }
        Change::Rotate(selector) => {
            let index = managed_index(rest, &selector)?;
            let (secret, digest) = secret_and_digest();
            rest.tokens[index].token_sha256 = digest;
            plaintext = Some(secret);
            event = "rotated";
            view(&rest.tokens[index])
        }
        Change::Revoke(selector) => {
            let index = managed_index(rest, &selector)?;
            let token = rest.tokens.remove(index);
            if metadata(rest).is_empty() {
                rest.enabled = false;
            }
            event = "revoked";
            let mut result = view(&token);
            result.enabled = false;
            result
        }
        Change::Enabled(selector, enabled) => {
            let index = managed_index(rest, &selector)?;
            rest.tokens[index].enabled = enabled;
            event = if enabled { "enabled" } else { "disabled" };
            view(&rest.tokens[index])
        }
    };
    disk.validate()
        .map_err(|error| RpcError::bad_params(error.to_string()))?;
    let effective = crosslane_store::config_loader::merge_admin_rest_auth(&current, &disk);
    let output = render(&raw, &disk.admin.rest)?;
    // Preserve pending disk edits without treating them as applied. Restore
    // must retain the latest tokens, including rotations and revocations.
    let applied_text = state
        .applied_config_text()
        .map(|applied| render(&applied, &effective))
        .transpose()?;
    write_config_update_locked(state, &raw, &output).await?;
    // No model/provider/listener reconfiguration is required for an auth change.
    state.apply_admin_rest_auth_locked(effective);
    state.mark_admin_rest_auth_reloaded(now_secs(), applied_text);
    let config = state.config.current();
    tracing::info!(event="admin.token.changed",action=event,token_id=%changed.id,role=changed.role.as_str(),"admin token configuration changed");
    if let Some(secret) = plaintext {
        to_value(CreatedAdminToken {
            token: changed,
            secret,
            rest_enabled: config.admin.rest.enabled,
            listener_enabled: config.status_http.enabled,
        })
    } else {
        to_value(AdminTokenResult {
            token: changed,
            rest_enabled: config.admin.rest.enabled,
            listener_enabled: config.status_http.enabled,
        })
    }
}

fn render(raw: &str, rest: &AdminRestConfig) -> Result<String, RpcError> {
    let before: toml::Value = toml::from_str(raw)
        .map_err(|_| RpcError::internal("could not parse configuration for update"))?;
    let mut after = before.clone();
    let root = after
        .as_table_mut()
        .ok_or_else(|| RpcError::internal("configuration must be a TOML table"))?;
    let admin = root
        .entry("admin".to_owned())
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()
        .ok_or_else(|| RpcError::internal("admin must be a TOML table"))?;
    let table = admin
        .entry("rest".to_owned())
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()
        .ok_or_else(|| RpcError::internal("admin.rest must be a TOML table"))?;
    let serialized = toml::Value::try_from(rest)
        .map_err(|_| RpcError::internal("could not serialize admin token configuration"))?;
    for field in ["enabled", "token_sha256", "read_token_sha256", "tokens"] {
        if let Some(value) = serialized.get(field) {
            table.insert(field.into(), value.clone());
        } else {
            table.remove(field);
        }
    }
    crosslane_store::config_update::render_update_preserving_comments(raw, &before, &after)
        .map_err(|_| RpcError::internal("could not update admin token configuration"))
}
