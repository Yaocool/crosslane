//! Chromium automation used only by the full SSO build.

use anyhow::{Context, Result};
use chromiumoxide::{Browser, BrowserConfig, Page};
use futures::StreamExt;
use tempfile::TempDir;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Owns Chromium and the two background drivers needed during login.
pub struct BrowserSession {
    browser: Browser,
    page: Page,
    handler: JoinHandle<()>,
    injector: Option<JoinHandle<()>>,
    cancel: CancellationToken,
    profile_dir: Option<TempDir>,
}

impl BrowserSession {
    pub async fn launch(
        authorize_url: &str,
        email: &str,
        password: &str,
        headful: bool,
    ) -> Result<Self> {
        let mut session = Self::launch_page(authorize_url, headful).await?;
        session.injector = Some(spawn_injector(
            session.page.clone(),
            email,
            password,
            session.cancel.clone(),
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
            cancel,
            profile_dir: Some(profile_dir),
        })
    }

    pub fn page(&self) -> &Page {
        &self.page
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

fn spawn_injector(
    page: Page,
    email: &str,
    password: &str,
    cancel: CancellationToken,
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
    format!(
        r#"(() => {{
          const email = {email};
          const password = {password};
          const visible = el => !!el && !el.disabled && el.offsetParent !== null;
          const setValue = (el, value) => {{
            if (!visible(el) || el.value) return false;
            const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
            setter.call(el, value);
            el.dispatchEvent(new Event('input', {{ bubbles: true }}));
            el.dispatchEvent(new Event('change', {{ bubbles: true }}));
            return true;
          }};
          const first = selectors => selectors.map(s => document.querySelector(s)).find(visible);
          const user = first(['input[name="loginfmt"]','input[name="username"]','input[type="email"]','input[name="email"]','#i0116','#username','#email','input[autocomplete="username"]','input[autocomplete="email"]']);
          if (setValue(user, email)) {{
            const next = first(['#idSIButton9','button[type="submit"]','input[type="submit"]']);
            if (next) next.click();
            return 'username';
          }}
          const pass = first(['input[name="passwd"]','input[type="password"]','input[name="password"]','#i0118','#passwordInput','#password']);
          if (setValue(pass, password)) {{
            const submit = first(['#idSIButton9','button[type="submit"]','input[type="submit"]']);
            if (submit) submit.click();
            return 'password';
          }}
          const no = first(['#idBtn_Back']);
          if (no && /stay signed in/i.test(document.body.innerText)) {{ no.click(); return 'kmsi'; }}
          const allow = first(['button[name="allow"]','[data-testid="allow-button"]','[data-testid="allow-access-button"]','button[value="allow"]','#allow-button','button.awsui-button--primary','#cli_login_button','button[data-analytics="allowButton"]']);
          if (allow) {{ allow.click(); return 'allow'; }}
          const textAllow = [...document.querySelectorAll('button,input[type="submit"]')]
            .find(el => visible(el) && /^(allow|authorize|allow access|continue|confirm|允许|确认授权)/i.test((el.innerText || el.value || '').trim()));
          if (textAllow) {{ textAllow.click(); return 'allow-text'; }}
          const visibleInputs = [...document.querySelectorAll('input')].filter(visible);
          const hasOtp = visibleInputs.some(el =>
            /otp|code|verification|one.?time/i.test(`${{el.name}} ${{el.id}} ${{el.autocomplete}}`));
          const hasAlert = [...document.querySelectorAll('[role="alert"],.error,.error-message')]
            .some(visible);
          if (hasOtp) return 'waiting:mfa';
          if (hasAlert) return 'waiting:error';
          if (visibleInputs.length) return 'waiting:unrecognized-input';
          return 'waiting:page';
        }})()"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
