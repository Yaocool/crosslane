use axum::extract::Path;
use crosslane_core::account::{Account, AuthMethod, Credentials};
use crosslane_core::ids::{new_account_id, new_machine_id};
use crosslane_core::provider::ProviderId;
use crosslane_ipc::protocol::{
    AccountAddSsoParams, ProviderAccountListResult, ProviderAccountSummary,
};

use super::*;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    provider: Option<String>,
    tag: Option<String>,
    status: Option<String>,
    sort: Option<Sort>,
    enabled_only: Option<bool>,
    #[serde(default)]
    untagged_only: bool,
    #[serde(default)]
    offset: usize,
    #[serde(default = "page_limit")]
    limit: usize,
    #[serde(default)]
    group_by: views::GroupBy,
}
fn page_limit() -> usize {
    100
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Sort {
    #[serde(alias = "email")]
    Name,
    Id,
    Credit,
}
impl Sort {
    fn rpc_value(self) -> &'static str {
        match self {
            Self::Name => "email",
            Self::Id => "id",
            Self::Credit => "credit",
        }
    }
}

pub(super) async fn list(State(state): State<RestState>, request: Request) -> ApiResult<Response> {
    let filter: ListQuery = query(&request)?;
    if filter.limit == 0 || filter.limit > 1000 || (filter.untagged_only && filter.tag.is_some()) {
        return Err(ApiError::bad(
            "limit must be 1..1000; tag and untagged_only cannot be combined",
        ));
    }
    let result = rpc(
        &state.app,
        method::V2_ACCOUNT_LIST,
        json!({
            "provider":filter.provider.unwrap_or_else(|| "all".into()), "tag":filter.tag,
            "enabled_only":filter.enabled_only, "status":filter.status, "sort":filter.sort.map(Sort::rpc_value)
        }),
    )
    .await?;
    let mut result: ProviderAccountListResult =
        serde_json::from_value(result).map_err(|_| ApiError::internal())?;
    if filter.untagged_only {
        result.accounts.retain(|account| account.tags.is_empty());
    }
    let total = result.accounts.len();
    let page = result
        .accounts
        .into_iter()
        .skip(filter.offset)
        .take(filter.limit)
        .map(views::account)
        .collect::<Vec<_>>();
    let has_more = filter.offset.saturating_add(page.len()) < total;
    Ok(ok(
        json!({"total":total,"offset":filter.offset,"limit":filter.limit,"has_more":has_more,
            "groups":views::groups(page,filter.group_by),"complete":result.complete,
            "errors":result.errors.into_iter().map(|(id,message)| (id,crosslane_translate::sanitize_error_message(&message))).collect::<std::collections::BTreeMap<_,_>>()
        }),
    ))
}

pub(super) async fn show(
    State(state): State<RestState>,
    Path((provider, account)): Path<(String, String)>,
) -> ApiResult<Response> {
    Ok(ok(lookup(&state.app, &provider, &account).await?))
}

fn require_single_provider(provider: &str) -> ApiResult<()> {
    if provider == "all" {
        return Err(ApiError::bad(
            "provider must identify a single configured instance",
        ));
    }
    Ok(())
}

async fn lookup(app: &Arc<AppState>, provider: &str, account: &str) -> ApiResult<Value> {
    lookup_mode(app, provider, account, true).await
}

async fn cached_lookup(app: &Arc<AppState>, provider: &str, account: &str) -> ApiResult<Value> {
    lookup_mode(app, provider, account, false).await
}

