use axum::extract::Path;
use crosslane_core::config::ServiceAccountTags;
use crosslane_core::ids::index_by_id_or_name;
use crosslane_ipc::protocol::{
    ProxyServiceAccountsResult, ProxyServiceListResult, ProxyServiceView,
};

use super::*;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PoolQuery {
    provider: Option<String>,
    #[serde(default)]
    group_by: views::GroupBy,
}

pub(super) async fn list(State(state): State<RestState>, request: Request) -> ApiResult<Response> {
    let filter: ProviderQuery = query(&request)?;
    Ok(ok(rpc(
        &state.app,
        method::SERVICE_LIST,
        json!({"provider":filter.provider}),
    )
    .await?))
}

async fn lookup(app: &Arc<AppState>, selector: &str) -> ApiResult<ProxyServiceView> {
    let list: ProxyServiceListResult =
        serde_json::from_value(rpc(app, method::SERVICE_LIST, Value::Null).await?)
            .map_err(|_| ApiError::internal())?;
    index_by_id_or_name(&list.services, selector, |service| {
        (Some(service.id.as_str()), Some(service.name.as_str()))
    })
    .map(|index| list.services[index].clone())
    .ok_or_else(|| ApiError::not_found("service not found"))
}

async fn pool(app: &Arc<AppState>, service_id: &str, filter: PoolQuery) -> ApiResult<Value> {
    let result: ProxyServiceAccountsResult = serde_json::from_value(
        rpc(
            app,
            method::SERVICE_ACCOUNTS,
            json!({"service":service_id,"provider":filter.provider}),
        )
        .await?,
    )
    .map_err(|_| ApiError::internal())?;
    if result.service_id != service_id {
        return Err(ApiError::not_found("service no longer exists"));
    }
    Ok(project_pool(result, filter.group_by))
}

async fn service_keys(app: &Arc<AppState>, service_id: &str) -> ApiResult<Value> {
    let result = rpc(
        app,
        method::SERVICE_APIKEYS,
        json!({"service":service_id,"show_secret":false}),
    )
    .await?;
    if result["service_id"]
        .as_str()
        .ok_or_else(ApiError::internal)?
        != service_id
    {
        return Err(ApiError::not_found("service no longer exists"));
    }
    Ok(result)
}

fn project_pool(result: ProxyServiceAccountsResult, group_by: views::GroupBy) -> Value {
    let count = result.provider_accounts.len();
    let accounts = result
        .provider_accounts
        .into_iter()
        .map(views::account)
        .collect();
    json!({
        "service_id":result.service_id,"service_name":result.service_name,
        "uses_global_pool":result.uses_global_pool,"account_tags":result.account_tags,
        "manual_account_ids":result.account_ids,"excluded_account_ids":result.excluded_account_ids,
        "total_accounts":count,"groups":views::groups(accounts,group_by)
    })
}

pub(super) async fn show(
    State(state): State<RestState>,
    Path(service): Path<String>,
    request: Request,
) -> ApiResult<Response> {
    let filter: PoolQuery = query(&request)?;
    let service = lookup(&state.app, &service).await?;
    let accounts = pool(&state.app, &service.id, filter).await?;
    let keys = service_keys(&state.app, &service.id).await?;
    Ok(ok(
        json!({"service":service,"account_pool":accounts,"api_keys":keys["api_keys"]}),
    ))
}

#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct Create {
    name: String,
    host: Option<String>,
    port: Option<u16>,
    #[serde(default)]
    skip_user_agent_check: bool,
    #[serde(alias = "tags")]
    account_tag: Option<ServiceAccountTags>,
    api_key_name: Option<String>,
    api_key_format: Option<String>,
    #[serde(default, alias = "providers")]
    allowed_providers: Vec<String>,
    default_provider: Option<String>,
}

