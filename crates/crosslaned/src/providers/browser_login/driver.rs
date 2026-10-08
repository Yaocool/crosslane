use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chromiumoxide::Page;
use crosslane_copilot::{CopilotProvider, DeviceLoginState, DeviceLoginStatus};
use crosslane_ipc::protocol::CopilotBrowserCredentials;
use serde::Deserialize;
use serde_json::json;

use super::{now_secs, LoginTask};
use crate::sso::browser::BrowserSession;

mod trace;

#[derive(Deserialize)]
struct BrowserStep {
    stage: String,
    #[serde(default)]
    number: Option<String>,
}

pub(super) async fn run(
    runtime: &Arc<CopilotProvider>,
    task: &Arc<LoginTask>,
    state: &DeviceLoginState,
    credentials: CopilotBrowserCredentials,
    trace_path: Option<std::path::PathBuf>,
) -> Result<()> {
    let mut session = BrowserSession::launch_page("about:blank", false)
        .await
        .context("unable to launch remote Chromium")?;
    let mut capture = None;
    let outcome =
        async {
            if task.cancel.is_cancelled() {
                return Ok(());
            }
            if let Some(path) = trace_path.as_deref() {
                capture = Some(trace::HeaderTrace::start(session.page(), path).await?);
            }
            let start = credentials
                .sso_start_url
                .as_deref()
                .unwrap_or(&state.verification_uri);
            session.page().goto(start).await.map_err(|_| {
                anyhow!("remote browser could not open the GitHub authorization page")
            })?;
            drive(
                session.page(),
                runtime,
                task,
                state,
                credentials,
                trace_path.as_deref(),
            )
            .await
        }
        .await;
    if let Some(capture) = capture {
        if capture.close().await.is_err() {
            task.progress(
                "capture_failed",
                "远端 header 记录写入失败，请检查服务器磁盘状态",
            );
        }
    }
    session.close().await;
    outcome
}

async fn drive(
    page: &Page,
    runtime: &Arc<CopilotProvider>,
    task: &Arc<LoginTask>,
    initial: &DeviceLoginState,
    credentials: CopilotBrowserCredentials,
    trace_path: Option<&Path>,
) -> Result<()> {
    let github_origin = url::Url::parse(&initial.verification_uri)?
        .origin()
        .ascii_serialization();
    let mut next_poll = tokio::time::Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(700));
    loop {
        tokio::select! {
            biased;
            _ = task.cancel.cancelled() => return Ok(()),
            _ = tick.tick() => {}
        }
        if now_secs() >= initial.expires_at || tokio::time::Instant::now() >= next_poll {
            let state = runtime.poll_device_login(&initial.id).await
                .map_err(|_| anyhow!("GitHub Device Flow polling failed; restart authorization and check server connectivity"))?;
            let status = state.status;
            next_poll =
                tokio::time::Instant::now() + Duration::from_secs(state.interval_secs.max(1));
            {
                let mut current = task.state.lock().unwrap();
                if current.status == DeviceLoginStatus::Pending {
                    *current = state;
                }
            }
            if status != DeviceLoginStatus::Pending {
                task.progress("finished", "远端 Device Flow 已结束，浏览器正在关闭");
                return Ok(());
            }
        }
        let code = task.code.lock().unwrap().take();
        let arguments = json!({
            "githubOrigin": github_origin,
            "verificationUri": initial.verification_uri,
            "userCode": initial.user_code,
            "credentials": credentials,
            "otp": code,
        });
        let script = format!("({})({arguments})", include_str!("automation.js"));
        let result = page.evaluate(script).await;
        let step: BrowserStep = match result {
            Ok(result) => result
                .into_value()
                .map_err(|_| anyhow!("remote browser returned an invalid login stage"))?,
            Err(error) => {
                let message = error.to_string();
                if message.contains("Cannot find context")
                    || message.contains("Execution context was destroyed")
                {
                    // Navigation may consume the submitted code; do not resubmit it blindly.
                    continue;
                }
                return Err(anyhow!(
                    "remote browser stopped responding during authorization"
                ));
            }
        };
        if let Some(error) = stage_error(&step.stage) {
            return Err(anyhow!("{error}"));
        }
        let mut message = stage_message(&step.stage).to_owned();
        if step.stage == "mfa_push" {
            if let Some(number) = step
                .number
                .filter(|number| number.len() <= 3 && number.bytes().all(|c| c.is_ascii_digit()))
            {
                message.push_str(&format!("；Authenticator 显示号码：{number}"));
            }
        }
        if let Some(path) = trace_path {
            message.push_str(&format!("（敏感 header 记录：{}）", path.display()));
        }
        task.progress(&step.stage, &message);
    }
}

