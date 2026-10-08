use crosslane_core::account::{Account, AuthMethod, Credentials, Usage};
use crosslane_core::config::{Config, ProviderConfig};
use crosslane_core::paths::Paths;
use crosslane_store::accounts::AccountStore;
use crosslane_store::config_loader::ConfigHandle;
use tempfile::TempDir;

use super::*;

fn sample_account(id: &str, email: &str, enabled: bool) -> Account {
    Account {
        id: id.into(),
        email: email.into(),
        label: None,
        enabled,
        machine_id: "a".repeat(64),
        profile_arn: None,
        upstream_user_id: None,
        credentials: Credentials {
            access_token: "at-secret".into(),
            refresh_token: Some("rt-secret".into()),
            client_id: Some("cid".into()),
            client_secret: Some("cs-secret".into()),
            region: "us-east-1".into(),
            expires_at: 1_700_000_000,
            auth_method: AuthMethod::Idc,
        },
        usage: None,
        subscription: None,
        tags: vec![],
        created_at: 0,
        credit_exhausted: false,
    }
}

#[test]
fn sso_identity_matching_accepts_idc_username_variants_but_rejects_other_users() {
    assert!(sso_identities_match(
        "alice@example.com",
        "ALICE@example.com"
    ));
    assert!(sso_identities_match(
        "kiro.svc.70@patsnap.com",
        "kirosvc.70"
    ));
    assert!(sso_identities_match(
        "kiro.svc.51@patsnap.com",
        "kirosvc.51 kirosvc.51"
    ));
    assert!(sso_identities_match(
        "kiro.svc.59@patsnap.com",
        "kirosvc.59 kirosvc.5"
    ));
    assert!(!sso_identities_match(
        "kiro.svc.70@patsnap.com",
        "kirosvc.41"
    ));
    assert!(!sso_identities_match(
        "kiro.svc.70@patsnap.com",
        "kirosvc.41 kirosvc.41"
    ));
    assert!(!sso_identities_match("", ""));
}

#[test]
fn sso_identity_validation_uses_stable_user_id_instead_of_display_name() {
    let limits: crosslane_kiro::UsageLimits = serde_json::from_value(serde_json::json!({
        "userInfo": {
            "email": "unrelated display name",
            "userId": "stable-user-71"
        }
    }))
    .expect("usage limits");
    assert_eq!(
        authenticated_sso_user_id(&limits).expect("stable identity"),
        "stable-user-71"
    );

    let error = authenticated_sso_user_id(&crosslane_kiro::UsageLimits::default())
        .expect_err("missing stable identity must be rejected");
    assert!(error.message.contains("stable user ID"));
    assert!(error.message.contains("not saved"));
}

async fn state_with(accounts: Vec<Account>) -> (TempDir, Arc<AppState>) {
    let directory = tempfile::tempdir().expect("tempdir");
    let paths = Paths::from_env_values(
        Some(directory.path().to_str().expect("utf8")),
        None,
        None,
        None,
    );
    crosslane_store::bootstrap::ensure_layout(&paths)
        .await
        .expect("bootstrap");
    let mut store = AccountStore::load(&paths.accounts_file)
        .await
        .expect("load");
    for account in accounts {
        store.insert(account).expect("insert");
    }
    store.save().await.expect("save");
    let state = Arc::new(AppState::new(
        paths,
        ConfigHandle::new(Config::default()),
        store,
    ));
    state
        .providers
        .reconcile(&state.config.current(), &state.paths.data_dir)
        .await
        .expect("reconcile providers");
    (directory, state)
}

fn expect_ok(response: Response) -> serde_json::Value {
    match response {
        Response::Ok { result, .. } => result,
        Response::Err { error, .. } => {
            panic!("expected ok, got {}: {}", error.code, error.message)
        }
    }
}

async fn state_with_copilot() -> (TempDir, Arc<AppState>) {
    let (directory, state) = state_with(vec![sample_account(
        "acc_00000001",
        "kiro@example.com",
        true,
    )])
    .await;
    let provider_dir = state.paths.data_dir.join("providers/copilot");
    tokio::fs::create_dir_all(&provider_dir).await.unwrap();
    tokio::fs::write(provider_dir.join("accounts.json"),serde_json::json!([{
        "id":"acc_b85b28b7","github_host":"github.com","github_user_id":42,"login":"octocat","email":"octocat@example.com",
        "tags":[],"enabled":true,"credentials":{"github_access_token":"fixture-only"},"created_at":0,"supported_models":[]
    }]).to_string()).await.unwrap();
    let mut config = state.config.current().as_ref().clone();
    config.provider = vec![
        ProviderConfig::default(),
        ProviderConfig {
            id: "copilot".into(),
            kind: "copilot".into(),
            ..ProviderConfig::default()
        },
    ];
    state.config.replace(config.clone());
    state
        .providers
        .reconcile(&config, &state.paths.data_dir)
        .await
        .unwrap();
    (directory, state)
}

#[tokio::test]
async fn copilot_tag_resolves_bare_ids_for_old_and_new_clients_and_persists() {
    let (_directory, state) = state_with_copilot().await;
    for method in [method::ACCOUNT_TAG, method::V2_ACCOUNT_TAG] {
        let result = expect_ok(
            dispatch(
                &state,
                Request::new(
                    1,
                    method,
                    serde_json::json!({
                        "id":"acc_b85b28b7","add":[" copilot-test ","copilot-test"],"remove":[]
                    }),
                ),
            )
            .await,
        );
        assert_eq!(result["tags"], serde_json::json!(["copilot-test"]));
    }
    let bytes = tokio::fs::read(state.paths.data_dir.join("providers/copilot/accounts.json"))
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        value["accounts"][0]["tags"],
        serde_json::json!(["copilot-test"])
    );
    let failure = dispatch(
        &state,
        Request::new(
            2,
            method::V2_ACCOUNT_TAG,
            serde_json::json!({
                "ids":["acc_b85b28b7","missing"],"add":["should-not-apply"]
            }),
        ),
    )
    .await;
    assert!(matches!(failure, Response::Err { .. }));
    let value = expect_ok(
        dispatch(
            &state,
            Request::new(
                3,
                method::V2_ACCOUNT_SHOW,
                serde_json::json!({"id":"acc_b85b28b7"}),
            ),
        )
        .await,
    );
    assert_eq!(value["tags"], serde_json::json!(["copilot-test"]));
}

#[tokio::test]
async fn mixed_provider_tag_batch_validates_duplicates_and_ambiguity_before_mutation() {
    let (_directory, state) = state_with_copilot().await;
    for provider in [Some("copilot"), Some("all"), None] {
        let resolved = providers::resolve_account_ref(&state, provider, "OCTOCAT@EXAMPLE.COM")
            .await
            .unwrap();
        assert_eq!(resolved.provider_id.as_str(), "copilot");
        assert_eq!(resolved.account_id, "acc_b85b28b7");
    }
    let duplicate = dispatch(
        &state,
        Request::new(
            1,
            method::V2_ACCOUNT_TAG,
            serde_json::json!({
                "ids":["acc_b85b28b7","copilot/octocat"],"add":["duplicate"]
            }),
        ),
    )
    .await;
    assert!(matches!(duplicate, Response::Err { .. }));
    let mixed = expect_ok(
        dispatch(
            &state,
            Request::new(
                2,
                method::V2_ACCOUNT_TAG,
                serde_json::json!({
                    "ids":["acc_00000001","acc_b85b28b7"],"add":["shared"]
                }),
            ),
        )
        .await,
    );
    assert_eq!(mixed["accounts"].as_array().unwrap().len(), 2);
    assert_eq!(mixed["accounts"][0]["tags"], serde_json::json!(["shared"]));
    assert_eq!(mixed["accounts"][1]["tags"], serde_json::json!(["shared"]));
    let conflict = dispatch(
        &state,
        Request::new(
            3,
            method::V2_ACCOUNT_TAG,
            serde_json::json!({
                "id":"copilot/acc_b85b28b7","provider":"kiro","add":["bad"]
            }),
        ),
    )
    .await;
    assert!(matches!(conflict, Response::Err { .. }));
}

