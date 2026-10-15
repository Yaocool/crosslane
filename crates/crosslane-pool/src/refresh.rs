//! IdC refresh-token flow with per-account stampede prevention.

use std::future::{ready, Future};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crosslane_core::account::{Account, AuthMethod, Credentials};
use crosslane_kiro::identity::ide_auth_user_agent;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{AccountHealth, AccountPool};

const SOCIAL_REFRESH_URL: &str = "https://prod.us-east-1.auth.desktop.kiro.dev/refreshToken";

#[derive(Debug, Error)]
pub enum RefreshError {
    #[error("account not found")]
    NotFound,
    #[error("account does not contain refreshable credentials")]
    NotRefreshable,
    #[error("token refresh failed: {0}")]
    Upstream(String),
    #[error("refreshed credentials could not be persisted: {0}")]
    Persistence(String),
    #[error("failed to reload credentials after the refresh token was rejected: {0}")]
    CredentialReload(String),
}

/// Credential fields produced by one successful token refresh.
#[derive(Clone)]
pub struct RefreshedCredentials {
    pub account_id: String,
    pub credentials: Credentials,
    pub profile_arn: Option<String>,
}

/// Credential state reloaded from the durable account source after an IdC
/// refresh token was rejected.
#[derive(Clone)]
pub struct ReloadedCredentials {
    pub credentials: Credentials,
    pub profile_arn: Option<String>,
}

/// Result of a refresh operation. A persistence error is deliberately carried
/// as a warning: the upstream may have already invalidated the previous
/// refresh token, so rolling memory back would make the account unusable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshOutcome {
    pub changed: bool,
    pub persistence_error: Option<String>,
}

impl RefreshOutcome {
    fn unchanged() -> Self {
        Self {
            changed: false,
            persistence_error: None,
        }
    }

    fn changed() -> Self {
        Self {
            changed: true,
            persistence_error: None,
        }
    }

    pub fn persisted(&self) -> bool {
        self.persistence_error.is_none()
    }
}

enum RefreshRequestFailure {
    NotRefreshable,
    Upstream {
        status: Option<u16>,
        message: String,
    },
}

impl RefreshRequestFailure {
    fn status(&self) -> Option<u16> {
        match self {
            Self::NotRefreshable => None,
            Self::Upstream { status, .. } => *status,
        }
    }

    fn into_refresh_error(self) -> RefreshError {
        match self {
            Self::NotRefreshable => RefreshError::NotRefreshable,
            Self::Upstream { message, .. } => RefreshError::Upstream(message),
        }
    }
}

#[derive(Clone)]
pub struct TokenRefresher {
    client: Client,
    before_expiry_secs: i64,
    endpoint_override: Option<String>,
}