fn stage_error(stage: &str) -> Option<&'static str> {
    match stage {
        "untrusted_origin" => Some("登录跳转到了尚未支持的身份提供商；未向该页面填写凭证"),
        "github_password_required" => Some("该账号要求 GitHub 密码；Azure 密码不会发送给 GitHub，请用 --credentials-stdin 分别提供两套凭证"),
        "sso_credentials_required" => Some("需要 Azure SSO 凭证；请提供 --sso-username 或在凭证 JSON 中设置 sso_username/sso_password"),
        "login_rejected" => Some("GitHub/Azure 拒绝了登录或授权，请检查账号、密码、组织授权策略及登录记录"),
        "account_mismatch" => Some("GitHub 确认页的账号与指定用户名不一致，已停止授权；请核对 GitHub 与 Azure 凭证对应关系"),
        "account_confirmation_required" => Some("无法校验 GitHub 账号确认页面，已停止自动授权；请检查页面变化"),
        "conditional_access_denied" => Some("Azure 条件访问拒绝了本次登录；请联系 Entra 管理员检查登录日志中的失败策略（包括安全信息注册策略）。复制 header 不能满足设备、应用或位置限制，不会自动重试"),
        "security_info_required" => Some("Azure 要求注册或更新 MFA/安全信息；请先按组织要求完成注册，再重新运行登录。远端浏览器不会自动修改安全信息"),
        "password_change" => Some("身份提供商要求修改密码，请先完成密码更新后重新运行登录"),
        "captcha" => Some("登录要求 CAPTCHA，当前远端自动登录无法继续"),
        "security_key" => Some("登录要求 Passkey、安全密钥或受管设备，此验证不能通过复制浏览器 header 完成"),
        _ => None,
    }
}