#[tokio::test]
async fn copilot_service_pool_uses_tags_qualified_bindings_and_exclusions() {
    let (_directory, state) = state_with_copilot().await;
    expect_ok(
        dispatch(
            &state,
            Request::new(
                1,
                method::V2_ACCOUNT_TAG,
                serde_json::json!({"id":"acc_b85b28b7","add":["team"]}),
            ),
        )
        .await,
    );
    let service = ProxyServiceConfig {
        id: "svc_copilot".into(),
        name: "copilot-test".into(),
        enabled: false,
        host: "127.0.0.1".into(),
        port: 5589,
        allowed_providers: vec!["copilot".into()],
        default_provider: "copilot".into(),
        account_tag: Some(ServiceAccountTags::One("team".into())),
        ..ProxyServiceConfig::default()
    };
    let mut config = state.config.current().as_ref().clone();
    config.proxy_service.push(service.clone());
    state.config.replace(config.clone());
    let result = service_accounts_result(&state, &service).await.unwrap();
    assert_eq!(result["accounts"], serde_json::json!([]));
    assert_eq!(result["provider_accounts"][0]["provider_id"], "copilot");
    let bound = expect_ok(
        dispatch(
            &state,
            Request::new(
                2,
                method::ACCOUNT_SERVICES,
                serde_json::json!({"id":"acc_b85b28b7"}),
            ),
        )
        .await,
    );
    assert_eq!(
        bound["services"][0]["binding_sources"],
        serde_json::json!(["tag:team"])
    );
    assert!(matches!(
        dispatch(
            &state,
            Request::new(
                3,
                method::V2_ACCOUNT_TAG,
                serde_json::json!({"id":"acc_b85b28b7","remove":["team"]})
            )
        )
        .await,
        Response::Err { .. }
    ));
    assert!(matches!(
        dispatch(
            &state,
            Request::new(
                4,
                method::V2_ACCOUNT_REMOVE,
                serde_json::json!({"id":"acc_b85b28b7"})
            )
        )
        .await,
        Response::Err { .. }
    ));
    config.proxy_service[0].excluded_account_ids = vec!["copilot/acc_b85b28b7".into()];
    state.config.replace(config.clone());
    let result = service_accounts_result(&state, &config.proxy_service[0])
        .await
        .unwrap();
    assert_eq!(result["provider_accounts"], serde_json::json!([]));
    expect_ok(
        dispatch(
            &state,
            Request::new(
                5,
                method::V2_ACCOUNT_TAG,
                serde_json::json!({"id":"acc_b85b28b7","add":["unbound"]}),
            ),
        )
        .await,
    );
    let mut manual = service;
    manual.account_tag = Some(ServiceAccountTags::One("no-match".into()));
    manual.account_ids = vec!["copilot/acc_b85b28b7".into()];
    let result = service_accounts_result(&state, &manual).await.unwrap();
    assert_eq!(result["provider_accounts"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn remote_copilot_login_rejects_missing_or_malformed_credentials_without_echoing_secrets() {
    let (_directory, state) = state_with(vec![]).await;
    for (method, params, expected) in [
        (
            method::V2_LOGIN_START,
            serde_json::json!({"provider":"github-copilot","auth":"device-flow-headless"}),
            "requires browser credentials",
        ),
        (
            method::V2_LOGIN_START,
            serde_json::json!({"provider":"github-copilot","auth":"device-flow-headless","browser":{"github_username":"octocat","github_password":{"secret":"never-echo-this"}}}),
            "invalid provider login parameters",
        ),
        (
            method::V2_LOGIN_SUBMIT_CODE,
            serde_json::json!({"provider":"github-copilot","task_id":"login_test","code":{"secret":"never-echo-this"}}),
            "invalid login task",
        ),
    ] {
        let response = dispatch(&state, Request::new(1, method, params)).await;
        let Response::Err { error, .. } = response else {
            panic!("expected remote login parameter error");
        };
        assert_eq!(error.code, 400);
        assert!(error.message.contains(expected));
        assert!(!error.message.contains("never-echo-this"));
    }
}

#[tokio::test]
async fn status_reports_counts_and_empty_hint() {
    let mut protected = sample_account("acc_00000003", "protected@example.com", true);
    protected.usage = Some(Usage {
        current: 97.0,
        limit: 100.0,
        overage_cap: None,
        percent_used: 97.0,
        next_reset_date: None,
        updated_at: 0,
    });
    let mut exhausted = sample_account("acc_00000004", "exhausted@example.com", true);
    exhausted.usage = Some(Usage {
        current: 100.0,
        limit: 100.0,
        overage_cap: None,
        percent_used: 100.0,
        next_reset_date: None,
        updated_at: 0,
    });
    let (_directory, state) = state_with(vec![
        sample_account("acc_00000001", "a@example.com", true),
        sample_account("acc_00000002", "b@example.com", false),
        protected,
        exhausted,
    ])
    .await;
    state.admission.set_maximum(123);
    let admission_guard = state
        .admission
        .try_acquire()
        .expect("legacy admission slot");
    let status: StatusResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(1, method::STATUS, serde_json::json!({})),
        )
        .await,
    ))
    .expect("status");
    assert_eq!(status.account_total, 4);
    assert_eq!(status.account_enabled, 3);
    assert_eq!(status.account_available, 1);
    assert_eq!(status.account_protected, 1);
    assert_eq!(status.account_exhausted, 1);
    assert_eq!(status.account_refreshing, 0);
    assert_eq!(status.listen, "-");
    assert_eq!(status.proxy_service_total, 0);
    assert_eq!(status.proxy_service_running, 0);
    assert_eq!(status.max_concurrent_requests, 123);
    assert_eq!(status.active_requests, 0);
    assert!(!status.ready);
    assert!(status
        .readiness_reasons
        .iter()
        .any(|reason| reason.contains("proxy service")));
    drop(admission_guard);

    let truncated: StatusResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                2,
                method::STATUS,
                serde_json::json!({"start_secs":0,"end_secs":now_secs()}),
            ),
        )
        .await,
    ))
    .expect("truncated status");
    assert!(truncated.stats_truncated);
    assert_eq!(
        truncated.stats_start,
        Some(state.stats.session_started_at())
    );

    let (_directory, empty) = state_with(vec![]).await;
    let empty_status: StatusResult = serde_json::from_value(expect_ok(
        dispatch(
            &empty,
            Request::new(1, method::STATUS, serde_json::json!({})),
        )
        .await,
    ))
    .expect("empty status");
    assert!(empty_status.hint.is_some());
    assert_eq!(
        empty_status.hint.as_deref(),
        Some("无可用账号，请先添加：crosslane account import")
    );
    assert!(!empty_status.ready);
}

#[tokio::test]
async fn an_unused_broken_provider_does_not_degrade_kiro_readiness() {
    let directory = tempfile::tempdir().expect("tempdir");
    let paths = Paths::from_env_values(
        Some(directory.path().to_str().expect("utf8")),
        None,
        None,
        None,
    );
    crosslane_store::bootstrap::ensure_layout(&paths)
        .await
        .expect("bootstrap");
    let provider_dir = paths.data_dir.join("providers/copilot");
    tokio::fs::create_dir_all(&provider_dir)
        .await
        .expect("provider directory");
    tokio::fs::write(provider_dir.join("accounts.json"), "{broken-json")
        .await
        .expect("broken provider state");

    let mut store = AccountStore::load(&paths.accounts_file)
        .await
        .expect("load accounts");
    store
        .insert(sample_account("acc_00000001", "kiro@example.com", true))
        .expect("insert Kiro account");
    let config = Config {
        provider: vec![
            ProviderConfig::default(),
            ProviderConfig {
                id: "copilot".into(),
                kind: "copilot".into(),
                ..ProviderConfig::default()
            },
        ],
        ..Config::default()
    };
    let state = Arc::new(AppState::new(paths, ConfigHandle::new(config), store));
    state
        .providers
        .reconcile(&state.config.current(), &state.paths.data_dir)
        .await
        .expect("provider reconciliation remains available");

    let status: StatusResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(1, method::STATUS, serde_json::json!({})),
        )
        .await,
    ))
    .expect("status");

    assert!(status
        .providers
        .iter()
        .any(|provider| provider.id == "kiro" && provider.status == "ready"));
    assert!(status
        .providers
        .iter()
        .any(|provider| provider.id == "copilot" && provider.status == "unavailable"));
    assert!(!status
        .readiness_reasons
        .iter()
        .any(|reason| reason.contains("provider copilot")));
}