impl TokenRefresher {
    pub fn new(before_expiry_secs: i64) -> Result<Self, RefreshError> {
        Ok(Self {
            client: Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(30))
                .build()
                .map_err(|error| RefreshError::Upstream(error.to_string()))?,
            before_expiry_secs,
            endpoint_override: None,
        })
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint_override = Some(endpoint.into());
        self
    }

    pub async fn refresh_account(
        &self,
        pool: &AccountPool,
        account_id: &str,
        force: bool,
    ) -> Result<bool, RefreshError> {
        self.refresh_account_and_persist(pool, account_id, force, |_| ready(Ok(())))
            .await
            .map(|outcome| outcome.changed)
    }

    /// Refreshes one account and keeps its singleflight guard until the
    /// rotated credentials have been durably committed by `persist`.
    pub async fn refresh_account_and_persist<F, Fut>(
        &self,
        pool: &AccountPool,
        account_id: &str,
        force: bool,
        persist: F,
    ) -> Result<RefreshOutcome, RefreshError>
    where
        F: FnOnce(RefreshedCredentials) -> Fut,
        Fut: Future<Output = Result<(), String>>,
    {
        self.refresh_account_with_hooks(pool, account_id, force, None, || ready(Ok(None)), persist)
            .await
    }

    /// Refreshes one account with a durable credential reload fallback for an
    /// IdC `400 invalid_request` response.
    pub async fn refresh_account_and_persist_with_reload<R, RFut, P, PFut>(
        &self,
        pool: &AccountPool,
        account_id: &str,
        force: bool,
        reload: R,
        persist: P,
    ) -> Result<RefreshOutcome, RefreshError>
    where
        R: FnOnce() -> RFut,
        RFut: Future<Output = Result<Option<ReloadedCredentials>, String>>,
        P: FnOnce(RefreshedCredentials) -> PFut,
        PFut: Future<Output = Result<(), String>>,
    {
        self.refresh_account_with_hooks(pool, account_id, force, None, reload, persist)
            .await
    }

    /// Refreshes after an upstream authentication failure. `rejected_access_token`
    /// must be the exact token used by the failed request, allowing late
    /// followers to reuse a refresh that completed before they acquired the
    /// per-account singleflight lock.
    pub async fn refresh_after_auth_failure_and_persist<R, RFut, P, PFut>(
        &self,
        pool: &AccountPool,
        account_id: &str,
        rejected_access_token: &str,
        reload: R,
        persist: P,
    ) -> Result<RefreshOutcome, RefreshError>
    where
        R: FnOnce() -> RFut,
        RFut: Future<Output = Result<Option<ReloadedCredentials>, String>>,
        P: FnOnce(RefreshedCredentials) -> PFut,
        PFut: Future<Output = Result<(), String>>,
    {
        self.refresh_account_with_hooks(
            pool,
            account_id,
            true,
            Some(rejected_access_token),
            reload,
            persist,
        )
        .await
    }

    async fn refresh_account_with_hooks<R, RFut, P, PFut>(
        &self,
        pool: &AccountPool,
        account_id: &str,
        force: bool,
        rejected_access_token: Option<&str>,
        reload: R,
        persist: P,
    ) -> Result<RefreshOutcome, RefreshError>
    where
        R: FnOnce() -> RFut,
        RFut: Future<Output = Result<Option<ReloadedCredentials>, String>>,
        P: FnOnce(RefreshedCredentials) -> PFut,
        PFut: Future<Output = Result<(), String>>,
    {
        let state = pool.get(account_id).await.ok_or(RefreshError::NotFound)?;
        let _singleflight = state.refresh_lock.lock().await;
        let mut snapshot = state.account.read().await.clone();
        if rejected_access_token
            .is_some_and(|rejected| snapshot.credentials.access_token.as_str() != rejected)
        {
            return Ok(RefreshOutcome::unchanged());
        }
        if !force && !snapshot.is_token_expiring(now_secs(), self.before_expiry_secs) {
            return Ok(RefreshOutcome::unchanged());
        }
        validate_refreshable(&snapshot)?;
        let health_before_refresh = state.health();
        state.set_health(AccountHealth::Refreshing);
        let mut result = self.request_tokens(&snapshot).await;
        if snapshot.credentials.auth_method == AuthMethod::Idc
            && result
                .as_ref()
                .is_err_and(|failure| failure.status() == Some(400))
        {
            let reloaded = match reload().await {
                Ok(reloaded) => reloaded,
                Err(error) => {
                    state.set_health(AccountHealth::Banned);
                    return Err(RefreshError::CredentialReload(error));
                }
            };
            if let Some(reloaded) = reloaded {
                let access_token_changed =
                    reloaded.credentials.access_token != snapshot.credentials.access_token;
                let refresh_inputs_changed =
                    refresh_inputs_changed(&snapshot.credentials, &reloaded.credentials);
                if access_token_changed || refresh_inputs_changed {
                    {
                        let mut account = state.account.write().await;
                        account.credentials = reloaded.credentials;
                        account.profile_arn = reloaded.profile_arn;
                        snapshot = account.clone();
                    }
                    if access_token_changed {
                        state.restore_health_after_refresh(health_before_refresh);
                        return Ok(RefreshOutcome::changed());
                    }
                    if let Err(error) = validate_refreshable(&snapshot) {
                        state.set_health(AccountHealth::Banned);
                        return Err(error);
                    }
                    result = self.request_tokens(&snapshot).await;
                }
            }
        }
        match result {
            Ok(tokens) => {
                let RefreshResponse {
                    access_token,
                    refresh_token,
                    expires_in,
                    profile_arn,
                } = tokens;
                let refreshed = {
                    let mut account = state.account.write().await;
                    account.credentials.access_token = access_token;
                    if let Some(refresh_token) = refresh_token {
                        account.credentials.refresh_token = Some(refresh_token);
                    }
                    account.credentials.expires_at = now_secs() + expires_in;
                    if let Some(profile_arn) = profile_arn.as_ref() {
                        account.profile_arn = Some(profile_arn.clone());
                    }
                    RefreshedCredentials {
                        account_id: account.id.clone(),
                        credentials: account.credentials.clone(),
                        profile_arn,
                    }
                };
                let persistence_error = persist(refreshed).await.err();
                state.restore_health_after_refresh(health_before_refresh);
                Ok(RefreshOutcome {
                    changed: true,
                    persistence_error,
                })
            }
            Err(failure) => {
                if force || snapshot.credentials.expires_at <= now_secs() {
                    state.set_health(AccountHealth::Banned);
                } else {
                    // A proactive refresh failure must not discard an access
                    // token that is still valid.
                    state.restore_health_after_refresh(health_before_refresh);
                }
                Err(failure.into_refresh_error())
            }
        }
    }

    fn token_request(
        &self,
        snapshot: &Account,
    ) -> Result<reqwest::RequestBuilder, RefreshRequestFailure> {
        let Some(refresh_token) = snapshot.credentials.refresh_token.as_deref() else {
            return Err(RefreshRequestFailure::NotRefreshable);
        };
        Ok(match snapshot.credentials.auth_method {
            AuthMethod::ApiKey => return Err(RefreshRequestFailure::NotRefreshable),
            AuthMethod::Idc => {
                let (Some(client_id), Some(client_secret)) = (
                    snapshot.credentials.client_id.as_deref(),
                    snapshot.credentials.client_secret.as_deref(),
                ) else {
                    return Err(RefreshRequestFailure::NotRefreshable);
                };
                let request = RefreshRequest {
                    client_id,
                    client_secret,
                    grant_type: "refresh_token",
                    refresh_token,
                };
                let endpoint = self.endpoint_override.clone().unwrap_or_else(|| {
                    format!(
                        "https://oidc.{}.amazonaws.com/token",
                        snapshot.credentials.region
                    )
                });
                self.client
                    .post(endpoint)
                    .header(reqwest::header::USER_AGENT, "KiroIDE")
                    .json(&request)
            }
            AuthMethod::Social => {
                // Desktop social OAuth is global; a profile's Kiro region
                // must not select a different token issuer.
                let endpoint = self
                    .endpoint_override
                    .clone()
                    .unwrap_or_else(|| SOCIAL_REFRESH_URL.into());
                self.client
                    .post(endpoint)
                    .header(
                        reqwest::header::USER_AGENT,
                        ide_auth_user_agent(&snapshot.machine_id),
                    )
                    .json(&SocialRefreshRequest { refresh_token })
            }
        })
    }

    async fn request_tokens(
        &self,
        snapshot: &Account,
    ) -> Result<RefreshResponse, RefreshRequestFailure> {
        let response = self.token_request(snapshot)?.send().await;
        match response {
            Ok(response) if response.status().is_success() => {
                let status = response.status().as_u16();
                response.json::<RefreshResponse>().await.map_err(|error| {
                    RefreshRequestFailure::Upstream {
                        status: Some(status),
                        message: error.to_string(),
                    }
                })
            }
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                Err(RefreshRequestFailure::Upstream {
                    status: Some(status.as_u16()),
                    message: format!(
                        "HTTP {status}: {}",
                        body.chars().take(500).collect::<String>()
                    ),
                })
            }
            Err(error) => Err(RefreshRequestFailure::Upstream {
                status: None,
                message: error.to_string(),
            }),
        }
    }

    pub async fn refresh_expiring(&self, pool: &AccountPool) -> Vec<(String, RefreshError)> {
        let accounts = pool.snapshot().await;
        let mut failures = Vec::new();
        for account in accounts {
            if account.is_token_expiring(now_secs(), self.before_expiry_secs) {
                if let Err(error) = self.refresh_account(pool, &account.id, false).await {
                    failures.push((account.id, error));
                }
            }
        }
        failures
    }
}