pub(super) async fn create(
    State(state): State<RestState>,
    request: Request,
) -> ApiResult<Response> {
    let input: Create = body(request).await?;
    Ok(created(
        rpc(
            &state.app,
            method::SERVICE_CREATE,
            serde_json::to_value(input).map_err(|_| ApiError::internal())?,
        )
        .await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Enabled {
    enabled: bool,
}
pub(super) async fn set_enabled(
    State(state): State<RestState>,
    Path(service): Path<String>,
    request: Request,
) -> ApiResult<Response> {
    let input: Enabled = body(request).await?;
    Ok(ok(crate::admin::handlers::rest::set_service_enabled(
        &state.app,
        &service,
        input.enabled,
    )
    .await?))
}
pub(super) async fn remove(
    State(state): State<RestState>,
    Path(service): Path<String>,
) -> ApiResult<Response> {
    Ok(ok(rpc(
        &state.app,
        method::SERVICE_DELETE,
        json!({"service":service}),
    )
    .await?))
}
pub(super) async fn accounts(
    State(state): State<RestState>,
    Path(service): Path<String>,
    request: Request,
) -> ApiResult<Response> {
    let filter: PoolQuery = query(&request)?;
    let service = lookup(&state.app, &service).await?;
    Ok(ok(pool(&state.app, &service.id, filter).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    accounts: Vec<String>,
    provider: Option<String>,
}
async fn change_binding(
    state: RestState,
    service: String,
    request: Request,
    add: bool,
) -> ApiResult<Response> {
    let input: Binding = body(request).await?;
    if input.accounts.is_empty() || input.accounts.len() > 1000 {
        return Err(ApiError::bad(
            "accounts must contain 1..1000 account references",
        ));
    }
    let result = rpc(
        &state.app,
        if add {
            method::SERVICE_ACCOUNT_ADD
        } else {
            method::SERVICE_ACCOUNT_REMOVE
        },
        json!({"service":service,"accounts":input.accounts,"provider":input.provider}),
    )
    .await?;
    let result: ProxyServiceAccountsResult =
        serde_json::from_value(result).map_err(|_| ApiError::internal())?;
    Ok(ok(project_pool(result, views::GroupBy::Provider)))
}
pub(super) async fn bind_accounts(
    State(state): State<RestState>,
    Path(service): Path<String>,
    request: Request,
) -> ApiResult<Response> {
    change_binding(state, service, request, true).await
}
pub(super) async fn unbind_accounts(
    State(state): State<RestState>,
    Path(service): Path<String>,
    request: Request,
) -> ApiResult<Response> {
    change_binding(state, service, request, false).await
}
pub(super) async fn api_keys(
    State(state): State<RestState>,
    Path(service): Path<String>,
) -> ApiResult<Response> {
    let service = lookup(&state.app, &service).await?;
    Ok(ok(service_keys(&state.app, &service.id).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosslane_core::config::{ApiKeyConfig, Config, ProxyServiceConfig};
    use crosslane_core::paths::Paths;
    use crosslane_store::{accounts::AccountStore, config_loader::ConfigHandle};

    #[tokio::test]
    async fn resolved_service_reads_do_not_fall_back_to_a_neighbors_matching_name() {
        let directory = tempfile::tempdir().unwrap();
        let paths =
            Paths::from_env_values(Some(directory.path().to_str().unwrap()), None, None, None);
        crosslane_store::bootstrap::ensure_layout(&paths)
            .await
            .unwrap();
        let accounts = AccountStore::load(&paths.accounts_file).await.unwrap();
        let config = Config {
            api_key: vec![ApiKeyConfig {
                id: Some("ak_neighbor".into()),
                name: "neighbor".into(),
                key: "fixture-neighbor-key".into(),
                ..ApiKeyConfig::default()
            }],
            proxy_service: vec![ProxyServiceConfig {
                id: "svc_neighbor".into(),
                name: "svc_removed".into(),
                host: "127.0.0.1".into(),
                port: 6201,
                api_key_ids: vec!["ak_neighbor".into()],
                ..ProxyServiceConfig::default()
            }],
            ..Config::default()
        };
        config.validate().unwrap();
        let app = Arc::new(AppState::new(paths, ConfigHandle::new(config), accounts));
        app.providers
            .reconcile(&app.config.current(), &app.paths.data_dir)
            .await
            .unwrap();
        // These helpers receive an ID already resolved by the HTTP layer. If
        // that service disappears, a native name alias must not redirect reads.
        let pool_result = pool(&app, "svc_removed", PoolQuery::default()).await;
        let key_result = service_keys(&app, "svc_removed").await;
        assert_eq!(pool_result.unwrap_err().status, StatusCode::NOT_FOUND);
        assert_eq!(key_result.unwrap_err().status, StatusCode::NOT_FOUND);
        assert!(pool(&app, "svc_neighbor", PoolQuery::default())
            .await
            .is_ok());
        let keys = service_keys(&app, "svc_neighbor").await.unwrap();
        assert_eq!(keys["api_keys"][0]["id"], "ak_neighbor");
        assert!(keys["api_keys"][0].get("key").is_none());
        app.shutdown.cancel();
    }
}