async fn lookup_mode(
    app: &Arc<AppState>,
    provider: &str,
    account: &str,
    refresh: bool,
) -> ApiResult<Value> {
    require_single_provider(provider)?;
    let result = if refresh {
        rpc(
            app,
            method::V2_ACCOUNT_SHOW,
            json!({"provider":provider,"id":account}),
        )
        .await?
    } else {
        crate::admin::handlers::rest::account_snapshot(app, provider, account).await?
    };
    let account: ProviderAccountSummary =
        serde_json::from_value(result).map_err(|_| ApiError::internal())?;
    Ok(views::account(account))
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Auth {
    #[serde(alias = "api_key")]
    ApiKey,
    Oauth,
    Sso,
    Token,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    provider: String,
    auth: Auth,
    #[serde(default)]
    tags: Vec<String>,
    tag: Option<String>,
    email: Option<String>,
    label: Option<String>,
    token: Option<String>,
    credentials: Option<Credentials>,
    region: Option<String>,
    profile_arn: Option<String>,
    machine_id: Option<String>,
    password: Option<String>,
    start_url: Option<String>,
}

pub(super) fn normalize_tags(mut tags: Vec<String>, tag: Option<String>) -> ApiResult<Vec<String>> {
    if let Some(tag) = tag {
        tags.push(tag);
    }
    if tags.len() > 64 {
        return Err(ApiError::bad("at most 64 account tags are allowed"));
    }
    for tag in &mut tags {
        *tag = tag.trim().to_owned();
        if tag.is_empty() || tag.chars().count() > 128 {
            return Err(ApiError::bad("account tags must contain 1..128 characters"));
        }
    }
    tags.sort();
    tags.dedup();
    Ok(tags)
}

/// Validated inputs are shared by single creation and queued batches. No Debug:
/// passwords/tokens exist only while the accepted operation is executing.
pub(super) enum PreparedCreate {
    Kiro(Box<Account>),
    Sso(AccountAddSsoParams),
    CopilotToken {
        provider: String,
        token: String,
        label: Option<String>,
        tags: Vec<String>,
    },
}

impl PreparedCreate {
    pub(super) fn identity(&self) -> (String, Option<String>) {
        match self {
            Self::Kiro(account) => ("kiro".into(), Some(account.email.clone())),
            Self::Sso(params) => ("kiro".into(), Some(params.email.clone())),
            Self::CopilotToken {
                provider, label, ..
            } => (provider.clone(), label.clone()),
        }
    }
    pub(super) fn secrets(&self) -> Vec<String> {
        match self {
            Self::Kiro(account) => [
                Some(&account.credentials.access_token),
                account.credentials.refresh_token.as_ref(),
                account.credentials.client_secret.as_ref(),
            ]
            .into_iter()
            .flatten()
            .filter(|value| !value.is_empty())
            .cloned()
            .collect(),
            Self::Sso(params) => vec![params.password.clone()],
            Self::CopilotToken { token, .. } => vec![token.clone()],
        }
    }
}

pub(super) fn prepare_json(
    app: &Arc<AppState>,
    value: Value,
    tags: &[String],
) -> ApiResult<PreparedCreate> {
    let mut input: Create =
        serde_json::from_value(value).map_err(|_| ApiError::bad("invalid account fields"))?;
    input.tags.extend_from_slice(tags);
    prepare(app, input)
}

pub(super) async fn create(
    State(state): State<RestState>,
    request: Request,
) -> ApiResult<Response> {
    let input: Create = body(request).await?;
    let input = prepare(&state.app, input)?;
    let _permit = state.account_creates.try_acquire().map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            "account creation limit reached",
        )
    })?;
    Ok(created(execute(&state.app, input).await?))
}