#[tokio::test]
async fn log_files_reports_resolved_paths_and_physical_partitions() {
    let (_directory, state) = state_with(vec![]).await;
    let log_directory = state.paths.data_dir.join("logs");
    std::fs::create_dir_all(&log_directory).expect("log directory");
    let info = log_directory.join("crosslaned-2026-08-23-info.log");
    let error = log_directory.join("crosslaned-2026-08-23-error.1.log");
    std::fs::write(&info, b"info\n").expect("info log");
    std::fs::write(&error, b"failure\n").expect("error log");
    std::fs::write(log_directory.join("unrelated.log"), b"ignore").expect("unrelated log");

    let result: LogFilesResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(1, method::LOG_FILES, serde_json::json!({})),
        )
        .await,
    ))
    .expect("log files result");
    assert_eq!(result.directory, log_directory.display().to_string());
    assert_eq!(
        result.base_path,
        log_directory.join("crosslaned.log").display().to_string()
    );
    assert_eq!(result.files.len(), 2);
    assert!(result.files.iter().any(|file| {
        file.path == info.display().to_string() && file.level == "info" && file.size_bytes == 5
    }));
    assert!(result.files.iter().any(|file| {
        file.path == error.display().to_string() && file.level == "error" && file.size_bytes == 8
    }));

    let paths: ConfigPathResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(2, method::CONFIG_PATH, serde_json::json!({})),
        )
        .await,
    ))
    .expect("config paths");
    assert_eq!(paths.log_directory, result.directory);
    assert_eq!(paths.log_base_path, result.base_path);
}

#[tokio::test]
async fn trace_logs_searches_across_exact_severity_partitions() {
    let (_directory, state) = state_with(vec![]).await;
    let log_directory = state.paths.data_dir.join("logs");
    std::fs::create_dir_all(&log_directory).expect("log directory");
    let trace_id = "trace_0123456789abcdef0123456789abcdef";
    std::fs::write(
        log_directory.join("crosslaned-2026-08-23-info.log"),
        format!(
            "{{\"timestamp\":\"2026-08-23T01:00:00Z\",\"fields\":{{\"message\":\"received\",\"trace_id\":\"{trace_id}\"}}}}\n"
        ),
    )
    .expect("info log");
    std::fs::write(
        log_directory.join("crosslaned-2026-08-23-error.log"),
        format!(
            "{{\"timestamp\":\"2026-08-23T01:00:01Z\",\"span\":{{\"trace_id\":\"{trace_id}\"}},\"fields\":{{\"message\":\"failed\"}}}}\n"
        ),
    )
    .expect("error log");

    let result: LogTraceResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                1,
                method::LOG_TRACE,
                serde_json::json!({"trace_id":trace_id,"tail":100}),
            ),
        )
        .await,
    ))
    .expect("trace log result");

    assert_eq!(result.trace_id, trace_id);
    assert_eq!(result.files_scanned, 2);
    assert_eq!(result.matched_records, 2);
    assert_eq!(result.entries.len(), 2);
    assert_eq!(result.entries[0].level, "info");
    assert_eq!(result.entries[1].level, "error");
    assert!(!result.truncated);
}

#[tokio::test]
async fn stats_default_is_compact_and_detail_restores_recent_requests() {
    let (_directory, state) = state_with(vec![]).await;
    state.stats.record(crate::stats::RequestLog {
        timestamp: now_secs(),
        trace_id: "trace_stats".into(),
        request_id: "req_stats".into(),
        path: "/v1/messages".into(),
        user_agent: "claude-cli/2.1.260 (external, test)".into(),
        provider_id: "kiro".into(),
        model: "claude-sonnet-4.6".into(),
        original_model: "claude-4.6-sonnet".into(),
        kiro_model: "claude-sonnet-4.6".into(),
        account_id: "acc_stats".into(),
        account_name: "Enterprise stats".into(),
        endpoint: "codewhisperer".into(),
        model_path: vec!["claude-4.6-sonnet".into(), "claude-sonnet-4.6".into()],
        model_mapping_rule: None,
        attempts: Vec::new(),
        duration_ms: 25,
        status: 200,
        input_tokens: 120,
        output_tokens: 30,
        credits: 0.5,
        error: None,
        diagnostics: crate::stats::RequestDiagnostics::default(),
    });

    let compact = expect_ok(
        dispatch(
            &state,
            Request::new(1, method::STATS, serde_json::json!({})),
        )
        .await,
    );
    assert_eq!(compact["summary"]["requests"], 1);
    assert_eq!(compact["scope"], "persistent");
    assert_eq!(compact["latency"]["average_ms"], 25);
    assert_eq!(compact["latency"]["p50_ms"], 25);
    assert!(compact.get("stats").is_none());
    assert!(compact.get("grouped").is_none());

    let detail = expect_ok(
        dispatch(
            &state,
            Request::new(
                2,
                method::STATS,
                serde_json::json!({"detail":true,"recent":20,"by":"model"}),
            ),
        )
        .await,
    );
    assert_eq!(
        detail["stats"]["recent_requests"][0]["account_name"],
        "Enterprise stats"
    );
    assert_eq!(
        detail["stats"]["recent_requests"][0]["user_agent"],
        "claude-cli/2.1.260 (external, test)"
    );
    assert_eq!(detail["grouped"]["claude-sonnet-4.6"]["requests"], 1);

    let status: StatusResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(3, method::STATUS, serde_json::json!({})),
        )
        .await,
    ))
    .expect("status");
    assert_eq!(status.stats_scope, "session");
    assert_eq!(status.request_count, 1);
    assert_eq!(status.average_latency_ms, 25);
    assert!(status.stats_start.is_some());
    assert!(status.stats_end.is_some());

    let historical = expect_ok(
        dispatch(
            &state,
            Request::new(
                4,
                method::STATS,
                serde_json::json!({"start_secs":0,"end_secs":60}),
            ),
        )
        .await,
    );
    assert_eq!(historical["summary"]["requests"], 0);
    assert_eq!(historical["range"]["start"], 0);
    assert_eq!(historical["range"]["end"], 60);
    assert_eq!(historical["range"]["truncated"], false);
}

#[tokio::test]
async fn creating_first_proxy_service_returns_a_scoped_api_key() {
    let (_directory, state) = state_with(vec![]).await;
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port");
    let port = listener.local_addr().expect("address").port();
    drop(listener);

    let created: ProxyServiceCreateResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                1,
                method::SERVICE_CREATE,
                serde_json::json!({
                    "name":"first",
                    "port":port,
                    "skip_user_agent_check":true
                }),
            ),
        )
        .await,
    ))
    .expect("created service");
    assert!(created.service.running);
    assert_eq!(created.service.host, "0.0.0.0");
    assert_eq!(created.service.port, port);
    assert!(created.service.skip_user_agent_check);
    assert_eq!(
        created.service.api_key_ids,
        vec![created.api_key.id.clone()]
    );
    assert!(created.api_key.key.starts_with("sk-"));

    let listed: ProxyServiceListResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(2, method::SERVICE_LIST, serde_json::json!({})),
        )
        .await,
    ))
    .expect("service list");
    assert_eq!(listed.services.len(), 1);
    assert!(!serde_json::to_string(&listed)
        .expect("serialize list")
        .contains(&created.api_key.key));

    let hidden: ProxyServiceApiKeysResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                3,
                method::SERVICE_APIKEYS,
                serde_json::json!({"service":"first"}),
            ),
        )
        .await,
    ))
    .expect("hidden service API keys");
    assert_eq!(hidden.service_id, created.service.id);
    assert_eq!(hidden.api_keys.len(), 1);
    assert!(!hidden.api_keys[0].skip_user_agent_check);
    assert!(!hidden.api_keys[0].user_agent_check_enforced);
    assert_eq!(hidden.api_keys[0].user_agent_check_reason, "service_bypass");
    assert!(hidden.api_keys[0].key.is_none());
    assert!(!serde_json::to_string(&hidden)
        .expect("serialize hidden keys")
        .contains(&created.api_key.key));

    let revealed: ProxyServiceApiKeysResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                4,
                method::SERVICE_APIKEYS,
                serde_json::json!({
                    "service":created.service.id,
                    "show_secret":true
                }),
            ),
        )
        .await,
    ))
    .expect("revealed service API keys");
    assert_eq!(
        revealed.api_keys[0].key.as_deref(),
        Some(created.api_key.key.as_str())
    );

    let health: serde_json::Value = reqwest::get(format!("http://127.0.0.1:{port}/health"))
        .await
        .expect("health request")
        .json()
        .await
        .expect("health JSON");
    assert_eq!(health["status"], "ok");
    assert_eq!(health["available_accounts"], 0);

    let deleted: ProxyServiceDeleteResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                5,
                method::SERVICE_DELETE,
                serde_json::json!({"service":"first"}),
            ),
        )
        .await,
    ))
    .expect("deleted service");
    assert_eq!(deleted.service_id, created.service.id);
    assert_eq!(deleted.service_name, created.service.name);
    assert_eq!(deleted.deleted_api_key_ids.len(), 1);
    assert_eq!(deleted.deleted_api_key_ids[0], created.api_key.id);
    assert!(deleted.retained_api_key_ids.is_empty());

    let listed_after_delete: ProxyServiceListResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(6, method::SERVICE_LIST, serde_json::json!({})),
        )
        .await,
    ))
    .expect("service list after delete");
    assert!(listed_after_delete.services.is_empty());
    assert!(!state
        .config
        .current()
        .api_key
        .iter()
        .any(|key| key.id.as_deref() == Some(created.api_key.id.as_str())));
    assert!(state
        .meter
        .authenticate(Some(&created.api_key.key))
        .expect("empty key registry permits unauthenticated requests")
        .is_none());
    let persisted = tokio::fs::read_to_string(&state.paths.config_file)
        .await
        .expect("persisted config");
    assert!(!persisted.contains(&created.api_key.id));
    assert!(!persisted.contains(&created.api_key.key));
    assert!(persisted.contains("# 日志"));
    assert!(persisted.contains("# [[proxy_service]]"));
    state.shutdown.cancel();
}

