//! Chromium automation used only by the full SSO build.

use anyhow::{Context, Result};
use chromiumoxide::{Browser, BrowserConfig, Page};
use futures::StreamExt;
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Owns Chromium and the two background drivers needed during login.
pub struct BrowserSession {
    browser: Browser,
    page: Page,
    handler: JoinHandle<()>,
    injector: Option<JoinHandle<()>>,
    kiro_failure: Option<oneshot::Receiver<LoginFailure>>,
    cancel: CancellationToken,
    profile_dir: Option<TempDir>,
}

impl BrowserSession {
    pub async fn launch_kiro(
        authorize_url: &str,
        email: &str,
        password: &str,
        headful: bool,
    ) -> Result<Self> {
        let mut session = Self::launch_page(authorize_url, headful).await?;
        let (failure, receiver) = oneshot::channel();
        session.kiro_failure = Some(receiver);
        session.injector = Some(spawn_kiro_injector(
            session.page.clone(),
            email,
            password,
            session.cancel.clone(),
            headful,
            failure,
        )?);
        Ok(session)
    }

    /// Launch an isolated browser for an authorization flow driven by its caller.
    pub async fn launch_page(authorize_url: &str, headful: bool) -> Result<Self> {
        // Chromiumoxide otherwise reuses /tmp/chromiumoxide-runner. A unique
        // profile prevents concurrent or sequential SSO logins from inheriting
        // another account's browser process, cookies, or local storage.
        let profile_dir = isolated_profile_dir()?;
        let mut builder = BrowserConfig::builder()
            .user_data_dir(profile_dir.path())
            .window_size(1100, 800)
            .launch_timeout(std::time::Duration::from_secs(20));
        if std::env::var("CROSSLANE_CHROMIUM_NO_SANDBOX").as_deref() == Ok("1") {
            builder = builder.no_sandbox();
        }
        if headful {
            builder = builder.with_head();
        }
        let config = builder
            .build()
            .map_err(|error| anyhow::anyhow!("invalid Chromium configuration: {error}"))?;
        let (mut browser, mut events) = Browser::launch(config)
            .await
            .context("unable to launch Chromium; install Chrome/Chromium or use a full image")?;
        let handler = tokio::spawn(async move {
            while let Some(event) = events.next().await {
                if let Err(error) = event {
                    tracing::warn!(%error, "Chromium event stream stopped");
                    return;
                }
            }
            tracing::debug!("Chromium event stream closed");
        });
        // Do not set BrowserConfig::incognito(): chromiumoxide treats that as
        // already isolated and makes start_incognito_context() a no-op. Creating
        // a CDP browser context here gives every login an actual ephemeral
        // cookie/cache partition.
        let page_result = match browser.start_incognito_context().await {
            Ok(browser) => browser.new_page(authorize_url).await,
            Err(error) => Err(error),
        };
        let page = match page_result {
            Ok(page) => page,
            Err(error) => {
                handler.abort();
                let _ = browser.kill().await;
                return Err(error).context("unable to create an incognito Chromium page");
            }
        };
        tracing::info!(headful, "isolated Chromium authorization browser launched");
        let cancel = CancellationToken::new();
        Ok(Self {
            browser,
            page,
            handler,
            injector: None,
            kiro_failure: None,
            cancel,
            profile_dir: Some(profile_dir),
        })
    }

    pub fn page(&self) -> &Page {
        &self.page
    }

    pub(super) fn take_kiro_failure(&mut self) -> Option<oneshot::Receiver<LoginFailure>> {
        self.kiro_failure.take()
    }

