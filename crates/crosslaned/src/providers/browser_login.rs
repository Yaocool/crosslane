//! Daemon-owned, temporary Copilot authorization browsers.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use crosslane_copilot::{CopilotProvider, DeviceLoginState, DeviceLoginStatus};
use crosslane_ipc::protocol::{CopilotBrowserCredentials, RpcError};
use serde_json::{json, Value};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

#[cfg(feature = "sso")]
mod driver;

pub struct BrowserLoginManager {
    tasks: Mutex<BTreeMap<(String, String), Arc<LoginTask>>>,
    slots: Arc<Semaphore>,
}

struct LoginTask {
    runtime: Weak<CopilotProvider>,
    state: Mutex<DeviceLoginState>,
    progress: Mutex<Value>,
    code: Mutex<Option<String>>,
    cancel: CancellationToken,
    expires_at: i64,
}

impl Default for BrowserLoginManager {
    fn default() -> Self {
        Self {
            tasks: Mutex::new(BTreeMap::new()),
            slots: Arc::new(Semaphore::new(2)),
        }
    }
}

impl LoginTask {
    fn snapshot(&self) -> Value {
        let mut value = serde_json::to_value(&*self.state.lock().unwrap()).unwrap();
        value["browser"] = self.progress.lock().unwrap().clone();
        value
    }

    #[cfg(feature = "sso")]
    fn progress(&self, stage: &str, message: &str) {
        *self.progress.lock().unwrap() = json!({
            "mode": "headless", "stage": stage, "message": message,
        });
    }

    fn fail(&self, message: String) {
        let mut state = self.state.lock().unwrap();
        if state.status == DeviceLoginStatus::Pending {
            state.status = DeviceLoginStatus::Failed;
            state.error = Some(message);
        }
    }
}

impl BrowserLoginManager {
    pub fn validate(credentials: &CopilotBrowserCredentials) -> Result<(), RpcError> {
        if !cfg!(feature = "sso") {
            return Err(RpcError::bad_params(
                "remote Copilot login requires the full crosslaned build with Chromium (--features sso)",
            ));
        }
        if credentials.github_username.trim().is_empty()
            || credentials.github_username.chars().any(char::is_whitespace)
        {
            return Err(RpcError::bad_params(
                "github_username must be a GitHub username or email address without whitespace",
            ));
        }
        if credentials.github_username.contains('@')
            && (credentials.sso_username.is_some()
                || credentials.sso_password.is_some()
                || credentials.sso_start_url.is_some())
        {
            return Err(RpcError::bad_params(
                "SSO login requires the complete GitHub username, not the Azure email address",
            ));
        }
        if credentials
            .github_password
            .as_deref()
            .is_none_or(str::is_empty)
            && credentials
                .sso_password
                .as_deref()
                .is_none_or(str::is_empty)
        {
            return Err(RpcError::bad_params(
                "github_password or sso_password is required for remote browser login",
            ));
        }
        if credentials.sso_password.is_some()
            != credentials
                .sso_username
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
        {
            return Err(RpcError::bad_params(
                "sso_username and sso_password must be supplied together",
            ));
        }
        Ok(())
    }