fn validate_refreshable(account: &Account) -> Result<(), RefreshError> {
    if account.credentials.auth_method == AuthMethod::ApiKey
        || account.credentials.refresh_token.is_none()
    {
        return Err(RefreshError::NotRefreshable);
    }
    if account.credentials.auth_method == AuthMethod::Idc
        && (account.credentials.client_id.is_none() || account.credentials.client_secret.is_none())
    {
        return Err(RefreshError::NotRefreshable);
    }
    Ok(())
}

fn refresh_inputs_changed(previous: &Credentials, reloaded: &Credentials) -> bool {
    previous.refresh_token != reloaded.refresh_token
        || previous.client_id != reloaded.client_id
        || previous.client_secret != reloaded.client_secret
        || previous.region != reloaded.region
        || previous.auth_method != reloaded.auth_method
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RefreshRequest<'a> {
    client_id: &'a str,
    client_secret: &'a str,
    grant_type: &'a str,
    refresh_token: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SocialRefreshRequest<'a> {
    refresh_token: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
    #[serde(default)]
    profile_arn: Option<String>,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use crosslane_core::account::{Account, Credentials};
    use crosslane_core::config::PoolConfig;
    use serde_json::json;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn account() -> Account {
        Account {
            id: "acc_refresh".into(),
            email: "refresh@example.com".into(),
            label: None,
            enabled: true,
            machine_id: "a".repeat(64),
            profile_arn: None,
            upstream_user_id: None,
            credentials: Credentials {
                access_token: "old-access-token".into(),
                refresh_token: Some("refresh-token".into()),
                client_id: None,
                client_secret: None,
                region: "us-east-1".into(),
                expires_at: 0,
                auth_method: AuthMethod::Social,
            },
            usage: None,
            subscription: None,
            tags: Vec::new(),
            created_at: 0,
            credit_exhausted: false,
        }
    }

    #[test]
    fn refresh_requests_use_the_token_issuer_instead_of_the_profile_region() {
        let refresher = TokenRefresher::new(300).unwrap();
        let mut account = account();
        account.credentials.region = "eu-west-1".into();
        account.profile_arn =
            Some("arn:aws:codewhisperer:us-east-1:123456789012:profile/test".into());
        let request = refresher
            .token_request(&account)
            .ok()
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            request.url().as_str(),
            "https://prod.us-east-1.auth.desktop.kiro.dev/refreshToken"
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(
                request.body().unwrap().as_bytes().unwrap()
            )
            .unwrap(),
            json!({"refreshToken":"refresh-token"})
        );
        assert!(!request.headers().contains_key("authorization"));
        assert!(!request.headers().contains_key("tokentype"));

        account.credentials.auth_method = AuthMethod::Idc;
        account.credentials.client_id = Some("client-id".into());
        account.credentials.client_secret = Some("client-secret".into());
        let request = refresher
            .token_request(&account)
            .ok()
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            request.url().as_str(),
            "https://oidc.eu-west-1.amazonaws.com/token"
        );
        assert_eq!(request.headers()["user-agent"], "KiroIDE");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(
                request.body().unwrap().as_bytes().unwrap()
            )
            .unwrap(),
            json!({
                "clientId":"client-id", "clientSecret":"client-secret",
                "grantType":"refresh_token", "refreshToken":"refresh-token"
            })
        );
    }

    #[tokio::test]
    async fn successful_refresh_preserves_cooling_and_exhausted_health() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                r#"{"accessToken":"new-access-token","refreshToken":"new-refresh-token","expiresIn":3600}"#,
                "application/json",
            ))
            .mount(&server)
            .await;

        let config = PoolConfig {
            cooldown: crosslane_core::config::CooldownConfig {
                quota_error_threshold: 1,
                ..crosslane_core::config::CooldownConfig::default()
            },
            ..PoolConfig::default()
        };
        let pool = AccountPool::new(vec![account()], config);
        let state = pool.get("acc_refresh").await.expect("account state");
        let refresher = TokenRefresher::new(300)
            .expect("refresher")
            .with_endpoint(format!("{}/refresh", server.uri()));

        pool.record_error("acc_refresh").await;
        assert_eq!(state.health(), AccountHealth::Cooling);
        assert!(refresher.refresh_expiring(&pool).await.is_empty());
        assert_eq!(state.health(), AccountHealth::Cooling);
        assert_eq!(
            state
                .consecutive_errors
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert!(state.cooling_until.lock().await.is_some());

        state.reset_health().await;
        pool.record_quota_error("acc_refresh").await;
        assert_eq!(state.health(), AccountHealth::Exhausted);
        assert!(refresher
            .refresh_account(&pool, "acc_refresh", true)
            .await
            .expect("exhausted refresh"));
        assert_eq!(state.health(), AccountHealth::Exhausted);
        assert!(state.account.read().await.credit_exhausted);
        assert_eq!(state.quota_errors.lock().await.len(), 1);

        state.reset_health().await;
        pool.mark_banned("acc_refresh").await;
        assert!(refresher
            .refresh_account(&pool, "acc_refresh", true)
            .await
            .expect("banned refresh"));
        assert_eq!(state.health(), AccountHealth::Available);
        assert_eq!(
            state.account.read().await.credentials.access_token,
            "new-access-token"
        );
    }

    #[tokio::test]
    async fn api_keys_never_enter_oauth_refresh_even_with_stale_refresh_fields() {
        let server = MockServer::start().await;
        let mut account = account();
        account.credentials.auth_method = AuthMethod::ApiKey;
        account.credentials.access_token = "ksk_test-key".into();
        account.credentials.expires_at = 0;
        let pool = AccountPool::new(vec![account], PoolConfig::default());
        let refresher = TokenRefresher::new(300)
            .unwrap()
            .with_endpoint(format!("{}/refresh", server.uri()));
        let state = pool.get("acc_refresh").await.unwrap();
        let before = state.health();
        assert!(!refresher
            .refresh_account(&pool, "acc_refresh", false)
            .await
            .unwrap());
        assert!(matches!(
            refresher.refresh_account(&pool, "acc_refresh", true).await,
            Err(RefreshError::NotRefreshable)
        ));
        assert_eq!(state.health(), before);
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn social_refresh_persists_rotated_credentials_and_uses_current_user_agent() {
        let server = MockServer::start().await;
        let expected_user_agent = format!("KiroIDE-1.2.56-{}", "a".repeat(64));
        Mock::given(method("POST"))
            .and(path("/refresh"))
            .and(header("user-agent", expected_user_agent.as_str()))
            .and(body_json(json!({"refreshToken": "refresh-token"})))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                r#"{"accessToken":"new-access-token","refreshToken":"new-refresh-token","expiresIn":3600,"profileArn":"arn:aws:codewhisperer:us-east-1:123456789012:profile/test"}"#,
                "application/json",
            ))
            .expect(1)
            .mount(&server)
            .await;

        let pool = AccountPool::new(vec![account()], PoolConfig::default());
        let persisted = Arc::new(AtomicUsize::new(0));
        let callback_count = Arc::clone(&persisted);
        let refresher = TokenRefresher::new(300)
            .expect("refresher")
            .with_endpoint(format!("{}/refresh", server.uri()));

        assert!(
            refresher
                .refresh_account_and_persist(&pool, "acc_refresh", true, move |refreshed| {
                    let callback_count = Arc::clone(&callback_count);
                    async move {
                        assert_eq!(refreshed.account_id, "acc_refresh");
                        assert_eq!(
                            refreshed.credentials.refresh_token.as_deref(),
                            Some("new-refresh-token")
                        );
                        assert_eq!(
                            refreshed.profile_arn.as_deref(),
                            Some("arn:aws:codewhisperer:us-east-1:123456789012:profile/test")
                        );
                        callback_count.fetch_add(1, Ordering::AcqRel);
                        Ok(())
                    }
                })
                .await
                .expect("refresh")
                .changed
        );

        assert_eq!(persisted.load(Ordering::Acquire), 1);
        let refreshed = pool
            .get("acc_refresh")
            .await
            .expect("account")
            .account
            .read()
            .await
            .clone();
        assert_eq!(
            refreshed.credentials.refresh_token.as_deref(),
            Some("new-refresh-token")
        );
        assert_eq!(
            refreshed.profile_arn.as_deref(),
            Some("arn:aws:codewhisperer:us-east-1:123456789012:profile/test")
        );
    }

    #[tokio::test]
    async fn concurrent_auth_failures_share_one_rotated_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/refresh"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(100))
                    .set_body_raw(
                        r#"{"accessToken":"new-access-token","refreshToken":"new-refresh-token","expiresIn":3600}"#,
                        "application/json",
                    ),
            )
            .expect(1)
            .mount(&server)
            .await;

        let pool = AccountPool::new(vec![account()], PoolConfig::default());
        let refresher = TokenRefresher::new(300)
            .expect("refresher")
            .with_endpoint(format!("{}/refresh", server.uri()));
        let first = refresher.refresh_after_auth_failure_and_persist(
            &pool,
            "acc_refresh",
            "old-access-token",
            || ready(Ok(None)),
            |_| async { Ok(()) },
        );
        let second = refresher.refresh_after_auth_failure_and_persist(
            &pool,
            "acc_refresh",
            "old-access-token",
            || ready(Ok(None)),
            |_| async { Ok(()) },
        );

        let (first, second) = tokio::join!(first, second);
        let changed = [first.expect("first"), second.expect("second")];
        assert_eq!(
            changed
                .into_iter()
                .filter(|outcome| outcome.changed)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn late_auth_failure_reuses_token_refreshed_by_an_earlier_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                r#"{"accessToken":"new-access-token","refreshToken":"new-refresh-token","expiresIn":3600}"#,
                "application/json",
            ))
            .expect(1)
            .mount(&server)
            .await;

        let pool = AccountPool::new(vec![account()], PoolConfig::default());
        let refresher = TokenRefresher::new(300)
            .expect("refresher")
            .with_endpoint(format!("{}/refresh", server.uri()));
        let first = refresher
            .refresh_after_auth_failure_and_persist(
                &pool,
                "acc_refresh",
                "old-access-token",
                || ready(Ok(None)),
                |_| async { Ok(()) },
            )
            .await
            .expect("first refresh");
        let late = refresher
            .refresh_after_auth_failure_and_persist(
                &pool,
                "acc_refresh",
                "old-access-token",
                || ready(Ok(None)),
                |_| async { Ok(()) },
            )
            .await
            .expect("late follower");

        assert!(first.changed);
        assert!(!late.changed);
    }

    #[tokio::test]
    async fn persistence_failure_keeps_refreshed_credentials_usable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                r#"{"accessToken":"new-access-token","refreshToken":"new-refresh-token","expiresIn":3600}"#,
                "application/json",
            ))
            .expect(1)
            .mount(&server)
            .await;

        let pool = AccountPool::new(vec![account()], PoolConfig::default());
        let state = pool.get("acc_refresh").await.expect("account");
        let refresher = TokenRefresher::new(300)
            .expect("refresher")
            .with_endpoint(format!("{}/refresh", server.uri()));
        let outcome = refresher
            .refresh_after_auth_failure_and_persist(
                &pool,
                "acc_refresh",
                "old-access-token",
                || ready(Ok(None)),
                |_| async { Err("disk full".into()) },
            )
            .await
            .expect("network refresh remains successful");

        assert!(outcome.changed);
        assert_eq!(outcome.persistence_error.as_deref(), Some("disk full"));
        assert_eq!(state.health(), AccountHealth::Available);
        let account = state.account.read().await;
        assert_eq!(account.credentials.access_token, "new-access-token");
        assert_eq!(
            account.credentials.refresh_token.as_deref(),
            Some("new-refresh-token")
        );
    }

    #[tokio::test]
    async fn idc_400_reloads_rotated_credentials_and_retries_once() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/refresh"))
            .and(body_json(json!({
                "clientId": "old-client",
                "clientSecret": "old-secret",
                "grantType": "refresh_token",
                "refreshToken": "old-refresh"
            })))
            .respond_with(ResponseTemplate::new(400).set_body_string("invalid_request"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/refresh"))
            .and(body_json(json!({
                "clientId": "new-client",
                "clientSecret": "new-secret",
                "grantType": "refresh_token",
                "refreshToken": "new-refresh"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                r#"{"accessToken":"recovered-access","refreshToken":"recovered-refresh","expiresIn":3600}"#,
                "application/json",
            ))
            .expect(1)
            .mount(&server)
            .await;

        let mut idc = account();
        idc.credentials.auth_method = AuthMethod::Idc;
        idc.credentials.refresh_token = Some("old-refresh".into());
        idc.credentials.client_id = Some("old-client".into());
        idc.credentials.client_secret = Some("old-secret".into());
        let mut reloaded = idc.credentials.clone();
        reloaded.refresh_token = Some("new-refresh".into());
        reloaded.client_id = Some("new-client".into());
        reloaded.client_secret = Some("new-secret".into());

        let pool = AccountPool::new(vec![idc], PoolConfig::default());
        let refresher = TokenRefresher::new(300)
            .expect("refresher")
            .with_endpoint(format!("{}/refresh", server.uri()));
        let outcome = refresher
            .refresh_after_auth_failure_and_persist(
                &pool,
                "acc_refresh",
                "old-access-token",
                move || {
                    ready(Ok(Some(ReloadedCredentials {
                        credentials: reloaded,
                        profile_arn: None,
                    })))
                },
                |_| async { Ok(()) },
            )
            .await
            .expect("recovered refresh");

        assert!(outcome.changed);
        let runtime = pool.get("acc_refresh").await.expect("account");
        let account = runtime.account.read().await;
        assert_eq!(account.credentials.access_token, "recovered-access");
        assert_eq!(
            account.credentials.refresh_token.as_deref(),
            Some("recovered-refresh")
        );
    }

    #[tokio::test]
    async fn proactive_refresh_failure_keeps_still_valid_account_available() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/refresh"))
            .respond_with(ResponseTemplate::new(500).set_body_string("temporary failure"))
            .mount(&server)
            .await;

        let mut expiring = account();
        expiring.credentials.expires_at = now_secs() + 60;
        let pool = AccountPool::new(vec![expiring], PoolConfig::default());
        let state = pool.get("acc_refresh").await.expect("account");
        let refresher = TokenRefresher::new(300)
            .expect("refresher")
            .with_endpoint(format!("{}/refresh", server.uri()));

        assert!(refresher
            .refresh_account(&pool, "acc_refresh", false)
            .await
            .is_err());
        assert_eq!(state.health(), AccountHealth::Available);
        assert_eq!(
            state.account.read().await.credentials.access_token,
            "old-access-token"
        );
    }
}
