//! Incident-based operator alerts.

use std::sync::Arc;

use crosslane_core::account::{Account, Usage};
use crosslane_notify::{WebhookEvent, WebhookEventKind};
use crosslane_pool::{account_credit_state, effective_credit_limit, AccountCreditState};

use crate::state::AppState;

const RECOVERY_CONFIRMATIONS: u8 = 2;

#[derive(Debug, Clone)]
enum CreditObservation {
    Unknown,
    Known {
        state: AccountCreditState,
        usage: Option<Usage>,
        generation: Option<u64>,
    },
}

pub async fn sync_quota_incidents(state: &Arc<AppState>) {
    let _sync = state.lock_quota_alert_sync().await;
    let accounts = state.pool().snapshot().await;
    let config = state.runtime_config_snapshot().pool;
    for account in &accounts {
        let upstream_exhausted = runtime_quota_exhausted(state, &account.id).await;
        sync_account_credit_incident(state, account, &config, upstream_exhausted);
    }
    sync_service_quota_inner(state, &accounts).await;
}

pub async fn sync_account_quota(state: &Arc<AppState>, account_id: &str) {
    let _sync = state.lock_quota_alert_sync().await;
    let pool = state.pool();
    let Some(runtime) = pool.get(account_id).await else {
        resolve_account_credit_incidents(state, account_id);
        return;
    };
    let upstream_exhausted = runtime.health() == crosslane_pool::AccountHealth::Exhausted;
    let account = runtime.account.read().await.clone();
    let config = state.runtime_config_snapshot().pool;
    sync_account_credit_incident(state, &account, &config, upstream_exhausted);
}

async fn runtime_quota_exhausted(state: &AppState, account_id: &str) -> bool {
    state
        .pool()
        .get(account_id)
        .await
        .is_some_and(|runtime| runtime.health() == crosslane_pool::AccountHealth::Exhausted)
}

fn sync_account_credit_incident(
    state: &AppState,
    account: &Account,
    config: &crosslane_core::config::PoolConfig,
    upstream_exhausted: bool,
) {
    if !account.enabled {
        resolve_account_credit_incidents(state, &account.id);
        state.clear_credit_transition(&account.id);
        return;
    }
    let notifier = state.notifier();
    let exhausted_active =
        notifier.incident_active(WebhookEventKind::AccountQuotaExhausted, Some(&account.id));
    let protected_active =
        notifier.incident_active(WebhookEventKind::AccountCreditProtected, Some(&account.id));
    match authoritative_credit_observation(state, account, config, upstream_exhausted) {
        CreditObservation::Unknown => {}
        CreditObservation::Known {
            state: AccountCreditState::Available,
            generation,
            ..
        } => {
            if (exhausted_active || protected_active)
                && !transition_confirmed(
                    state,
                    &account.id,
                    generation,
                    AccountCreditState::Available,
                )
            {
                return;
            }
            resolve_account_credit_incidents(state, &account.id);
            state.clear_credit_transition(&account.id);
        }
        CreditObservation::Known {
            state: AccountCreditState::Protected,
            usage: Some(usage),
            generation,
        } => {
            if exhausted_active
                && !transition_confirmed(
                    state,
                    &account.id,
                    generation,
                    AccountCreditState::Protected,
                )
            {
                return;
            }
            state
                .notifier()
                .resolve_incident(WebhookEventKind::AccountQuotaExhausted, Some(&account.id));
            emit_account_credit_protected(state, account, &usage, config);
            state.clear_credit_transition(&account.id);
        }
        CreditObservation::Known {
            state: AccountCreditState::Exhausted,
            usage,
            ..
        } => {
            state
                .notifier()
                .resolve_incident(WebhookEventKind::AccountCreditProtected, Some(&account.id));
            emit_account_quota(state, account, usage.as_ref(), config);
            state.clear_credit_transition(&account.id);
        }
        CreditObservation::Known {
            state: AccountCreditState::Protected,
            usage: None,
            ..
        } => {}
    }
}