    pub fn start(
        &self,
        runtime: Arc<CopilotProvider>,
        state: DeviceLoginState,
        credentials: CopilotBrowserCredentials,
        trace_path: Option<PathBuf>,
        shutdown: &CancellationToken,
    ) -> Result<Value, RpcError> {
        Self::validate(&credentials)?;
        validate_urls(
            &state.verification_uri,
            runtime.oauth_base(),
            credentials.sso_start_url.as_deref(),
        )?;
        let slot = Arc::clone(&self.slots)
            .try_acquire_owned()
            .map_err(|_| RpcError {
                code: 429,
                message:
                    "two remote authorization browsers are already running; wait for one to finish"
                        .into(),
            })?;
        let task = Arc::new(LoginTask {
            runtime: Arc::downgrade(&runtime),
            expires_at: state.expires_at,
            state: Mutex::new(state.clone()),
            progress: Mutex::new(
                json!({"mode":"headless","stage":"starting","message":"正在启动远端独立无痕浏览器"}),
            ),
            code: Mutex::new(None),
            cancel: shutdown.child_token(),
        });
        {
            let mut tasks = self.tasks.lock().unwrap();
            tasks.retain(|_, task| {
                if task.expires_at.saturating_add(60) < now_secs() {
                    task.cancel.cancel();
                    false
                } else {
                    true
                }
            });
            tasks.insert(
                (state.provider_id.clone(), state.id.clone()),
                Arc::clone(&task),
            );
        }
        let initial = task.snapshot();
        tokio::spawn(async move {
            let _slot = slot;
            #[cfg(feature = "sso")]
            let result = driver::run(&runtime, &task, &state, credentials, trace_path).await;
            #[cfg(not(feature = "sso"))]
            let result: anyhow::Result<()> = {
                let _ = (credentials, trace_path);
                Err(anyhow::anyhow!("Chromium support is not compiled in"))
            };
            if let Err(error) = result {
                // The driver reports fixed stage errors, never Chromium JS/credential contents.
                task.fail(error.to_string());
                let _ = runtime.cancel_device_login(&state.id).await;
            }
            if task.cancel.is_cancelled() {
                let cancelled = runtime.cancel_device_login(&state.id).await;
                if let Ok(cancelled) = cancelled {
                    let mut current = task.state.lock().unwrap();
                    if current.status == DeviceLoginStatus::Pending {
                        *current = cancelled;
                    }
                }
            }
            task.code.lock().unwrap().take();
        });
        Ok(initial)
    }

    pub fn status(&self, provider: &str, id: &str) -> Option<Value> {
        self.tasks
            .lock()
            .unwrap()
            .get(&(provider.into(), id.into()))
            .map(|task| task.snapshot())
    }

    pub fn cancel(&self, provider: &str, id: &str) -> Option<Value> {
        let tasks = self.tasks.lock().unwrap();
        let task = tasks.get(&(provider.into(), id.into()))?;
        task.cancel.cancel();
        let mut state = task.state.lock().unwrap();
        if state.status == DeviceLoginStatus::Pending {
            state.status = DeviceLoginStatus::Cancelled;
        }
        drop(state);
        Some(task.snapshot())
    }

    pub fn submit_code(&self, provider: &str, id: &str, code: String) -> Result<(), RpcError> {
        let tasks = self.tasks.lock().unwrap();
        let task = tasks
            .get(&(provider.into(), id.into()))
            .ok_or_else(|| RpcError::bad_params("remote browser login task not found"))?;
        if task.state.lock().unwrap().status != DeviceLoginStatus::Pending
            || task.progress.lock().unwrap()["stage"] != "mfa_code"
        {
            return Err(RpcError::bad_params(
                "this login task is not waiting for an MFA code",
            ));
        }
        let code = code.trim().replace([' ', '-'], "");
        if !(4..=12).contains(&code.len()) || !code.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(RpcError::bad_params("MFA code must contain 4 to 12 digits"));
        }
        *task.code.lock().unwrap() = Some(code);
        Ok(())
    }

    pub fn reconcile(&self, providers: &[Arc<CopilotProvider>]) {
        for task in self.tasks.lock().unwrap().values() {
            if !providers
                .iter()
                .any(|provider| task.runtime.ptr_eq(&Arc::downgrade(provider)))
            {
                task.cancel.cancel();
                task.fail(
                    "provider configuration changed during browser login; restart authorization"
                        .into(),
                );
            }
        }
    }
}

impl Drop for BrowserLoginManager {
    fn drop(&mut self) {
        for task in self.tasks.get_mut().unwrap().values() {
            task.cancel.cancel();
        }
    }
}