fn stage_message(stage: &str) -> &'static str {
    match stage {
        "github_username" => "已填写 GitHub 用户名，等待登录方式识别",
        "github_password" => "正在使用 GitHub 密码登录",
        "sso_redirect" => "正在跳转到组织身份提供商",
        "sso_username" => "已填写 Azure SSO 用户名",
        "sso_password" => "正在使用 Azure SSO 密码登录",
        "stay_signed_in" => "已选择不保留 Azure 浏览器登录会话",
        "device_code" => "远端浏览器已提交 Device Flow 验证码",
        "select_account" => "已核对并确认当前 GitHub 登录账号",
        "authorize" => "远端浏览器正在确认 OAuth 应用授权",
        "mfa_code" => "远端浏览器正在等待 MFA 动态验证码",
        "mfa_submitted" => "MFA 验证码已提交",
        "mfa_push" => "请在手机 Authenticator 中批准当前登录；远端浏览器继续等待",
        "complete" => "网页授权已完成，等待 GitHub token 与 Copilot 模型验证",
        "return_to_device" => "SSO 登录完成，正在返回 Device Flow 验证页面",
        _ => "远端浏览器正在等待授权页面",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    #[test]
    fn policy_and_enrollment_blocks_have_actionable_errors() {
        assert!(stage_error("conditional_access_denied")
            .unwrap()
            .contains("Entra 管理员"));
        assert!(stage_error("security_info_required")
            .unwrap()
            .contains("不会自动修改安全信息"));
    }

    async fn step(
        page: &Page,
        options: serde_json::Value,
        simulated_origin: Option<&str>,
    ) -> String {
        let script = match simulated_origin {
            Some(origin) => format!("(() => {{ const location = {{ origin: {}, pathname: '/tenant/login' }}; return ({})({options}); }})()", serde_json::to_string(origin).unwrap(), include_str!("automation.js")),
            None => format!("({})({options})", include_str!("automation.js")),
        };
        for _ in 0..100 {
            let stage = page
                .evaluate(script.clone())
                .await
                .unwrap()
                .into_value::<BrowserStep>()
                .unwrap()
                .stage;
            if stage != "waiting" {
                return stage;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let diagnostic = page
            .evaluate("({url:location.href,html:document.body.innerHTML})")
            .await
            .unwrap()
            .into_value::<serde_json::Value>()
            .unwrap();
        panic!("fixture did not reach an actionable browser stage: {diagnostic}");
    }

    #[tokio::test]
    #[ignore = "requires an installed Chrome/Chromium browser"]
    async fn remote_browser_ui_and_sensitive_header_capture() {
        let server = MockServer::start().await;
        let form = r#"<!doctype html><form onsubmit="event.preventDefault();document.body.dataset.submitted='yes'">
            <input id="login_field" name="login"><input id="password" name="password" type="password">
            <button type="submit">Sign in</button></form>"#;
        Mock::given(method("GET"))
            .and(path("/login"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/html")
                    .insert_header(
                        "set-cookie",
                        "trace_cookie=fixture-cookie; HttpOnly; Path=/",
                    )
                    .set_body_raw(form, "text/html"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET")).and(path("/login/device"))
            .respond_with(ResponseTemplate::new(200).insert_header("content-type","text/html")
                .set_body_raw(r#"<form onsubmit="event.preventDefault();document.body.dataset.submitted='yes'"><input name="user_code"><button>Continue</button></form>"#, "text/html"))
            .mount(&server).await;
        Mock::given(method("GET"))
            .and(path("/enterprises/example/sso"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                r#"<form method="post" action="/enterprises/example/oidc/initiate" onsubmit="event.preventDefault();document.body.dataset.submitted='sso'"><button type="submit">Continue</button></form>"#,
                "text/html",
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/login/device/select_account"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                r#"<form method="post" action="/login/device/select_account" onsubmit="event.preventDefault();document.body.dataset.submitted='account'"><input type="submit" value="Continue" aria-label="Continue as fixture-user"></form>"#,
                "text/html",
            ))
            .mount(&server)
            .await;
        let directory = tempfile::tempdir().unwrap();
        let trace_path = directory.path().join("headers.jsonl");
        let mut browser = BrowserSession::launch_page("about:blank", false)
            .await
            .unwrap();
        let page = browser.page();
        let capture = trace::HeaderTrace::start(page, &trace_path).await.unwrap();
        page.goto(format!("{}/login", server.uri())).await.unwrap();
        let mut options = json!({"githubOrigin":server.uri(),"verificationUri":format!("{}/login/device",server.uri()),"userCode":"ABCD-EFGH","credentials":{"github_username":"fixture-user","github_password":"fixture-password"},"otp":null});
        assert_eq!(step(page, options.clone(), None).await, "github_username");
        assert_eq!(step(page, options.clone(), None).await, "github_password");
        assert_eq!(
            page.evaluate("document.body.dataset.submitted")
                .await
                .unwrap()
                .into_value::<String>()
                .unwrap(),
            "yes"
        );
        page.goto(format!("{}/login/device", server.uri()))
            .await
            .unwrap();
        assert_eq!(step(page, options.clone(), None).await, "device_code");
        assert_eq!(
            page.evaluate("document.querySelector('input').value")
                .await
                .unwrap()
                .into_value::<String>()
                .unwrap(),
            "ABCD-EFGH"
        );

        page.goto(format!("{}/enterprises/example/sso", server.uri()))
            .await
            .unwrap();
        assert_eq!(step(page, options.clone(), None).await, "sso_redirect");
        assert_eq!(
            page.evaluate("document.body.dataset.submitted")
                .await
                .unwrap()
                .into_value::<String>()
                .unwrap(),
            "sso"
        );

        page.goto(format!("{}/login/device/select_account", server.uri()))
            .await
            .unwrap();
        assert_eq!(step(page, options.clone(), None).await, "select_account");
        assert_eq!(
            page.evaluate("document.body.dataset.submitted")
                .await
                .unwrap()
                .into_value::<String>()
                .unwrap(),
            "account"
        );
        page.goto(format!("{}/login/device/select_account", server.uri()))
            .await
            .unwrap();
        let mut wrong_account = options.clone();
        wrong_account["credentials"]["github_username"] = json!("another-user");
        assert_eq!(step(page, wrong_account, None).await, "account_mismatch");
        assert_eq!(
            page.evaluate("document.body.dataset.submitted || ''")
                .await
                .unwrap()
                .into_value::<String>()
                .unwrap(),
            ""
        );

        page.set_content(r#"<input name="loginfmt"><button id="idSIButton9" onclick="document.body.dataset.submitted='user'">Next</button>"#).await.unwrap();
        options["credentials"] = json!({"github_username":"emu_example","sso_username":"fixture@example.com","sso_password":"fixture-sso-password"});
        assert_eq!(
            step(
                page,
                options.clone(),
                Some("https://login.microsoftonline.com")
            )
            .await,
            "sso_username"
        );
        page.set_content(r#"<input name="passwd" type="password"><button id="idSIButton9" onclick="document.body.dataset.submitted='password'">Sign in</button>"#).await.unwrap();
        assert_eq!(
            step(
                page,
                options.clone(),
                Some("https://login.microsoftonline.com")
            )
            .await,
            "sso_password"
        );
        assert_eq!(
            page.evaluate("document.querySelector('input').value")
                .await
                .unwrap()
                .into_value::<String>()
                .unwrap(),
            "fixture-sso-password"
        );
        page.set_content(r#"<input name="passwd" type="password">"#)
            .await
            .unwrap();
        assert_eq!(
            step(
                page,
                options.clone(),
                Some("https://login.microsoftonline.com.evil.test")
            )
            .await,
            "untrusted_origin"
        );
        assert_eq!(
            page.evaluate("document.querySelector('input').value")
                .await
                .unwrap()
                .into_value::<String>()
                .unwrap(),
            ""
        );
        page.set_content(r#"<input id="currentPassword" name="currentpasswd" type="password"><input id="newPassword" name="newpasswd" type="password" autocomplete="off"><input id="confirmNewPassword" name="confirmnewpasswd" type="password">"#)
            .await
            .unwrap();
        assert_eq!(
            step(
                page,
                options.clone(),
                Some("https://login.microsoftonline.com")
            )
            .await,
            "password_change"
        );
        assert!(page
            .evaluate("[...document.querySelectorAll('input')].every(input => input.value === '')")
            .await
            .unwrap()
            .into_value::<bool>()
            .unwrap());
        for text in [
            "无法立即访问此资源 登录已成功，但是不符合访问此资源的条件。",
            "You cannot access this right now",
            "Error Code: 53003",
            "AADSTS53000",
        ] {
            page.set_content(format!(
                "<h1>{text}</h1><input name='passwd' type='password'>"
            ))
            .await
            .unwrap();
            assert_eq!(
                step(
                    page,
                    options.clone(),
                    Some("https://login.microsoftonline.com")
                )
                .await,
                "conditional_access_denied"
            );
            assert_eq!(
                page.evaluate("document.querySelector('input').value")
                    .await
                    .unwrap()
                    .into_value::<String>()
                    .unwrap(),
                ""
            );
        }
        page.set_content("<h1>More information required</h1><button>Next</button>")
            .await
            .unwrap();
        assert_eq!(
            step(
                page,
                options.clone(),
                Some("https://login.microsoftonline.com")
            )
            .await,
            "security_info_required"
        );
        assert_eq!(
            step(
                page,
                options.clone(),
                Some("https://mysignins.microsoft.com")
            )
            .await,
            "security_info_required"
        );
        page.set_content(
            r#"<input name="otc"><button id="idSubmit_SAOTCC_Continue">Verify</button>"#,
        )
        .await
        .unwrap();
        assert_eq!(
            step(
                page,
                options.clone(),
                Some("https://login.microsoftonline.com")
            )
            .await,
            "mfa_code"
        );
        options["otp"] = json!("654321");
        assert_eq!(
            step(page, options, Some("https://login.microsoftonline.com")).await,
            "mfa_submitted"
        );
        assert_eq!(
            page.evaluate("document.querySelector('input').value")
                .await
                .unwrap()
                .into_value::<String>()
                .unwrap(),
            "654321"
        );
        capture.close().await.unwrap();
        browser.close().await;
        let captured = tokio::fs::read_to_string(&trace_path).await.unwrap();
        assert!(captured.contains("request-extra"));
        assert!(captured.contains("trace_cookie=fixture-cookie"));
        assert!(!captured.contains("fixture-password"));
        assert!(!captured.contains("fixture-sso-password"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(trace_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