#[test]
fn default_service_port_follows_the_highest_configured_service() {
    let mut config = Config::default();
    assert_eq!(next_service_port(&config).unwrap(), config.server.port);
    config.proxy_service.push(ProxyServiceConfig {
        port: 5580,
        ..ProxyServiceConfig::default()
    });
    config.proxy_service.push(ProxyServiceConfig {
        port: 6000,
        enabled: false,
        ..ProxyServiceConfig::default()
    });
    assert_eq!(next_service_port(&config).unwrap(), 6001);
    config.proxy_service.push(ProxyServiceConfig {
        port: u16::MAX,
        ..ProxyServiceConfig::default()
    });
    assert!(next_service_port(&config).is_err());
}

#[test]
fn default_service_port_follows_the_effective_listener_port() {
    let mut config = Config::default();
    config.server.port = 62880;
    config.proxy_service.push(ProxyServiceConfig {
        port: config.server.port,
        ..ProxyServiceConfig::default()
    });
    assert_eq!(
        next_service_port_with_override(&config, Some(62881)).unwrap(),
        62882
    );
    assert_eq!(
        crate::http::effective_port_with_override(&config, &config.proxy_service[0], Some(62881)),
        62881
    );
    assert_eq!(
        next_service_port_with_override(&config, Some(1)).unwrap(),
        1024
    );

    // A lower override must not allocate server.port again, since that would
    // resolve to the already occupied override port.
    config.proxy_service.push(ProxyServiceConfig {
        port: 62879,
        ..ProxyServiceConfig::default()
    });
    assert_eq!(
        next_service_port_with_override(&config, Some(62879)).unwrap(),
        62881
    );
}

#[tokio::test]
async fn creating_another_service_without_a_port_uses_the_previous_port_plus_one() {
    let (_directory, state) = state_with(vec![]).await;
    let port = (0..32)
        .find_map(|_| {
            let first = std::net::TcpListener::bind(("127.0.0.1", 0)).ok()?;
            let port = first.local_addr().ok()?.port();
            let next = port.checked_add(1)?;
            let second = std::net::TcpListener::bind(("127.0.0.1", next)).ok()?;
            drop((first, second));
            Some(port)
        })
        .expect("adjacent free ports");
    let first: ProxyServiceCreateResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                1,
                method::SERVICE_CREATE,
                serde_json::json!({"name":"first","host":"127.0.0.1","port":port}),
            ),
        )
        .await,
    ))
    .unwrap();
    let second: ProxyServiceCreateResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                2,
                method::SERVICE_CREATE,
                serde_json::json!({"name":"second","host":"127.0.0.1"}),
            ),
        )
        .await,
    ))
    .unwrap();
    assert_eq!(second.service.port, first.service.port + 1);
    assert!(second.service.running);
    state.shutdown.cancel();
}

#[tokio::test]
async fn service_account_pool_combines_tag_manual_accounts_and_exclusions() {
    let mut tagged = sample_account("acc_00000001", "tagged@example.com", true);
    tagged.tags = vec!["team-a".into()];
    let mut manual = sample_account("acc_00000002", "manual@example.com", true);
    manual.tags = vec!["team-b".into(), "shared".into()];
    let (_directory, state) = state_with(vec![tagged, manual]).await;
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port");
    let port = listener.local_addr().expect("address").port();
    drop(listener);

    let created: ProxyServiceCreateResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                1,
                method::SERVICE_CREATE,
                serde_json::json!({"name":"team","port":port,"account_tag":"team-a"}),
            ),
        )
        .await,
    ))
    .expect("created service");
    assert_eq!(created.service.account_tag.as_deref(), Some("team-a"));

    let initial: ProxyServiceAccountsResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                2,
                method::SERVICE_ACCOUNTS,
                serde_json::json!({"service":"team"}),
            ),
        )
        .await,
    ))
    .expect("initial service accounts");
    assert_eq!(initial.accounts.len(), 1);
    assert_eq!(initial.accounts[0].id, "acc_00000001");

    let delete_blocked = dispatch(
        &state,
        Request::new(
            3,
            method::ACCOUNT_REMOVE,
            serde_json::json!({"id":"acc_00000001"}),
        ),
    )
    .await;
    assert!(matches!(
        delete_blocked,
        Response::Err { error, .. } if error.message.contains("bound to proxy service")
    ));

    let added: ProxyServiceAccountsResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                4,
                method::SERVICE_ACCOUNT_ADD,
                serde_json::json!({"service":"team","accounts":["MANUAL@EXAMPLE.COM"]}),
            ),
        )
        .await,
    ))
    .expect("added account");
    assert_eq!(added.accounts.len(), 2);
    assert_eq!(added.account_ids, ["acc_00000002"]);

    let unchanged_tags = expect_ok(
        dispatch(
            &state,
            Request::new(
                41,
                method::ACCOUNT_TAG,
                serde_json::json!({"id":"acc_00000002","add":["team-b"]}),
            ),
        )
        .await,
    );
    assert_eq!(
        unchanged_tags["tags"],
        serde_json::json!(["shared", "team-b"])
    );

    let removed: ProxyServiceAccountsResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                5,
                method::SERVICE_ACCOUNT_REMOVE,
                serde_json::json!({"service":"team","accounts":["acc_00000001"]}),
            ),
        )
        .await,
    ))
    .expect("removed tagged account");
    assert_eq!(removed.accounts.len(), 1);
    assert_eq!(removed.accounts[0].id, "acc_00000002");
    assert_eq!(removed.excluded_account_ids, ["acc_00000001"]);

    let blocked = dispatch(
        &state,
        Request::new(
            6,
            method::ACCOUNT_TAG,
            serde_json::json!({"id":"acc_00000002","add":["new"]}),
        ),
    )
    .await;
    assert!(matches!(
        blocked,
        Response::Err { error, .. } if error.message.contains("bound to proxy service")
    ));

    expect_ok(
        dispatch(
            &state,
            Request::new(
                7,
                method::ACCOUNT_TAG,
                serde_json::json!({"id":"acc_00000001","add":["new"]}),
            ),
        )
        .await,
    );
    let deleted = expect_ok(
        dispatch(
            &state,
            Request::new(
                8,
                method::ACCOUNT_REMOVE,
                serde_json::json!({"id":"acc_00000001"}),
            ),
        )
        .await,
    );
    assert_eq!(deleted["removed"], "acc_00000001");
    let unchanged = state.config.current().as_ref().clone();
    state
        .apply_config_transaction(&unchanged)
        .await
        .expect("an exclusion may outlive a deleted account");
    state.shutdown.cancel();
}

