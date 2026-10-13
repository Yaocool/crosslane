use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use crosslane_core::config::{
    ApiKeyConfig, ApiKeyFormat, Config, ProviderConfig, ProxyServiceConfig,
};
use crosslane_core::provider::ProviderId;
use serde_json::{json, Value};
use tower::ServiceExt;
use wiremock::matchers::{header as mock_header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::router_for_service;
use super::tests::{account_with_usage, body_json, discovered_model, test_state};
use crate::state::AppState;

fn replace_config(state: &AppState, config: Config) {
    state.meter.replace_configs(&config.api_key);
    state.config.replace(config);
}

async fn catalog(
    state: &Arc<AppState>,
    listener: &ProxyServiceConfig,
    endpoint: &str,
    protocol: usize,
    credential: Option<&str>,
) -> Value {
    let mut request = Request::get(endpoint);
    if let Some(credential) = credential {
        request = request.header(header::AUTHORIZATION, format!("Bearer {credential}"));
    }
    if protocol == 0 {
        request = request.header(header::USER_AGENT, "codex_cli_rs/0.147.0 (test)");
    } else if protocol == 1 {
        request = request.header("anthropic-version", "2023-06-01");
    }
    let response = router_for_service(Arc::clone(state), listener.clone(), credential.is_some())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    body_json(response).await
}

async fn assert_catalog(
    state: &Arc<AppState>,
    listener: &ProxyServiceConfig,
    credential: Option<&str>,
    expected: &[&str],
) {
    let expected = expected
        .iter()
        .map(|id| id.to_string())
        .collect::<BTreeSet<_>>();
    for endpoint in ["/models", "/v1/models"] {
        for protocol in 0..3 {
            let body = catalog(state, listener, endpoint, protocol, credential).await;
            let actual = body["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|model| model["id"].as_str().unwrap().to_string())
                .collect::<BTreeSet<_>>();
            assert_eq!(actual, expected, "{endpoint} protocol={protocol}: {body}");
        }
    }
}

#[tokio::test]
async fn kiro_catalog_requires_an_enabled_account_even_without_dynamic_discovery() {
    for dynamic in [false, true] {
        let mut config = Config::default();
        config.models.dynamic_discovery = dynamic;
        let (_directory, state) = test_state(config).await;
        let service = ProxyServiceConfig {
            id: "svc_scope".into(),
            account_tag: Some("team-a".into()),
            ..ProxyServiceConfig::default()
        };
        assert_catalog(&state, &service, None, &[]).await;

        let mut outside = account_with_usage("outside", "outside@example.com", true, None);
        outside.tags = vec!["team-b".into()];
        let mut disabled = account_with_usage("disabled", "disabled@example.com", false, None);
        disabled.tags = vec!["team-a".into()];
        state.pool().replace_accounts(vec![outside, disabled]).await;
        assert_catalog(&state, &service, None, &[]).await;
    }
}

#[tokio::test]
async fn mixed_catalog_uses_current_pool_and_preserves_key_and_disabled_model_filters() {
    let server = MockServer::start().await;
    let copilot = |id: &str| {
        let mut provider = ProviderConfig {
            id: id.into(),
            kind: "copilot".into(),
            ..ProviderConfig::default()
        };
        provider
            .settings
            .insert("github_api_base".into(), json!(server.uri()));
        provider
            .settings
            .insert("oauth_base".into(), json!(server.uri()));
        provider
            .settings
            .insert("allow_insecure_http".into(), json!(true));
        provider
    };
    let mut config = Config {
        provider: vec![
            ProviderConfig::default(),
            copilot("copilot"),
            copilot("copilot-alt"),
        ],
        ..Config::default()
    };
    config.models.disabled = vec!["gpt-disabled-model".into(), "claude-disabled-model".into()];
    config.api_key = vec![ApiKeyConfig {
        id: Some("ak_scope".into()),
        name: "scope".into(),
        key: "sk-scope-test".into(),
        format: ApiKeyFormat::Sk,
        allowed_providers: vec!["kiro".into(), "copilot".into(), "copilot-alt".into()],
        allowed_models: vec![
            "kiro/*".into(),
            "copilot/gpt-*".into(),
            "copilot-alt/*".into(),
        ],
        ..ApiKeyConfig::default()
    }];
    let (_directory, state) = test_state(config).await;
    state
        .providers
        .reconcile(&state.config.current(), &state.paths.data_dir)
        .await
        .unwrap();
    let runtime = state
        .providers
        .copilot(&ProviderId::parse("copilot").unwrap())
        .await
        .unwrap();
    let mut ids = BTreeMap::new();
    for (index, (name, tag, models)) in [
        (
            "inside",
            "team-a",
            vec![
                "gpt-inside",
                "gpt-shared",
                "gpt-disabled-model",
                "key-denied",
            ],
        ),
        ("manual", "team-b", vec!["gpt-manual"]),
        ("excluded", "team-a", vec!["gpt-excluded"]),
        ("outside", "team-b", vec!["gpt-outside", "gpt-shared"]),
        ("disabled", "team-a", vec!["gpt-disabled-account"]),
    ]
    .into_iter()
    .enumerate()
    {
        Mock::given(method("GET"))
            .and(path("/user"))
            .and(mock_header(
                "authorization",
                format!("Bearer github-{name}"),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":index + 10, "login":name
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/copilot_internal/v2/token"))
            .and(mock_header("authorization", format!("token github-{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "token":format!("api-{name}"), "expires_at":4_000_000_000i64,
                "endpoints":{"api":server.uri()}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(mock_header("authorization", format!("Bearer api-{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data":models.into_iter().map(|id| json!({
                    "id":id, "name":id, "capabilities":{
                        "limits":{"max_prompt_tokens":if name == "outside" {128} else {8192}},
                        "supported_endpoints":["/v1/messages","/chat/completions","/responses"]
                    }
                })).collect::<Vec<_>>()
            })))
            .expect(if name == "manual" { 2 } else { 1 })
            .mount(&server)
            .await;
        let account = runtime
            .import_token(format!("github-{name}"), None)
            .await
            .unwrap();
        runtime
            .update_account_tags(&account.id, &[tag.into()], &[])
            .await
            .unwrap();
        if name == "disabled" {
            runtime
                .set_account_enabled(&account.id, false)
                .await
                .unwrap();
        }
        ids.insert(name, account.id);
    }
    // A manual binding to Copilot must not include an account from a second
    // provider, even when both accounts belong to the same GitHub user.
    let alternate = state
        .providers
        .copilot(&ProviderId::parse("copilot-alt").unwrap())
        .await
        .unwrap();
    let alternate_account = alternate
        .import_token("github-manual".into(), None)
        .await
        .unwrap();
    alternate
        .update_account_tags(&alternate_account.id, &["team-b".into()], &[])
        .await
        .unwrap();

    let mut kiro_accounts = Vec::new();
    let mut kiro_models = Vec::new();
    for (name, tag, enabled) in [
        ("inside", "team-a", true),
        ("manual", "team-b", true),
        ("excluded", "team-a", true),
        ("outside", "team-b", true),
        ("disabled", "team-a", false),
    ] {
        let mut account =
            account_with_usage(&format!("kiro_{name}"), "scope@example.com", enabled, None);
        account.tags = vec![tag.into()];
        kiro_accounts.push(account);
        kiro_models.push(discovered_model(
            &format!("claude-scope-{name}"),
            name,
            Some(200_000),
            None,
        ));
    }
    kiro_models.push(discovered_model(
        "claude-disabled-model",
        "disabled model",
        None,
        None,
    ));
    state.pool().replace_accounts(kiro_accounts).await;
    for name in ["inside", "manual", "excluded", "outside", "disabled"] {
        let mut supported = vec![format!("claude-scope-{name}")];
        if name == "inside" {
            supported.push("claude-disabled-model".into());
        }
        state
            .pool()
            .get(&format!("kiro_{name}"))
            .await
            .unwrap()
            .set_supported_models(supported)
            .await;
    }
    state.models.finish_refresh(kiro_models);
    let listener = ProxyServiceConfig {
        id: "svc_scope".into(),
        api_key_ids: vec!["ak_scope".into()],
        allowed_providers: vec!["kiro".into(), "copilot".into(), "copilot-alt".into()],
        account_tag: Some("team-a".into()),
        account_ids: vec!["kiro_manual".into(), format!("copilot/{}", ids["manual"])],
        excluded_account_ids: vec![
            "kiro_excluded".into(),
            format!("copilot/{}", ids["excluded"]),
        ],
        ..ProxyServiceConfig::default()
    };
    let mut original = state.config.current().as_ref().clone();
    original.proxy_service = vec![listener.clone()];
    replace_config(&state, original.clone());
    assert_catalog(
        &state,
        &listener,
        Some("sk-scope-test"),
        &[
            "kiro/claude-scope-inside",
            "kiro/claude-scope-manual",
            "copilot/gpt-inside",
            "copilot/gpt-shared",
            "copilot/gpt-manual",
        ],
    )
    .await;
    let body = catalog(&state, &listener, "/v1/models", 1, Some("sk-scope-test")).await;
    let shared = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == "copilot/gpt-shared")
        .unwrap();
    assert_eq!(
        shared["max_input_tokens"], 8192,
        "pool-external metadata must not leak"
    );

    for (bindings, expected) in [
        (
            vec!["kiro_inside".to_string()],
            vec!["kiro/claude-scope-inside"],
        ),
        (
            vec![format!("copilot/{}", ids["inside"])],
            vec!["copilot/gpt-inside", "copilot/gpt-shared"],
        ),
        (vec![], vec![]),
    ] {
        let mut next = original.clone();
        next.proxy_service[0].account_tag = Some("empty-tag".into());
        next.proxy_service[0].account_ids = bindings;
        replace_config(&state, next);
        // Reuse the original listener snapshot to verify pool changes apply to
        // subsequent requests without rebuilding or restarting the listener.
        assert_catalog(&state, &listener, Some("sk-scope-test"), &expected).await;
    }

    let mut next = original.clone();
    next.proxy_service[0].allowed_providers = vec!["copilot".into()];
    next.proxy_service[0].default_provider = "copilot".into();
    replace_config(&state, next);
    assert_catalog(
        &state,
        &listener,
        Some("sk-scope-test"),
        &["gpt-inside", "gpt-shared", "gpt-manual"],
    )
    .await;

    let mut next = original.clone();
    next.api_key[0].allowed_providers = vec!["kiro".into()];
    replace_config(&state, next);
    assert_catalog(
        &state,
        &listener,
        Some("sk-scope-test"),
        &["claude-scope-inside", "claude-scope-manual"],
    )
    .await;

    let mut next = original.clone();
    next.api_key[0].allowed_models = vec!["gpt-inside".into()];
    replace_config(&state, next);
    assert_catalog(
        &state,
        &listener,
        Some("sk-scope-test"),
        &["copilot/gpt-inside"],
    )
    .await;

    let mut next = original;
    next.provider
        .iter_mut()
        .find(|provider| provider.id == "copilot")
        .unwrap()
        .enabled = false;
    replace_config(&state, next);
    assert_catalog(
        &state,
        &listener,
        Some("sk-scope-test"),
        &["kiro/claude-scope-inside", "kiro/claude-scope-manual"],
    )
    .await;
}