fn validate_urls(
    verification_uri: &str,
    oauth_base: &str,
    sso_start_url: Option<&str>,
) -> Result<(), RpcError> {
    let verification = url::Url::parse(verification_uri)
        .map_err(|_| RpcError::bad_params("invalid GitHub verification URL"))?;
    if verification.scheme() != "https"
        || verification.host_str().is_none()
        || !verification.username().is_empty()
        || verification.password().is_some()
    {
        return Err(RpcError::bad_params(
            "GitHub verification URL must use HTTPS without URL credentials",
        ));
    }
    let oauth =
        url::Url::parse(oauth_base).map_err(|_| RpcError::bad_params("invalid OAuth base URL"))?;
    if verification.origin() != oauth.origin() {
        return Err(RpcError::bad_params(
            "GitHub verification URL does not match the configured OAuth origin",
        ));
    }
    if let Some(start) = sso_start_url {
        let start =
            url::Url::parse(start).map_err(|_| RpcError::bad_params("invalid sso_start_url"))?;
        if start.origin() != verification.origin()
            || !start.username().is_empty()
            || start.password().is_some()
            || !(start.path().starts_with("/orgs/") || start.path().starts_with("/enterprises/"))
            || !start.path().ends_with("/sso")
        {
            return Err(RpcError::bad_params("sso_start_url must be an /orgs/<name>/sso or /enterprises/<name>/sso URL on the GitHub verification origin"));
        }
    }
    Ok(())
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "sso")]
    #[test]
    fn browser_credentials_require_the_github_identity_and_an_origin_specific_password() {
        let mut credentials = CopilotBrowserCredentials {
            github_username: "developer_company".into(),
            github_password: None,
            sso_username: Some("developer@example.com".into()),
            sso_password: Some("azure-only-password".into()),
            sso_start_url: None,
        };
        assert!(BrowserLoginManager::validate(&credentials).is_ok());
        credentials.github_username = "developer@example.com".into();
        assert!(BrowserLoginManager::validate(&credentials).is_err());
        credentials.github_username = "developer_company".into();
        credentials.sso_username = None;
        assert!(BrowserLoginManager::validate(&credentials).is_err());
        credentials.sso_password = None;
        assert!(BrowserLoginManager::validate(&credentials).is_err());
        credentials.github_password = Some("github-only-password".into());
        assert!(BrowserLoginManager::validate(&credentials).is_ok());
        credentials.github_username = "developer@example.com".into();
        assert!(BrowserLoginManager::validate(&credentials).is_ok());
        credentials.sso_start_url = Some("https://github.com/enterprises/example/sso".into());
        assert!(BrowserLoginManager::validate(&credentials).is_err());
    }

    #[test]
    fn remote_browser_verification_and_sso_urls_remain_on_the_configured_origin() {
        assert!(validate_urls(
            "https://github.com/login/device",
            "https://github.com",
            Some("https://github.com/enterprises/example/sso")
        )
        .is_ok());
        for (verification, sso) in [
            ("https://github.com.evil.test/login/device", None),
            ("http://github.com/login/device", None),
            ("https://user:pass@github.com/login/device", None),
            (
                "https://github.com/login/device",
                Some("https://evil.test/enterprises/example/sso"),
            ),
            (
                "https://github.com/login/device",
                Some("https://github.com/settings"),
            ),
        ] {
            assert!(validate_urls(verification, "https://github.com", sso).is_err());
        }
    }

    #[test]
    fn one_time_codes_and_cancellation_are_scoped_to_the_login_task() {
        let manager = BrowserLoginManager::default();
        let task = Arc::new(LoginTask {
            runtime: Weak::new(),
            expires_at: now_secs() + 60,
            state: Mutex::new(DeviceLoginState {
                id: "login_test".into(),
                provider_id: "github-copilot".into(),
                user_code: "ABCD-EFGH".into(),
                verification_uri: "https://github.com/login/device".into(),
                expires_at: now_secs() + 60,
                interval_secs: 5,
                status: DeviceLoginStatus::Pending,
                account_id: None,
                error: None,
            }),
            progress: Mutex::new(json!({"mode":"headless","stage":"mfa_code"})),
            code: Mutex::new(None),
            cancel: CancellationToken::new(),
        });
        manager.tasks.lock().unwrap().insert(
            ("github-copilot".into(), "login_test".into()),
            Arc::clone(&task),
        );
        assert!(manager
            .submit_code("other", "login_test", "123456".into())
            .is_err());
        assert!(manager
            .submit_code("github-copilot", "login_test", "bad".into())
            .is_err());
        manager
            .submit_code("github-copilot", "login_test", "123 456".into())
            .unwrap();
        assert_eq!(task.code.lock().unwrap().as_deref(), Some("123456"));
        assert!(!manager
            .status("github-copilot", "login_test")
            .unwrap()
            .to_string()
            .contains("123456"));
        assert!(manager.cancel("other", "login_test").is_none());
        assert_eq!(
            manager.cancel("github-copilot", "login_test").unwrap()["status"],
            "cancelled"
        );
        assert!(task.cancel.is_cancelled());
        assert!(manager
            .submit_code("github-copilot", "login_test", "123456".into())
            .is_err());
    }
}