#[tokio::test]
async fn service_create_with_multiple_tags_uses_the_union_and_reports_every_source() {
    let mut first = sample_account("acc_00000001", "a@example.com", true);
    first.tags = vec!["team-a".into()];
    let mut second = sample_account("acc_00000002", "b@example.com", true);
    second.tags = vec!["team-b".into()];
    let mut both = sample_account("acc_00000003", "c@example.com", true);
    both.tags = vec!["team-a".into(), "team-b".into()];
    let other = sample_account("acc_00000004", "d@example.com", true);
    let (_directory, state) = state_with(vec![first, second, both, other]).await;
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port");
    let port = listener.local_addr().expect("address").port();
    drop(listener);

    let created: ProxyServiceCreateResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                1,
                method::SERVICE_CREATE,
                serde_json::json!({
                    "name":"team",
                    "port":port,
                    "account_tag":["team-b","team-a"]
                }),
            ),
        )
        .await,
    ))
    .expect("multi-tag service");
    assert!(created.service.account_tag.is_none());
    assert_eq!(created.service.account_tags, ["team-a", "team-b"]);

    let pool: ProxyServiceAccountsResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                2,
                method::SERVICE_ACCOUNTS,
                serde_json::json!({"service":"team"}),
            ),
        )
        .await,
    ))
    .expect("multi-tag pool");
    assert!(!pool.uses_global_pool);
    assert_eq!(pool.account_tags, ["team-a", "team-b"]);
    assert_eq!(
        pool.accounts
            .iter()
            .map(|account| account.id.as_str())
            .collect::<Vec<_>>(),
        ["acc_00000001", "acc_00000002", "acc_00000003"]
    );

    let bindings: AccountServicesResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                3,
                method::ACCOUNT_SERVICES,
                serde_json::json!({"id":"c@example.com"}),
            ),
        )
        .await,
    ))
    .expect("account bindings");
    assert_eq!(bindings.services.len(), 1);
    assert_eq!(
        bindings.services[0].binding_sources,
        ["tag:team-a", "tag:team-b"]
    );

    let raw = tokio::fs::read_to_string(&state.paths.config_file)
        .await
        .expect("saved config");
    assert!(raw.contains("account_tag = [\"team-a\", \"team-b\"]"));
    state.shutdown.cancel();
}

#[tokio::test]
async fn account_services_reports_every_effective_binding_and_its_sources() {
    let mut account = sample_account("acc_00000001", "a@example.com", true);
    account.tags = vec!["team-a".into()];
    let (_directory, state) = state_with(vec![account]).await;
    let first_listener =
        std::net::TcpListener::bind(("127.0.0.1", 0)).expect("first ephemeral port");
    let first_port = first_listener.local_addr().expect("first address").port();
    let second_listener =
        std::net::TcpListener::bind(("127.0.0.1", 0)).expect("second ephemeral port");
    let second_port = second_listener.local_addr().expect("second address").port();
    drop(first_listener);
    drop(second_listener);

    for (id, name, port, tag) in [
        (1, "tagged", first_port, Some("team-a")),
        (2, "global", second_port, None),
    ] {
        expect_ok(
            dispatch(
                &state,
                Request::new(
                    id,
                    method::SERVICE_CREATE,
                    serde_json::json!({"name":name,"host":"127.0.0.1","port":port,"account_tag":tag}),
                ),
            )
            .await,
        );
    }
    expect_ok(
        dispatch(
            &state,
            Request::new(
                3,
                method::SERVICE_ACCOUNT_ADD,
                serde_json::json!({"service":"global","accounts":["a@example.com"]}),
            ),
        )
        .await,
    );

    let result: AccountServicesResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                4,
                method::ACCOUNT_SERVICES,
                serde_json::json!({"id":"a@example.com"}),
            ),
        )
        .await,
    ))
    .expect("account services");
    assert_eq!(result.account_id, "acc_00000001");
    assert_eq!(result.account_email, "a@example.com");
    assert_eq!(
        result
            .services
            .iter()
            .map(|service| service.service_name.as_str())
            .collect::<Vec<_>>(),
        ["global", "tagged"]
    );
    assert_eq!(result.services[0].binding_sources, ["global", "manual"]);
    assert_eq!(result.services[1].binding_sources, ["tag:team-a"]);
    assert!(result.services.iter().all(|service| service.running));
    state.shutdown.cancel();
}

#[tokio::test]
async fn service_account_add_cannot_bind_an_account_removed_from_durable_storage() {
    let mut account = sample_account("acc_00000001", "gone@example.com", true);
    account.tags = vec!["team-b".into()];
    let (_directory, state) = state_with(vec![account]).await;
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port");
    let port = listener.local_addr().expect("address").port();
    drop(listener);

    expect_ok(
        dispatch(
            &state,
            Request::new(
                1,
                method::SERVICE_CREATE,
                serde_json::json!({"name":"team","port":port,"account_tag":"team-a"}),
            ),
        )
        .await,
    );
    let mut external = AccountStore::load(&state.paths.accounts_file)
        .await
        .expect("external store");
    external.remove("acc_00000001").expect("removed account");
    external.save().await.expect("save removal");

    let response = dispatch(
        &state,
        Request::new(
            2,
            method::SERVICE_ACCOUNT_ADD,
            serde_json::json!({"service":"team","accounts":["acc_00000001"]}),
        ),
    )
    .await;

    assert!(matches!(
        response,
        Response::Err { error, .. } if error.message.contains("account not found")
    ));
    assert!(state.with_accounts(|store| store.find("acc_00000001").is_none()));
    assert!(state.config.current().proxy_service[0]
        .account_ids
        .is_empty());
    state.shutdown.cancel();
}

#[tokio::test]
async fn enabled_service_must_be_stopped_before_changing_its_account_tag() {
    let mut account = sample_account("acc_00000001", "a@example.com", true);
    account.tags = vec!["team-a".into(), "team-b".into()];
    let (_directory, state) = state_with(vec![account]).await;
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port");
    let port = listener.local_addr().expect("address").port();
    drop(listener);

    expect_ok(
        dispatch(
            &state,
            Request::new(
                1,
                method::SERVICE_CREATE,
                serde_json::json!({"name":"team","port":port,"account_tag":"team-a"}),
            ),
        )
        .await,
    );
    let mut equivalent = state.config.current().as_ref().clone();
    equivalent.proxy_service[0].account_tag = Some(ServiceAccountTags::Many(vec!["team-a".into()]));
    state
        .apply_config_transaction(&equivalent)
        .await
        .expect("equivalent tag representation does not change the pool");

    let mut changed_while_running = state.config.current().as_ref().clone();
    changed_while_running.proxy_service[0].account_tag = Some(ServiceAccountTags::Many(vec![
        "team-a".into(),
        "team-b".into(),
    ]));
    let error = state
        .apply_config_transaction(&changed_while_running)
        .await
        .expect_err("running service tag change must fail");
    assert!(error.contains("must be disabled"), "{error}");

    let mut stopped = state.config.current().as_ref().clone();
    stopped.proxy_service[0].enabled = false;
    state
        .apply_config_transaction(&stopped)
        .await
        .expect("stop service");
    let mut changed = stopped;
    changed.proxy_service[0].account_tag = Some(ServiceAccountTags::Many(vec![
        "team-a".into(),
        "team-b".into(),
    ]));
    state
        .apply_config_transaction(&changed)
        .await
        .expect("change stopped service tag");
    assert_eq!(
        state.config.current().proxy_service[0].selected_account_tags(),
        ["team-a", "team-b"]
    );
    state.shutdown.cancel();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_mutation_waits_for_the_config_file_lock() {
    let (_directory, state) = state_with(vec![]).await;
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port");
    let port = listener.local_addr().expect("address").port();
    drop(listener);
    let transaction = crosslane_store::atomic::lock_file_exclusive(&state.paths.config_file)
        .await
        .expect("lock config transaction");

    let mutation_state = Arc::clone(&state);
    let mut mutation = tokio::spawn(async move {
        dispatch(
            &mutation_state,
            Request::new(
                1,
                method::SERVICE_CREATE,
                serde_json::json!({"name":"locked","port":port}),
            ),
        )
        .await
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut mutation)
            .await
            .is_err(),
        "service mutation bypassed an active config transaction"
    );

    drop(transaction);
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), mutation)
        .await
        .expect("service mutation timed out")
        .expect("join service mutation");
    let created: ProxyServiceCreateResult =
        serde_json::from_value(expect_ok(response)).expect("created service");
    assert_eq!(created.service.name, "locked");
    state.shutdown.cancel();
}