fn prepare(app: &Arc<AppState>, input: Create) -> ApiResult<PreparedCreate> {
    validate_login_options(input.region.as_deref(), input.start_url.as_deref())?;
    ProviderId::parse(&input.provider)
        .map_err(|_| ApiError::bad("invalid provider instance ID"))?;
    let provider = app
        .config
        .current()
        .effective_providers()
        .into_iter()
        .find(|provider| provider.id == input.provider)
        .ok_or_else(|| ApiError::not_found("provider instance not found"))?;
    let tags = normalize_tags(input.tags, input.tag)?;
    if input
        .label
        .as_ref()
        .is_some_and(|label| label.trim().is_empty() || label.chars().count() > 256)
    {
        return Err(ApiError::bad("label must contain 1..256 characters"));
    }
    if provider.kind == "copilot" {
        if !matches!(input.auth, Auth::Token)
            || input.credentials.is_some()
            || input.email.is_some()
            || input.region.is_some()
            || input.profile_arn.is_some()
            || input.machine_id.is_some()
            || input.password.is_some()
            || input.start_url.is_some()
        {
            return Err(ApiError::bad(
                "Copilot account creation requires auth=token, token, optional label and tags",
            ));
        }
        let token = input
            .token
            .filter(|token| !token.trim().is_empty() && !token.chars().any(char::is_whitespace))
            .ok_or_else(|| ApiError::bad("a non-empty token without whitespace is required"))?;
        return Ok(PreparedCreate::CopilotToken {
            provider: input.provider,
            token,
            label: input.label,
            tags,
        });
    }
    if provider.kind != "kiro" {
        return Err(ApiError::bad(
            "account creation is unsupported for this provider driver",
        ));
    }
    let email = input
        .email
        .map(|email| email.trim().to_ascii_lowercase())
        .filter(|email| email.contains('@') && !email.chars().any(char::is_whitespace))
        .ok_or_else(|| ApiError::bad("a valid Kiro email is required"))?;
    if matches!(input.auth, Auth::Sso) {
        if !cfg!(feature = "sso") {
            return Err(ApiError::new(
                StatusCode::NOT_IMPLEMENTED,
                "unsupported",
                "Kiro SSO requires a build with the sso feature",
            ));
        }
        if input.token.is_some()
            || input.credentials.is_some()
            || input.label.is_some()
            || input.profile_arn.is_some()
            || input.machine_id.is_some()
        {
            return Err(ApiError::bad(
                "SSO accepts email, password, start_url, region and tags",
            ));
        }
        let start_url = input
            .start_url
            .unwrap_or_else(|| app.config.current().sso.start_url.clone());
        validate_login_options(None, Some(&start_url))?;
        let url = url::Url::parse(&start_url)
            .map_err(|_| ApiError::bad("a valid HTTPS start_url is required"))?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(ApiError::bad(
                "start_url must use HTTPS without URL credentials",
            ));
        }
        let password = input
            .password
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ApiError::bad("password is required"))?;
        return Ok(PreparedCreate::Sso(AccountAddSsoParams {
            email,
            password,
            start_url,
            region: input.region.unwrap_or_else(|| "us-east-1".into()),
            tags,
            headful: false,
        }));
    }
    if input.password.is_some() || input.start_url.is_some() {
        return Err(ApiError::bad(
            "password and start_url are only accepted for SSO",
        ));
    }
    let credentials = match input.auth {
        Auth::ApiKey => {
            if input.credentials.is_some() || input.profile_arn.is_some() {
                return Err(ApiError::bad(
                    "API key auth does not accept OAuth credentials or profile_arn",
                ));
            }
            Credentials {
                access_token: input
                    .token
                    .ok_or_else(|| ApiError::bad("token is required"))?,
                refresh_token: None,
                client_id: None,
                client_secret: None,
                region: input.region.unwrap_or_else(|| "us-east-1".into()),
                expires_at: 0,
                auth_method: AuthMethod::ApiKey,
            }
        }
        Auth::Oauth => {
            if input.token.is_some() || input.region.is_some() {
                return Err(ApiError::bad(
                    "OAuth accepts credentials rather than token or top-level region",
                ));
            }
            input
                .credentials
                .filter(|credentials| credentials.auth_method != AuthMethod::ApiKey)
                .ok_or_else(|| {
                    ApiError::bad("OAuth credentials with idc or social auth_method are required")
                })?
        }
        _ => return Err(ApiError::bad("Kiro supports auth=api-key, oauth or sso")),
    };
    let account = Account {
        id: new_account_id(),
        email,
        label: input.label,
        enabled: true,
        machine_id: input.machine_id.unwrap_or_else(new_machine_id),
        profile_arn: input.profile_arn,
        upstream_user_id: None,
        credentials,
        usage: None,
        subscription: None,
        tags,
        created_at: crate::meter::now_secs(),
        credit_exhausted: false,
    };
    crate::admin::handlers::rest::validate_kiro_account(&account)?;
    Ok(PreparedCreate::Kiro(Box::new(account)))
}

pub(super) fn validate_login_options(
    region: Option<&str>,
    start_url: Option<&str>,
) -> ApiResult<()> {
    if region.is_some_and(|value| {
        value.is_empty() || value.len() > 64 || value.chars().any(char::is_whitespace)
    }) {
        return Err(ApiError::bad(
            "region must contain 1..64 bytes without whitespace",
        ));
    }
    if start_url.is_some_and(|value| value.len() > 8192) {
        return Err(ApiError::bad("start_url must not exceed 8192 bytes"));
    }
    Ok(())
}