fn transition_confirmed(
    state: &AppState,
    account_id: &str,
    generation: Option<u64>,
    observed: AccountCreditState,
) -> bool {
    let Some(generation) = generation else {
        return false;
    };
    state.observe_credit_transition(account_id, generation, observed) >= RECOVERY_CONFIRMATIONS
}

fn authoritative_credit_observation(
    state: &AppState,
    account: &Account,
    config: &crosslane_core::config::PoolConfig,
    upstream_exhausted: bool,
) -> CreditObservation {
    let authoritative = state.authoritative_usage(&account.id);
    if upstream_exhausted {
        let exhausted_usage = authoritative.as_ref().and_then(|observation| {
            let limit = effective_credit_limit(&observation.usage, config);
            (limit > 0.0 && observation.usage.current >= limit).then(|| observation.usage.clone())
        });
        return CreditObservation::Known {
            state: AccountCreditState::Exhausted,
            usage: exhausted_usage,
            generation: authoritative.map(|value| value.generation),
        };
    }
    if account.credit_exhausted && !config.enable_overage {
        let authoritative_exhausted = authoritative.as_ref().is_some_and(|observation| {
            let limit = effective_credit_limit(&observation.usage, config);
            limit > 0.0 && observation.usage.current >= limit
        });
        return CreditObservation::Known {
            state: AccountCreditState::Exhausted,
            usage: authoritative_exhausted
                .then(|| authoritative.as_ref().map(|value| value.usage.clone()))
                .flatten(),
            generation: authoritative.map(|value| value.generation),
        };
    }
    let Some(authoritative) = authoritative else {
        return CreditObservation::Unknown;
    };
    let mut observed = account.clone();
    observed.usage = Some(authoritative.usage.clone());
    observed.credit_exhausted = false;
    CreditObservation::Known {
        state: account_credit_state(&observed, config),
        usage: Some(authoritative.usage),
        generation: Some(authoritative.generation),
    }
}

fn resolve_account_credit_incidents(state: &AppState, account_id: &str) {
    let notifier = state.notifier();
    notifier.resolve_incident(WebhookEventKind::AccountCreditProtected, Some(account_id));
    notifier.resolve_incident(WebhookEventKind::AccountQuotaExhausted, Some(account_id));
}

pub async fn sync_service_quota(state: &Arc<AppState>) {
    let _sync = state.lock_quota_alert_sync().await;
    let accounts = state.pool().snapshot().await;
    sync_service_quota_inner(state, &accounts).await;
}