#[test]
fn service_key_cleanup_preserves_keys_shared_with_other_services() {
    let exclusive = ApiKeyConfig {
        id: Some("ak_exclusive".into()),
        name: "exclusive".into(),
        key: "sk-exclusive".into(),
        format: ApiKeyFormat::Sk,
        enabled: true,
        skip_user_agent_check: false,
        credits_limit: None,
        ..ApiKeyConfig::default()
    };
    let shared = ApiKeyConfig {
        id: Some("ak_shared".into()),
        name: "shared".into(),
        key: "sk-shared".into(),
        format: ApiKeyFormat::Sk,
        enabled: true,
        skip_user_agent_check: false,
        credits_limit: None,
        ..ApiKeyConfig::default()
    };
    let removed = ProxyServiceConfig {
        id: "svc_removed".into(),
        name: "removed".into(),
        host: "127.0.0.1".into(),
        port: 5580,
        enabled: true,
        skip_user_agent_check: false,
        api_key_ids: vec!["ak_exclusive".into(), "ak_shared".into()],
        account_tag: None,
        account_ids: Vec::new(),
        excluded_account_ids: Vec::new(),
        created_at: 0,
        ..ProxyServiceConfig::default()
    };
    let remaining = ProxyServiceConfig {
        id: "svc_remaining".into(),
        name: "remaining".into(),
        host: "127.0.0.1".into(),
        port: 5581,
        enabled: true,
        skip_user_agent_check: false,
        api_key_ids: vec!["ak_shared".into()],
        account_tag: None,
        account_ids: Vec::new(),
        excluded_account_ids: Vec::new(),
        created_at: 0,
        ..ProxyServiceConfig::default()
    };
    let mut config = Config {
        api_key: vec![exclusive, shared],
        proxy_service: vec![remaining],
        ..Config::default()
    };

    let (deleted, retained) = remove_unshared_service_api_keys(&mut config, &removed);

    assert_eq!(deleted, ["ak_exclusive"]);
    assert_eq!(retained, ["ak_shared"]);
    assert_eq!(config.api_key.len(), 1);
    assert_eq!(config.api_key[0].id.as_deref(), Some("ak_shared"));
}

#[tokio::test]
async fn account_lifecycle_persists_without_exposing_tokens() {
    let (_directory, state) = state_with(vec![]).await;
    let account = sample_account("acc_00000001", "a@example.com", true);
    let imported: AccountImportResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                1,
                method::ACCOUNT_IMPORT,
                serde_json::json!({"accounts": [account]}),
            ),
        )
        .await,
    ))
    .expect("import");
    assert_eq!(imported.imported, 1);

    let list = expect_ok(
        dispatch(
            &state,
            Request::new(2, method::ACCOUNT_LIST, serde_json::json!({})),
        )
        .await,
    );
    assert_eq!(list["accounts"].as_array().expect("array").len(), 1);
    assert!(!serde_json::to_string(&list)
        .expect("serialize")
        .contains("at-secret"));

    expect_ok(
        dispatch(
            &state,
            Request::new(
                3,
                method::ACCOUNT_TAG,
                serde_json::json!({"id": "a@example.com", "add": ["prod"]}),
            ),
        )
        .await,
    );
    let raw = tokio::fs::read_to_string(&state.paths.accounts_file)
        .await
        .expect("read disk");
    assert!(raw.contains("prod"));
}

#[tokio::test]
async fn account_import_rejects_malformed_tags() {
    let (_directory, state) = state_with(vec![]).await;
    for tags in [vec![" team-a"], vec!["team-a", "team-a"]] {
        let mut account = sample_account("acc_00000001", "a@example.com", true);
        account.tags = tags.into_iter().map(str::to_owned).collect();
        let response = dispatch(
            &state,
            Request::new(
                1,
                method::ACCOUNT_IMPORT,
                serde_json::json!({"accounts": [account]}),
            ),
        )
        .await;
        assert!(matches!(response, Response::Err { .. }));
    }
}

#[tokio::test]
async fn account_tag_batch_updates_all_accounts_and_preserves_single_result() {
    let mut first = sample_account("acc_00000001", "a@example.com", true);
    first.tags = vec!["legacy".into()];
    let second = sample_account("acc_00000002", "b@example.com", true);
    let (_directory, state) = state_with(vec![first, second]).await;

    let result: AccountTagBatchResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                1,
                method::ACCOUNT_TAG,
                serde_json::json!({
                    "ids": ["a@example.com", "acc_00000002"],
                    "add": [" test ", "prod"],
                    "remove": ["legacy"]
                }),
            ),
        )
        .await,
    ))
    .expect("batch tag result");
    assert_eq!(result.accounts.len(), 2);
    assert_eq!(result.accounts[0].id, "acc_00000001");
    assert_eq!(result.accounts[0].tags, ["prod", "test"]);
    assert_eq!(result.accounts[1].id, "acc_00000002");
    assert_eq!(result.accounts[1].tags, ["prod", "test"]);

    let single = expect_ok(
        dispatch(
            &state,
            Request::new(
                2,
                method::ACCOUNT_TAG,
                serde_json::json!({"id": "a@example.com", "add": ["single"]}),
            ),
        )
        .await,
    );
    assert_eq!(single["id"], "a@example.com");
    assert_eq!(
        single["tags"],
        serde_json::json!(["prod", "single", "test"])
    );
    assert!(single.get("accounts").is_none());
    assert_eq!(
        state.with_accounts(|store| store.find("b@example.com").expect("second").tags.clone()),
        ["prod", "test"]
    );
    let persisted = AccountStore::load(&state.paths.accounts_file)
        .await
        .expect("reload accounts");
    assert_eq!(
        persisted.find("a@example.com").expect("first on disk").tags,
        ["prod", "single", "test"]
    );
    assert_eq!(
        persisted
            .find("b@example.com")
            .expect("second on disk")
            .tags,
        ["prod", "test"]
    );
}

#[tokio::test]
async fn account_tag_batch_rejects_invalid_targets_without_partial_changes() {
    let first = sample_account("acc_00000001", "a@example.com", true);
    let mut bound = sample_account("acc_00000002", "b@example.com", true);
    bound.tags = vec!["team-a".into()];
    let (_directory, state) = state_with(vec![first, bound]).await;
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port");
    let port = listener.local_addr().expect("address").port();
    drop(listener);
    expect_ok(
        dispatch(
            &state,
            Request::new(
                1,
                method::SERVICE_CREATE,
                serde_json::json!({"name":"team","port":port,"account_tag":"team-a"}),
            ),
        )
        .await,
    );
    let original_accounts = tokio::fs::read(&state.paths.accounts_file)
        .await
        .expect("original accounts");

    for (ids, expected_error) in [
        (
            vec!["a@example.com", "missing@example.com"],
            "account not found",
        ),
        (
            vec!["a@example.com", "b@example.com"],
            "bound to proxy service",
        ),
        (
            vec!["a@example.com", "acc_00000001"],
            "specified more than once",
        ),
    ] {
        let response = dispatch(
            &state,
            Request::new(
                2,
                method::ACCOUNT_TAG,
                serde_json::json!({"ids": ids, "add": ["new"]}),
            ),
        )
        .await;
        assert!(matches!(
            response,
            Response::Err { error, .. } if error.message.contains(expected_error)
        ));
        assert!(state.with_accounts(|store| store
            .find("a@example.com")
            .expect("first")
            .tags
            .is_empty()));
        assert_eq!(
            state.with_accounts(|store| store.find("b@example.com").expect("bound").tags.clone()),
            ["team-a"]
        );
        assert_eq!(
            tokio::fs::read(&state.paths.accounts_file)
                .await
                .expect("unchanged accounts"),
            original_accounts
        );
    }
    for params in [
        serde_json::json!({"ids": [], "add": ["new"]}),
        serde_json::json!({"id": "a@example.com", "ids": ["b@example.com"], "add": ["new"]}),
    ] {
        assert!(matches!(
            dispatch(&state, Request::new(3, method::ACCOUNT_TAG, params)).await,
            Response::Err { .. }
        ));
        assert!(state.with_accounts(|store| store
            .find("a@example.com")
            .expect("first")
            .tags
            .is_empty()));
    }
    state.shutdown.cancel();
}