pub(super) async fn execute(app: &Arc<AppState>, input: PreparedCreate) -> ApiResult<Value> {
    let secrets = input.secrets();
    let result = async {
        match input {
            PreparedCreate::Kiro(account) => {
                let result =
                    crate::admin::handlers::rest::create_kiro_account(app, *account).await?;
                Ok(views::account(
                    serde_json::from_value(result).map_err(|_| ApiError::internal())?,
                ))
            }
            PreparedCreate::Sso(params) => {
                let result = rpc(
                    app,
                    method::ACCOUNT_ADD_SSO,
                    serde_json::to_value(params).map_err(|_| ApiError::internal())?,
                )
                .await?;
                let id = result["id"].as_str().ok_or_else(ApiError::internal)?;
                cached_lookup(app, "kiro", id).await
            }
            PreparedCreate::CopilotToken {
                provider,
                token,
                label,
                tags,
            } => {
                let result = rpc(
                    app,
                    method::V2_ACCOUNT_IMPORT_TOKEN,
                    json!({"provider":provider,"token":token,"label":label,"tags":tags}),
                )
                .await?;
                let id = result["id"].as_str().ok_or_else(ApiError::internal)?;
                cached_lookup(app, &provider, id).await
            }
        }
    }
    .await;
    result.map_err(|error| protect_error(error, &secrets))
}

pub(super) fn protect_error(mut error: ApiError, secrets: &[String]) -> ApiError {
    // Some upstream/browser errors can echo a password without a recognizable
    // credential prefix. Exact submitted values must also be removed.
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        error.message = error.message.replace(secret, "<redacted>");
    }
    error.message = crosslane_translate::sanitize_error_message(&error.message);
    error
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveQuery {
    #[serde(default)]
    force: bool,
}