    pub async fn close(&mut self) {
        self.cancel.cancel();
        if let Some(injector) = self.injector.take() {
            injector.abort();
        }
        if let Err(error) = self.browser.quit_incognito_context().await {
            tracing::warn!(%error, "unable to dispose Chromium incognito context");
        }
        if let Err(error) = self.browser.close().await {
            tracing::warn!(%error, "unable to close Chromium cleanly; killing it");
            if let Some(Err(error)) = self.browser.kill().await {
                tracing::warn!(%error, "unable to kill Chromium after close failure");
            }
        } else {
            match tokio::time::timeout(std::time::Duration::from_secs(10), self.browser.wait())
                .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    tracing::warn!(%error, "unable to wait for Chromium; killing it");
                    if let Some(Err(error)) = self.browser.kill().await {
                        tracing::warn!(%error, "unable to kill Chromium after wait failure");
                    }
                }
                Err(_) => {
                    tracing::warn!("Chromium did not exit within 10 seconds; killing it");
                    if let Some(Err(error)) = self.browser.kill().await {
                        tracing::warn!(%error, "unable to kill Chromium after exit timeout");
                    }
                }
            }
        }
        self.handler.abort();
        if let Some(profile_dir) = self.profile_dir.take() {
            if let Err(error) = profile_dir.close() {
                tracing::warn!(%error, "unable to remove isolated Chromium profile");
            }
        }
    }
}

fn isolated_profile_dir() -> Result<TempDir> {
    tempfile::Builder::new()
        .prefix("crosslane-sso-")
        .tempdir()
        .context("unable to create an isolated Chromium profile")
}

impl Drop for BrowserSession {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(injector) = self.injector.take() {
            injector.abort();
        }
        self.handler.abort();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LoginFailure {
    PasswordUpdateRequired,
    InvalidCredentials,
    AccountLocked,
}

impl LoginFailure {
    fn from_action(action: &str) -> Option<Self> {
        match action {
            "blocked:password-update-required" => Some(Self::PasswordUpdateRequired),
            "blocked:invalid-credentials" => Some(Self::InvalidCredentials),
            "blocked:account-locked" => Some(Self::AccountLocked),
            _ => None,
        }
    }

    pub(super) fn message(self) -> &'static str {
        match self {
            Self::PasswordUpdateRequired => "Kiro SSO: the identity provider requires a password update before sign-in can continue; complete that organization sign-in step, then retry with the current login password",
            Self::InvalidCredentials => "Kiro SSO: the identity provider rejected the login credentials",
            Self::AccountLocked => "Kiro SSO: the identity provider locked the account or blocked further login attempts",
        }
    }
}

fn spawn_kiro_injector(
    page: Page,
    email: &str,
    password: &str,
    cancel: CancellationToken,
    headful: bool,
    failure: oneshot::Sender<LoginFailure>,
) -> Result<JoinHandle<()>> {
    let email = serde_json::to_string(email)?;
    let password = serde_json::to_string(password)?;
    let script = automation_script(&email, &password);
    Ok(tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(600));
        let mut last_action: Option<String> = None;
        let mut evaluate_error_logged = false;
        let mut navigation_error_logged = false;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    match page.evaluate(script.clone()).await {
                        Ok(result) => {
                            evaluate_error_logged = false;
                            navigation_error_logged = false;
                            if let Ok(action) = result.into_value::<String>() {
                                if let Some(reason) = LoginFailure::from_action(&action) {
                                    if !headful {
                                        tracing::info!(reason = %action, "Kiro SSO requires operator action");
                                        let _ = failure.send(reason);
                                        return;
                                    }
                                }
                                if last_action.as_deref() != Some(action.as_str()) {
                                    if let Some(stage) = action.strip_prefix("waiting:") {
                                        tracing::info!(%stage, "SSO browser automation waiting");
                                    } else {
                                        tracing::info!(%action, "SSO browser automation advanced");
                                    }
                                    last_action = Some(action);
                                }
                            }
                        }
                        Err(error) if is_navigation_context_error(&error.to_string()) => {
                            // A navigation can briefly invalidate the execution context. Log the
                            // first failure and keep polling so the next page can continue login.
                            if !navigation_error_logged {
                                tracing::debug!(%error, "SSO browser navigation replaced the execution context; retrying");
                                navigation_error_logged = true;
                            }
                        }
                        Err(error) if !evaluate_error_logged => {
                            tracing::warn!(%error, "SSO browser automation step failed");
                            evaluate_error_logged = true;
                        }
                        Err(_) => {}
                    }
                }
            }
        }
    }))
}