#[tokio::test]
async fn account_import_applies_requested_tags_to_every_account() {
    let (_directory, state) = state_with(vec![]).await;
    let mut first = sample_account("acc_00000001", "a@example.com", true);
    first.tags = vec!["existing".into()];
    let second = sample_account("acc_00000002", "b@example.com", true);
    let imported: AccountImportResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                1,
                method::ACCOUNT_IMPORT,
                serde_json::json!({
                    "accounts":[first, second],
                    "tags":[" batch ", "prod", "prod"]
                }),
            ),
        )
        .await,
    ))
    .expect("import result");
    assert_eq!(imported.imported, 2);
    assert_eq!(
        state.with_accounts(|store| store.find("a@example.com").expect("first").tags.clone()),
        ["batch", "existing", "prod"]
    );
    assert_eq!(
        state.with_accounts(|store| store.find("b@example.com").expect("second").tags.clone()),
        ["batch", "prod"]
    );
}

#[tokio::test]
async fn account_lists_and_exports_default_to_email_order() {
    let (_directory, state) = state_with(vec![
        sample_account("acc_00000001", "z@example.com", true),
        sample_account("acc_00000002", "a@example.com", true),
        sample_account("acc_00000003", "B@example.com", true),
    ])
    .await;

    let list: AccountListResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(1, method::ACCOUNT_LIST, serde_json::json!({})),
        )
        .await,
    ))
    .expect("account list");
    assert_eq!(
        list.accounts
            .iter()
            .map(|account| account.email.as_str())
            .collect::<Vec<_>>(),
        ["a@example.com", "B@example.com", "z@example.com"]
    );

    let by_id: AccountListResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(2, method::ACCOUNT_LIST, serde_json::json!({"sort":"id"})),
        )
        .await,
    ))
    .expect("account list by ID");
    assert_eq!(
        by_id
            .accounts
            .iter()
            .map(|account| account.email.as_str())
            .collect::<Vec<_>>(),
        ["z@example.com", "a@example.com", "B@example.com"]
    );

    let exported = expect_ok(
        dispatch(
            &state,
            Request::new(
                3,
                method::ACCOUNT_EXPORT,
                serde_json::json!({"redact":true}),
            ),
        )
        .await,
    );
    assert_eq!(
        exported
            .as_array()
            .expect("exported accounts")
            .iter()
            .map(|account| account["email"].as_str().expect("email"))
            .collect::<Vec<_>>(),
        ["a@example.com", "B@example.com", "z@example.com"]
    );

    let invalid = dispatch(
        &state,
        Request::new(
            4,
            method::ACCOUNT_LIST,
            serde_json::json!({"sort":"unknown"}),
        ),
    )
    .await;
    assert!(matches!(
        invalid,
        Response::Err { error, .. } if error.message.contains("unsupported account sort field")
    ));
}

#[tokio::test]
async fn provider_account_list_accepts_null_enabled_filter_from_existing_clients() {
    let (_directory, state) =
        state_with(vec![sample_account("acc_00000001", "a@example.com", true)]).await;

    let result: ProviderAccountListResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                1,
                method::V2_ACCOUNT_LIST,
                serde_json::json!({"provider":"kiro","enabled_only":null}),
            ),
        )
        .await,
    ))
    .expect("provider account list");
    assert_eq!(result.accounts.len(), 1);
}

#[tokio::test]
async fn administrative_lists_use_stable_name_order() {
    let (_directory, state) = state_with(vec![]).await;
    let mut config = Config::default();
    config.webhook = ["Zulu", "alpha"]
        .map(|name| crosslane_core::config::WebhookConfig {
            name: name.into(),
            kind: "custom".into(),
            url: format!("https://example.com/{name}"),
            enabled: true,
            events: vec![],
            dingtalk_sign: None,
            telegram_chat_id: None,
            custom_template: None,
        })
        .into();
    config.api_key = [
        ("ak_zulu", "Zulu key", "secret-zulu"),
        ("ak_alpha", "alpha key", "secret-alpha"),
    ]
    .map(|(id, name, key)| ApiKeyConfig {
        id: Some(id.into()),
        name: name.into(),
        key: key.into(),
        format: ApiKeyFormat::Sk,
        enabled: true,
        skip_user_agent_check: false,
        credits_limit: None,
        ..ApiKeyConfig::default()
    })
    .into();
    config.proxy_service = [
        ("svc_zulu", "Zulu service", 6001),
        ("svc_alpha", "alpha service", 6002),
    ]
    .map(|(id, name, port)| ProxyServiceConfig {
        id: id.into(),
        name: name.into(),
        host: "127.0.0.1".into(),
        port,
        enabled: false,
        skip_user_agent_check: false,
        api_key_ids: vec!["ak_zulu".into(), "ak_alpha".into()],
        account_tag: None,
        account_ids: Vec::new(),
        excluded_account_ids: Vec::new(),
        created_at: 0,
        ..ProxyServiceConfig::default()
    })
    .into();
    state.config.replace(config);

    let services: ProxyServiceListResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(1, method::SERVICE_LIST, serde_json::json!({})),
        )
        .await,
    ))
    .expect("service list");
    assert_eq!(
        services
            .services
            .iter()
            .map(|service| service.name.as_str())
            .collect::<Vec<_>>(),
        ["alpha service", "Zulu service"]
    );

    let keys: ProxyServiceApiKeysResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                2,
                method::SERVICE_APIKEYS,
                serde_json::json!({"service":"svc_alpha","show_secret":false}),
            ),
        )
        .await,
    ))
    .expect("service API keys");
    assert_eq!(
        keys.api_keys
            .iter()
            .map(|key| key.name.as_str())
            .collect::<Vec<_>>(),
        ["alpha key", "Zulu key"]
    );

    let webhooks = expect_ok(
        dispatch(
            &state,
            Request::new(3, method::WEBHOOK_LIST, serde_json::json!({})),
        )
        .await,
    );
    assert_eq!(
        webhooks
            .as_array()
            .expect("webhook list")
            .iter()
            .map(|target| target["name"].as_str().expect("name"))
            .collect::<Vec<_>>(),
        ["alpha", "Zulu"]
    );

    let mut models = ["z-model", "A-model", "b-model"].map(|model_id| crosslane_kiro::ModelInfo {
        model_id: model_id.into(),
        model_name: model_id.into(),
        description: String::new(),
        rate_multiplier: None,
        token_limits: None,
        additional_model_request_fields_schema: None,
    });
    sort_models_for_display(&mut models);
    assert_eq!(
        models
            .iter()
            .map(|model| model.model_id.as_str())
            .collect::<Vec<_>>(),
        ["A-model", "b-model", "z-model"]
    );
}

#[tokio::test]
async fn account_list_distinguishes_low_credit_from_exhaustion() {
    let ready = sample_account("acc_00000001", "ready@example.com", true);
    let mut protected = sample_account("acc_00000002", "protected@example.com", true);
    protected.usage = Some(Usage {
        current: 97.0,
        limit: 100.0,
        overage_cap: None,
        percent_used: 97.0,
        next_reset_date: None,
        updated_at: 0,
    });
    let mut exhausted = sample_account("acc_00000003", "exhausted@example.com", true);
    exhausted.usage = Some(Usage {
        current: 100.0,
        limit: 100.0,
        overage_cap: None,
        percent_used: 100.0,
        next_reset_date: None,
        updated_at: 0,
    });
    let (_directory, state) = state_with(vec![ready, protected, exhausted]).await;

    let list: AccountListResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(1, method::ACCOUNT_LIST, serde_json::json!({})),
        )
        .await,
    ))
    .expect("account list");
    let health_by_email = list
        .accounts
        .iter()
        .map(|account| (account.email.as_str(), account.health.as_deref()))
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(health_by_email["ready@example.com"], Some("available"));
    assert_eq!(health_by_email["protected@example.com"], Some("low_credit"));
    assert_eq!(health_by_email["exhausted@example.com"], Some("exhausted"));

    let protected_only: AccountListResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                2,
                method::ACCOUNT_LIST,
                serde_json::json!({"status":"low_credit"}),
            ),
        )
        .await,
    ))
    .expect("protected accounts");
    assert_eq!(protected_only.accounts.len(), 1);
    assert_eq!(protected_only.accounts[0].email, "protected@example.com");

    let detail: AccountDetail = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                3,
                method::ACCOUNT_SHOW,
                serde_json::json!({"id":"protected@example.com"}),
            ),
        )
        .await,
    ))
    .expect("account detail");
    assert_eq!(detail.summary.health.as_deref(), Some("low_credit"));
}