async fn sync_service_quota_inner(state: &Arc<AppState>, accounts: &[Account]) {
    let config = state.runtime_config_snapshot();
    let mut services = config
        .proxy_service
        .iter()
        .filter(|service| service.enabled)
        .filter(|service| !service.name.trim().is_empty())
        .collect::<Vec<_>>();
    services.sort_by_key(|service| service.name.to_ascii_lowercase());
    if services.is_empty() {
        state
            .notifier()
            .resolve_incident(WebhookEventKind::ServiceQuotaExhausted, None);
        state.clear_credit_transition("__service_quota__");
        return;
    }

    let mut exhausted_services = Vec::new();
    let mut has_nonempty_pool = false;
    let mut recovery_is_authoritative = true;
    let mut recovery_generation = None;
    for service in services {
        let enabled = accounts
            .iter()
            .filter(|account| account.enabled && service.includes_account(account))
            .collect::<Vec<_>>();
        if enabled.is_empty() {
            continue;
        }
        has_nonempty_pool = true;
        let mut observations = Vec::with_capacity(enabled.len());
        for account in &enabled {
            let upstream_exhausted = runtime_quota_exhausted(state, &account.id).await;
            observations.push(authoritative_credit_observation(
                state,
                account,
                &config.pool,
                upstream_exhausted,
            ));
        }
        let all_exhausted = observations.iter().all(|observation| {
            matches!(
                observation,
                CreditObservation::Known {
                    state: AccountCreditState::Exhausted,
                    ..
                }
            )
        });
        if all_exhausted {
            exhausted_services.push((service.name.trim().to_owned(), enabled.len()));
            continue;
        }
        recovery_is_authoritative &= observations
            .iter()
            .all(|observation| !matches!(observation, CreditObservation::Unknown));
        recovery_generation = observations
            .iter()
            .filter_map(|observation| match observation {
                CreditObservation::Known { generation, .. } => *generation,
                CreditObservation::Unknown => None,
            })
            .chain(recovery_generation)
            .max();
    }

    if !exhausted_services.is_empty() {
        state.clear_credit_transition("__service_quota__");
        let service_names = exhausted_services
            .iter()
            .map(|(name, count)| format!("`{}`（{count} / {count}）", markdown_code(name)))
            .collect::<Vec<_>>()
            .join("、");
        let exhausted = exhausted_services.len();
        let message = format!(
            "- **代理服务：** {service_names}\n\
             - **账号池状态：** {exhausted} 个服务的启用账号已全部额度耗尽\n\
             - **影响：** 上述服务没有可用额度账号，新的 API 代理请求将被拒绝\n\
             - **处理建议：** 补充对应账号池的额度，或向服务加入有额度的账号"
        );
        state.notifier().emit(WebhookEvent::new(
            WebhookEventKind::ServiceQuotaExhausted,
            "Crosslane API 代理服务额度耗尽",
            message,
        ));
        return;
    }

    if !has_nonempty_pool {
        state
            .notifier()
            .resolve_incident(WebhookEventKind::ServiceQuotaExhausted, None);
        state.clear_credit_transition("__service_quota__");
        return;
    }
    if !recovery_is_authoritative {
        return;
    }
    let notifier = state.notifier();
    let active = notifier.incident_active(WebhookEventKind::ServiceQuotaExhausted, None);
    if active
        && !transition_confirmed(
            state,
            "__service_quota__",
            recovery_generation,
            AccountCreditState::Available,
        )
    {
        return;
    }
    notifier.resolve_incident(WebhookEventKind::ServiceQuotaExhausted, None);
    state.clear_credit_transition("__service_quota__");
}

pub fn emit_token_refresh_failure(
    state: &AppState,
    account_id: &str,
    account_name: &str,
    error: &str,
) {
    let message = format!(
        "- **账号：** `{}`\n\
         - **账号 ID：** `{}`\n\
         - **失败原因：** `{}`\n\
         - **影响：** Token 无法自动续期，该账号可能无法继续代理请求",
        markdown_code(account_name),
        markdown_code(account_id),
        markdown_code(error),
    );
    let mut event = WebhookEvent::new(
        WebhookEventKind::TokenRefreshFailed,
        "Crosslane 账号 Token 刷新失败",
        message,
    );
    event.account_id = Some(account_id.to_owned());
    state.notifier().emit(event);
}

pub fn resolve_token_refresh_failure(state: &AppState, account_id: &str) {
    state
        .notifier()
        .resolve_incident(WebhookEventKind::TokenRefreshFailed, Some(account_id));
}

fn emit_account_quota(
    state: &AppState,
    account: &Account,
    usage: Option<&Usage>,
    config: &crosslane_core::config::PoolConfig,
) {
    let credit = usage
        .map(|usage| {
            format!(
                "{:.2} / {:.2} credits",
                usage.current,
                effective_credit_limit(usage, config)
            )
        })
        .unwrap_or_else(|| "上游已返回额度耗尽".into());
    let message = format!(
        "- **账号：** `{}`\n\
         - **账号 ID：** `{}`\n\
         - **额度：** `{credit}`\n\
         - **影响：** 该账号已停止参与请求调度",
        markdown_code(account.display_name()),
        markdown_code(&account.id),
    );
    let mut event = WebhookEvent::new(
        WebhookEventKind::AccountQuotaExhausted,
        "Crosslane 账号额度耗尽",
        message,
    );
    event.account_id = Some(account.id.clone());
    state.notifier().emit(event);
}

fn emit_account_credit_protected(
    state: &AppState,
    account: &Account,
    usage: &Usage,
    config: &crosslane_core::config::PoolConfig,
) {
    let Some(event) = account_credit_protected_event(account, usage, config) else {
        return;
    };
    state.notifier().emit(event);
}