fn is_navigation_context_error(error: &str) -> bool {
    error.contains("Cannot find context with specified id")
        || error.contains("Execution context was destroyed")
}

fn automation_script(email: &str, password: &str) -> String {
    let driver = include_str!("kiro_login.js");
    format!("({driver})({email},{password})")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture_page(html: &'static str) -> (String, JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(move || async move { axum::response::Html(html) }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}/"), server)
    }

    #[tokio::test]
    #[ignore = "requires a local Chromium executable"]
    async fn kiro_password_update_page_fails_without_filling_or_submitting_fields() {
        let (url, server) = fixture_page(
            r#"<form onsubmit="event.preventDefault();window.submitted=true">
            <input type="password" id="currentPassword" name="currentpasswd">
            <input type="password" id="newPassword" name="newpasswd">
            <input type="password" id="confirmNewPassword" name="confirmnewpasswd">
            <input type="submit" id="idSIButton9"></form>"#,
        )
        .await;
        let mut browser =
            BrowserSession::launch_kiro(&url, "test@example.invalid", "fixture-secret", false)
                .await
                .unwrap();
        let reason = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            browser.take_kiro_failure().unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(reason, LoginFailure::PasswordUpdateRequired);
        let untouched = browser
            .page()
            .evaluate("!window.submitted && [...document.querySelectorAll('input[type=password]')].every(x => !x.value)")
            .await
            .unwrap()
            .into_value::<bool>()
            .unwrap();
        browser.close().await;
        server.abort();
        assert!(untouched);
    }

    #[tokio::test]
    #[ignore = "requires a local Chromium executable"]
    async fn kiro_normal_password_page_submits_the_same_single_login_password() {
        let (url, server) = fixture_page(
            r#"<form onsubmit="event.preventDefault();window.submitted=true">
            <input type="password" id="i0118" name="passwd">
            <input type="submit" id="idSIButton9"></form>"#,
        )
        .await;
        let password = "  fixture,\"中文🙂  ";
        let mut browser =
            BrowserSession::launch_kiro(&url, "test@example.invalid", password, false)
                .await
                .unwrap();
        let received = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let result = browser
                    .page()
                    .evaluate("window.submitted ? document.querySelector('#i0118').value : null")
                    .await
                    .unwrap();
                if let Ok(value) = result.into_value::<String>() {
                    break value;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await;
        browser.close().await;
        server.abort();
        assert_eq!(received.unwrap(), password);
    }

    #[tokio::test]
    #[ignore = "requires a local Chromium executable"]
    async fn kiro_invalid_credentials_page_returns_a_specific_failure() {
        let (url, server) = fixture_page(
            r#"<div id="passwordError" role="alert">Your account or password is incorrect.</div>
            <input type="password" name="passwd" value="already-submitted">"#,
        )
        .await;
        let mut browser =
            BrowserSession::launch_kiro(&url, "test@example.invalid", "fixture-secret", false)
                .await
                .unwrap();
        let reason = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            browser.take_kiro_failure().unwrap(),
        )
        .await;
        browser.close().await;
        server.abort();
        assert_eq!(reason.unwrap().unwrap(), LoginFailure::InvalidCredentials);
    }

    #[test]
    fn navigation_errors_are_distinct_from_real_browser_failures() {
        assert!(is_navigation_context_error(
            "Error -32000: Cannot find context with specified id"
        ));
        assert!(is_navigation_context_error(
            "Execution context was destroyed, most likely because of a navigation"
        ));
        for error in [
            "Browser connection closed",
            "Evaluation timed out",
            "Error -32000: Permission denied",
        ] {
            assert!(!is_navigation_context_error(error));
        }
    }

    #[test]
    fn chromium_profiles_are_unique_and_removed_after_close() {
        let first = isolated_profile_dir().expect("first profile");
        let second = isolated_profile_dir().expect("second profile");
        assert_ne!(first.path(), second.path());

        let first_path = first.path().to_path_buf();
        first.close().expect("remove first profile");
        assert!(!first_path.exists());
    }
}