#[tokio::test]
async fn overage_config_switch_updates_account_health_and_visible_limit() {
    let mut account = sample_account("acc_00000001", "overage@example.com", true);
    account.usage = Some(Usage {
        current: 10_000.0,
        limit: 20_000.0,
        overage_cap: Some(10_000.0),
        percent_used: 50.0,
        next_reset_date: None,
        updated_at: 1,
    });
    account.credit_exhausted = true;
    let (_directory, state) = state_with(vec![account]).await;
    let list = |id| Request::new(id, method::ACCOUNT_LIST, serde_json::json!({}));

    let disabled: AccountListResult =
        serde_json::from_value(expect_ok(dispatch(&state, list(1)).await)).expect("account list");
    assert_eq!(disabled.accounts[0].health.as_deref(), Some("exhausted"));
    assert_eq!(disabled.accounts[0].credit_limit, Some(10_000.0));
    let v2_disabled: ProviderAccountListResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(3, method::V2_ACCOUNT_LIST, serde_json::json!({})),
        )
        .await,
    ))
    .expect("provider account list");
    assert_eq!(v2_disabled.accounts[0].quota_limit, Some(10_000.0));

    let mut enabled_config = state.runtime_config_snapshot();
    enabled_config.pool.enable_overage = true;
    state
        .apply_config_transaction(&enabled_config)
        .await
        .expect("apply overage config");
    let enabled: AccountListResult =
        serde_json::from_value(expect_ok(dispatch(&state, list(2)).await)).expect("account list");
    assert_eq!(enabled.accounts[0].health.as_deref(), Some("available"));
    assert_eq!(enabled.accounts[0].credit_limit, Some(20_000.0));
    let v2_enabled: ProviderAccountListResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(4, method::V2_ACCOUNT_LIST, serde_json::json!({})),
        )
        .await,
    ))
    .expect("provider account list");
    assert_eq!(v2_enabled.accounts[0].quota_limit, Some(20_000.0));
    assert_eq!(v2_enabled.accounts[0].details["overage_enabled"], true);

    enabled_config.pool.max_overage_credits_per_account = Some(500.0);
    state
        .apply_config_transaction(&enabled_config)
        .await
        .expect("apply per-account overage cap");
    let capped: AccountListResult =
        serde_json::from_value(expect_ok(dispatch(&state, list(5)).await)).expect("capped list");
    assert_eq!(capped.accounts[0].health.as_deref(), Some("available"));
    assert_eq!(capped.accounts[0].credit_limit, Some(10_500.0));
    let v2_capped: ProviderAccountListResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(6, method::V2_ACCOUNT_LIST, serde_json::json!({})),
        )
        .await,
    ))
    .expect("capped provider account list");
    assert_eq!(v2_capped.accounts[0].quota_limit, Some(10_500.0));
    assert_eq!(
        v2_capped.accounts[0].details["max_overage_credits_per_account"],
        500.0
    );
    assert_eq!(v2_capped.accounts[0].details["overage_cap"], 10_000.0);
    assert_eq!(v2_capped.accounts[0].details["kiro_overage_cap"], 10_000.0);
    assert_eq!(
        v2_capped.accounts[0].details["kiro_overage_total_limit"],
        20_000.0
    );
    assert_eq!(
        v2_capped.accounts[0].details["effective_overage_cap"],
        500.0
    );

    enabled_config.pool.max_overage_credits_per_account = Some(0.0);
    state
        .apply_config_transaction(&enabled_config)
        .await
        .expect("apply zero overage cap");
    let capped_to_zero: AccountListResult =
        serde_json::from_value(expect_ok(dispatch(&state, list(7)).await)).expect("zero cap list");
    assert_eq!(
        capped_to_zero.accounts[0].health.as_deref(),
        Some("exhausted")
    );
    assert_eq!(capped_to_zero.accounts[0].credit_limit, Some(10_000.0));
}

#[tokio::test]
async fn model_resolution_uses_each_accounts_real_model_cache() {
    let account = sample_account("acc_00000001", "enterprise@example.com", true);
    let mut protected = sample_account("acc_00000002", "protected@example.com", true);
    protected.usage = Some(Usage {
        current: 99.0,
        limit: 100.0,
        overage_cap: None,
        percent_used: 99.0,
        next_reset_date: None,
        updated_at: 0,
    });
    let (_directory, state) = state_with(vec![account, protected]).await;
    state
        .pool()
        .get("acc_00000001")
        .await
        .expect("runtime account")
        .set_supported_models(["claude-opus-5".into(), "claude-sonnet-4.6".into()])
        .await;
    state
        .pool()
        .get("acc_00000002")
        .await
        .expect("protected runtime account")
        .set_supported_models(["claude-opus-4.8".into()])
        .await;

    let result: ModelResolutionResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(
                1,
                method::MODEL_RESOLVE,
                serde_json::json!({"model":"opus5"}),
            ),
        )
        .await,
    ))
    .expect("model resolution result");

    assert_eq!(result.input_model, "opus5");
    assert_eq!(result.mapped_model, "opus5");
    assert_eq!(result.resolved_model.as_deref(), Some("claude-opus-5"));
    assert_eq!(result.possible_models, ["claude-opus-5"]);
    assert_eq!(result.matched_accounts, 1);
    assert_eq!(result.total_accounts, 1);
    assert_eq!(result.accounts.len(), 2);
    assert_eq!(result.accounts[0].model_source, "account_cache");
    assert!(result.accounts[0].schedulable);
    assert_eq!(result.accounts[0].health, "available");
    assert_eq!(
        result.accounts[0].resolved_model.as_deref(),
        Some("claude-opus-5")
    );
    assert!(!result.accounts[1].schedulable);
    assert_eq!(result.accounts[1].health, "low_credit");
    assert_eq!(
        result.accounts[1].resolved_model.as_deref(),
        Some("claude-opus-4.8")
    );
}

#[tokio::test]
async fn config_reload_keeps_old_value_on_error() {
    let (_directory, state) = state_with(vec![]).await;
    tokio::fs::write(&state.paths.config_file, "[server\nport = ")
        .await
        .expect("break");
    let result: ConfigReloadResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(1, method::CONFIG_RELOAD, serde_json::json!({})),
        )
        .await,
    ))
    .expect("reload");
    assert!(!result.applied);
    assert_eq!(state.config.current().server.port, 5580);
}

#[tokio::test]
async fn config_reload_applies_service_defaults_but_not_socket_fields() {
    let (_directory, state) = state_with(vec![]).await;
    tokio::fs::write(
        &state.paths.config_file,
        "[server]\nport = 6100\n\n[features]\nenable_prompt_cache = true\n",
    )
    .await
    .expect("edit");
    let result: ConfigReloadResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(1, method::CONFIG_RELOAD, serde_json::json!({})),
        )
        .await,
    ))
    .expect("reload");
    assert!(result.applied);
    assert!(result.needs_restart.is_empty());
    assert_eq!(state.config.current().server.port, 6100);
    assert!(state.config.current().features.enable_prompt_cache);
}

#[tokio::test]
async fn config_reload_rolls_back_when_proxy_listener_cannot_start() {
    let (_directory, state) = state_with(vec![]).await;
    let occupied = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("occupied port");
    let port = occupied.local_addr().expect("address").port();
    let mut next = state.config.current().as_ref().clone();
    next.api_key.push(ApiKeyConfig {
        id: Some("ak_reload".into()),
        name: "reload".into(),
        key: "sk-reload".into(),
        format: ApiKeyFormat::Sk,
        enabled: true,
        skip_user_agent_check: false,
        credits_limit: None,
        ..ApiKeyConfig::default()
    });
    next.proxy_service.push(ProxyServiceConfig {
        id: "svc_reload".into(),
        name: "reload".into(),
        host: "127.0.0.1".into(),
        port,
        enabled: true,
        skip_user_agent_check: false,
        api_key_ids: vec!["ak_reload".into()],
        account_tag: None,
        account_ids: Vec::new(),
        excluded_account_ids: Vec::new(),
        created_at: 0,
        ..ProxyServiceConfig::default()
    });
    tokio::fs::write(
        &state.paths.config_file,
        toml::to_string_pretty(&next).expect("serialize"),
    )
    .await
    .expect("write config");

    let result: ConfigReloadResult = serde_json::from_value(expect_ok(
        dispatch(
            &state,
            Request::new(1, method::CONFIG_RELOAD, serde_json::json!({})),
        )
        .await,
    ))
    .expect("reload result");

    assert!(!result.applied);
    assert!(result
        .error
        .as_deref()
        .is_some_and(|error| error.contains("svc_reload")));
    assert!(state.config.current().proxy_service.is_empty());
    assert!(state.config.current().api_key.is_empty());
}