fn account_credit_protected_event(
    account: &Account,
    usage: &Usage,
    config: &crosslane_core::config::PoolConfig,
) -> Option<WebhookEvent> {
    let limit = effective_credit_limit(usage, config);
    if limit <= 0.0 {
        return None;
    }
    let remaining = (limit - usage.current).max(0.0);
    let remaining_percent = (remaining / limit * 100.0).clamp(0.0, 100.0);
    let threshold = format!("剩余额度 ≤ {:.2} credits", config.low_credit_min_remaining);
    let message = format!(
        "- **账号：** `{}`\n\
         - **账号 ID：** `{}`\n\
         - **额度：** {:.2} / {:.2} credits\n\
         - **剩余：** {remaining:.2} credits（{remaining_percent:.2}%）\n\
         - **保护阈值：** {}\n\
         - **影响：** 账号仍有额度，但已暂停参与请求调度，以保留最后可用额度",
        markdown_code(account.display_name()),
        markdown_code(&account.id),
        usage.current,
        limit,
        threshold,
    );
    let mut event = WebhookEvent::new(
        WebhookEventKind::AccountCreditProtected,
        "Crosslane 账号剩余额度保护",
        message,
    );
    event.account_id = Some(account.id.clone());
    Some(event)
}

fn markdown_code(value: &str) -> String {
    value
        .replace(['\r', '\n'], " ")
        .replace('`', "'")
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use crosslane_core::account::{AuthMethod, Credentials, Usage};
    use crosslane_core::config::{Config, PoolConfig, ProxyServiceConfig};
    use crosslane_store::accounts::AccountStore;
    use crosslane_store::config_loader::ConfigHandle;

    use super::*;

    fn account_with_usage(current: f64, limit: f64) -> Account {
        Account {
            id: "acc_00000001".into(),
            email: "protected@example.com".into(),
            label: None,
            enabled: true,
            machine_id: "0".repeat(64),
            profile_arn: None,
            upstream_user_id: None,
            credentials: Credentials {
                access_token: "token".into(),
                refresh_token: None,
                client_id: None,
                client_secret: None,
                region: "us-east-1".into(),
                expires_at: 1,
                auth_method: AuthMethod::Idc,
            },
            usage: Some(Usage {
                current,
                limit,
                overage_cap: None,
                percent_used: current / limit * 100.0,
                next_reset_date: None,
                updated_at: 1,
            }),
            subscription: None,
            tags: Vec::new(),
            created_at: 1,
            credit_exhausted: false,
        }
    }

    async fn test_state() -> (tempfile::TempDir, AppState) {
        let directory = tempfile::tempdir().expect("tempdir");
        let paths = crosslane_core::paths::Paths::from_env_values(
            Some(directory.path().to_str().expect("utf8")),
            None,
            None,
            None,
        );
        crosslane_store::bootstrap::ensure_layout(&paths)
            .await
            .expect("layout");
        let accounts = AccountStore::load(&paths.accounts_file)
            .await
            .expect("accounts");
        let state = AppState::new(paths, ConfigHandle::new(Config::default()), accounts);
        (directory, state)
    }

    #[test]
    fn protected_credit_event_reports_remaining_credit_and_scheduler_threshold() {
        let account = account_with_usage(97.0, 100.0);

        assert_eq!(
            account_credit_state(&account, &PoolConfig::default()),
            AccountCreditState::Protected
        );

        let event = account_credit_protected_event(
            &account,
            account.usage.as_ref().expect("usage"),
            &PoolConfig::default(),
        )
        .expect("protected credit event");

        assert_eq!(event.kind, WebhookEventKind::AccountCreditProtected);
        assert_eq!(event.account_id.as_deref(), Some("acc_00000001"));
        assert!(event.message.contains("剩余：** 3.00 credits（3.00%）"));
        assert!(event.message.contains("剩余额度 ≤ 4.00 credits"));
        assert!(event.message.contains("已暂停参与请求调度"));
    }

    #[tokio::test]
    async fn optimistic_usage_cannot_change_authoritative_alert_state() {
        let (_directory, state) = test_state().await;
        let mut account = account_with_usage(100.0, 100.0);
        assert!(matches!(
            authoritative_credit_observation(&state, &account, &PoolConfig::default(), false),
            CreditObservation::Unknown
        ));

        let authoritative = Usage {
            current: 97.0,
            limit: 100.0,
            overage_cap: None,
            percent_used: 97.0,
            next_reset_date: None,
            updated_at: 2,
        };
        state.record_authoritative_usage(&account.id, authoritative.clone());
        assert!(matches!(
            authoritative_credit_observation(&state, &account, &PoolConfig::default(), false),
            CreditObservation::Known {
                state: AccountCreditState::Protected,
                ..
            }
        ));

        account.usage.as_mut().expect("usage").current = 100.0;
        assert!(matches!(
            authoritative_credit_observation(&state, &account, &PoolConfig::default(), false),
            CreditObservation::Known {
                state: AccountCreditState::Protected,
                ..
            }
        ));

        let first = state.authoritative_usage(&account.id).expect("observation");
        assert_eq!(
            state.observe_credit_transition(
                &account.id,
                first.generation,
                AccountCreditState::Available,
            ),
            1
        );
        assert_eq!(
            state.observe_credit_transition(
                &account.id,
                first.generation,
                AccountCreditState::Available,
            ),
            1,
            "the same authoritative generation must not count twice"
        );
        state.record_authoritative_usage(&account.id, authoritative);
        let second = state.authoritative_usage(&account.id).expect("observation");
        assert_eq!(
            state.observe_credit_transition(
                &account.id,
                second.generation,
                AccountCreditState::Available,
            ),
            2
        );
    }

    #[tokio::test]
    async fn overage_alerts_use_the_configured_cap_and_preserve_upstream_exhaustion() {
        let (_directory, state) = test_state().await;
        let mut account = account_with_usage(10_000.0, 20_000.0);
        account.usage.as_mut().expect("usage").overage_cap = Some(10_000.0);
        account.credit_exhausted = true;
        let config = PoolConfig {
            enable_overage: true,
            max_overage_credits_per_account: Some(500.0),
            low_credit_min_remaining: 4.0,
            ..PoolConfig::default()
        };

        state.record_authoritative_usage(
            &account.id,
            account.usage.as_ref().expect("usage").clone(),
        );
        assert!(matches!(
            authoritative_credit_observation(&state, &account, &config, false),
            CreditObservation::Known {
                state: AccountCreditState::Available,
                ..
            }
        ));

        let mut near_cap = account.usage.as_ref().expect("usage").clone();
        near_cap.current = 10_499.0;
        state.record_authoritative_usage(&account.id, near_cap);
        assert!(matches!(
            authoritative_credit_observation(&state, &account, &config, false),
            CreditObservation::Known {
                state: AccountCreditState::Available,
                ..
            }
        ));

        let mut at_cap = account.usage.as_ref().expect("usage").clone();
        at_cap.current = 10_500.0;
        state.record_authoritative_usage(&account.id, at_cap);
        assert!(matches!(
            authoritative_credit_observation(&state, &account, &config, false),
            CreditObservation::Known {
                state: AccountCreditState::Exhausted,
                usage: Some(_),
                ..
            }
        ));

        let mut below_cap = account.usage.as_ref().expect("usage").clone();
        below_cap.current = 10_000.0;
        state.record_authoritative_usage(&account.id, below_cap);
        assert!(matches!(
            authoritative_credit_observation(&state, &account, &config, true),
            CreditObservation::Known {
                state: AccountCreditState::Exhausted,
                usage: None,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn overage_does_not_false_alarm_at_base_limit_but_reports_upstream_exhaustion() {
        let directory = tempfile::tempdir().expect("tempdir");
        let paths = crosslane_core::paths::Paths::from_env_values(
            Some(directory.path().to_str().expect("utf8")),
            None,
            None,
            None,
        );
        crosslane_store::bootstrap::ensure_layout(&paths)
            .await
            .expect("layout");
        let mut accounts = AccountStore::load(&paths.accounts_file)
            .await
            .expect("accounts");
        let mut account = account_with_usage(10_000.0, 20_000.0);
        account.usage.as_mut().expect("usage").overage_cap = Some(10_000.0);
        accounts.insert(account.clone()).expect("account");

        let mut config = Config::default();
        config.pool.enable_overage = true;
        config.pool.max_overage_credits_per_account = Some(500.0);
        config.proxy_service.push(ProxyServiceConfig {
            id: "svc_main".into(),
            name: "main".into(),
            host: "127.0.0.1".into(),
            port: 5580,
            ..ProxyServiceConfig::default()
        });
        config.webhook.push(
            serde_json::from_value(serde_json::json!({
                "name":"test",
                "kind":"custom",
                "url":"http://127.0.0.1:9/alerts",
                "events":[
                    WebhookEventKind::AccountQuotaExhausted.as_str(),
                    WebhookEventKind::ServiceQuotaExhausted.as_str()
                ]
            }))
            .expect("webhook config"),
        );
        let state = Arc::new(AppState::new(paths, ConfigHandle::new(config), accounts));
        state.record_authoritative_usage(
            &account.id,
            account.usage.as_ref().expect("usage").clone(),
        );

        sync_quota_incidents(&state).await;
        assert!(!state
            .notifier()
            .incident_active(WebhookEventKind::AccountQuotaExhausted, Some(&account.id)));
        assert!(!state
            .notifier()
            .incident_active(WebhookEventKind::ServiceQuotaExhausted, None));

        state
            .pool()
            .get(&account.id)
            .await
            .expect("runtime")
            .set_health(crosslane_pool::AccountHealth::Exhausted);
        sync_quota_incidents(&state).await;
        assert!(state
            .notifier()
            .incident_active(WebhookEventKind::AccountQuotaExhausted, Some(&account.id)));
        assert!(state
            .notifier()
            .incident_active(WebhookEventKind::ServiceQuotaExhausted, None));
    }

    #[tokio::test]
    async fn service_quota_alert_uses_each_services_effective_account_pool() {
        let directory = tempfile::tempdir().expect("tempdir");
        let paths = crosslane_core::paths::Paths::from_env_values(
            Some(directory.path().to_str().expect("utf8")),
            None,
            None,
            None,
        );
        crosslane_store::bootstrap::ensure_layout(&paths)
            .await
            .expect("layout");
        let mut accounts = AccountStore::load(&paths.accounts_file)
            .await
            .expect("accounts");
        let mut exhausted = account_with_usage(100.0, 100.0);
        exhausted.id = "acc_00000001".into();
        exhausted.email = "exhausted@example.com".into();
        exhausted.tags = vec!["team-a".into()];
        exhausted.credit_exhausted = true;
        accounts.insert(exhausted).expect("exhausted account");
        let mut healthy = account_with_usage(0.0, 100.0);
        healthy.id = "acc_00000002".into();
        healthy.email = "healthy@example.com".into();
        healthy.tags = vec!["team-b".into()];
        accounts.insert(healthy).expect("healthy account");

        let mut config = Config::default();
        config.proxy_service.push(ProxyServiceConfig {
            web_search_provider: None,
            id: "svc_team_a".into(),
            name: "team-a".into(),
            host: "127.0.0.1".into(),
            port: 5580,
            enabled: true,
            skip_user_agent_check: false,
            api_key_ids: Vec::new(),
            account_tag: Some("team-a".into()),
            account_ids: Vec::new(),
            excluded_account_ids: Vec::new(),
            created_at: 0,
            default_provider: String::new(),
            allowed_providers: Vec::new(),
        });
        config.webhook.push(
            serde_json::from_value(serde_json::json!({
                "name":"test",
                "kind":"custom",
                "url":"http://127.0.0.1:9/alerts",
                "events":[WebhookEventKind::ServiceQuotaExhausted.as_str()]
            }))
            .expect("webhook config"),
        );
        let state = Arc::new(AppState::new(paths, ConfigHandle::new(config), accounts));

        sync_service_quota(&state).await;

        assert!(state
            .notifier()
            .incident_active(WebhookEventKind::ServiceQuotaExhausted, None));
    }
}