pub(super) async fn remove(
    State(state): State<RestState>,
    Path((provider, account)): Path<(String, String)>,
    request: Request,
) -> ApiResult<Response> {
    let input: RemoveQuery = query(&request)?;
    let account = cached_lookup(&state.app, &provider, &account).await?;
    rpc(
        &state.app,
        method::V2_ACCOUNT_REMOVE,
        json!({"provider":provider,"id":account["id"],"force":input.force}),
    )
    .await?;
    // A provider's native removal result can contain raw quota errors and
    // endpoint metadata. Return a stable acknowledgement for both drivers.
    Ok(ok(
        json!({"removed":account["id"],"provider_id":account["provider_id"],"reference":account["reference"]}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Enabled {
    enabled: bool,
}
pub(super) async fn set_enabled(
    State(state): State<RestState>,
    Path((provider, account)): Path<(String, String)>,
    request: Request,
) -> ApiResult<Response> {
    require_single_provider(&provider)?;
    let input: Enabled = body(request).await?;
    rpc(
        &state.app,
        method::V2_ACCOUNT_SET_ENABLED,
        json!({"provider":provider,"id":account,"enabled":input.enabled}),
    )
    .await?;
    Ok(ok(cached_lookup(&state.app, &provider, &account).await?))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Tags {
    #[serde(default)]
    add: Vec<String>,
    #[serde(default)]
    remove: Vec<String>,
}
pub(super) async fn tags(
    State(state): State<RestState>,
    Path((provider, account)): Path<(String, String)>,
    request: Request,
) -> ApiResult<Response> {
    require_single_provider(&provider)?;
    let input: Tags = body(request).await?;
    let add = normalize_tags(input.add, None)?;
    let remove = normalize_tags(input.remove, None)?;
    if add.is_empty() && remove.is_empty() {
        return Err(ApiError::bad("add or remove must contain at least one tag"));
    }
    rpc(
        &state.app,
        method::V2_ACCOUNT_TAG,
        json!({"provider":provider,"id":account,"add":add,"remove":remove}),
    )
    .await?;
    Ok(ok(cached_lookup(&state.app, &provider, &account).await?))
}

async fn action(
    state: RestState,
    provider: String,
    account: String,
    name: &str,
) -> ApiResult<Response> {
    let before = cached_lookup(&state.app, &provider, &account).await?;
    let actual_id = before["id"].as_str().ok_or_else(ApiError::internal)?;
    let key_usage_only = name == method::V2_ACCOUNT_REFRESH
        && provider == "kiro"
        && state.app.with_accounts(|store| {
            store.all().iter().any(|account| {
                account.id == actual_id && account.credentials.auth_method == AuthMethod::ApiKey
            })
        });
    let result = if key_usage_only {
        crate::admin::handlers::rest::refresh_kiro_api_key_usage(&state.app, actual_id).await?
    } else {
        rpc(&state.app, name, json!({"provider":provider,"id":account})).await?
    };
    if name == method::ACCOUNT_SERVICES {
        return Ok(ok(result));
    }
    // Kiro's single-account refresh returns a one-element report array. An RPC
    // success can still carry a failed probe/refresh, so keep that distinction.
    let report = result
        .as_array()
        .and_then(|items| items.first())
        .unwrap_or(&result);
    if report["ok"] == false {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "upstream_failed",
            crosslane_translate::sanitize_error_message(
                report["error"]
                    .as_str()
                    .unwrap_or("account operation failed"),
            ),
        ));
    }
    let mut projected = if name == method::V2_ACCOUNT_REFRESH {
        lookup(&state.app, &provider, &account).await?
    } else {
        cached_lookup(&state.app, &provider, &account).await?
    };
    let mut operation = json!({"ok":true});
    for field in ["refreshed", "persisted", "usage_refreshed"] {
        if let Some(value) = report[field].as_bool() {
            operation[field] = json!(value);
        }
    }
    projected["operation"] = operation;
    Ok(ok(projected))
}
pub(super) async fn refresh(
    State(state): State<RestState>,
    Path((provider, account)): Path<(String, String)>,
) -> ApiResult<Response> {
    action(state, provider, account, method::V2_ACCOUNT_REFRESH).await
}
pub(super) async fn probe(
    State(state): State<RestState>,
    Path((provider, account)): Path<(String, String)>,
) -> ApiResult<Response> {
    action(state, provider, account, method::V2_ACCOUNT_PROBE).await
}
pub(super) async fn reset_health(
    State(state): State<RestState>,
    Path((provider, account)): Path<(String, String)>,
) -> ApiResult<Response> {
    action(state, provider, account, method::V2_ACCOUNT_RESET_HEALTH).await
}
pub(super) async fn services(
    State(state): State<RestState>,
    Path((provider, account)): Path<(String, String)>,
) -> ApiResult<Response> {
    action(state, provider, account, method::ACCOUNT_SERVICES).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Request {
        Request::post("/accounts")
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(
                json!({
                    "provider":"kiro","auth":"api-key","email":"creation-slot@example.invalid",
                    "token":"ksk_fixture_creation_slot"
                })
                .to_string(),
            ))
            .unwrap()
    }

    #[tokio::test]
    async fn occupied_creation_slots_fail_fast_without_persisting_and_recover_when_released() {
        let directory = tempfile::tempdir().unwrap();
        let paths = crosslane_core::paths::Paths::from_env_values(
            Some(directory.path().to_str().unwrap()),
            None,
            None,
            None,
        );
        crosslane_store::bootstrap::ensure_layout(&paths)
            .await
            .unwrap();
        let accounts = crosslane_store::accounts::AccountStore::load(&paths.accounts_file)
            .await
            .unwrap();
        let app = Arc::new(AppState::new(
            paths,
            crosslane_store::config_loader::ConfigHandle::new(Default::default()),
            accounts,
        ));
        app.providers
            .reconcile(&app.config.current(), &app.paths.data_dir)
            .await
            .unwrap();
        let state = RestState {
            app,
            requests: Arc::new(Semaphore::new(16)),
            account_creates: Arc::new(Semaphore::new(8)),
            batches: Arc::new(super::super::batches::BatchRegistry::default()),
            enterprise: Arc::new(super::super::enterprise::Control::default()),
        };
        let occupied = state.account_creates.acquire_many(8).await.unwrap();
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            create(State(state.clone()), request()),
        )
        .await
        .expect("saturated creation must not queue a management request")
        .unwrap_err();
        assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error.code, "busy");
        assert!(state.app.with_accounts(|store| store.is_empty()));
        drop(occupied);
        let response = create(State(state.clone()), request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(state.app.with_accounts(|store| store.all().len()), 1);
        assert_eq!(state.account_creates.available_permits(), 8);
        state.app.shutdown.cancel();
    }
}
