//! IAM Identity Center authorization-code + PKCE login.

#[cfg(feature = "sso")]
pub(crate) mod browser;

#[cfg(feature = "sso")]
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(feature = "sso")]
use anyhow::Context;
use anyhow::{anyhow, Result};
#[cfg(any(feature = "sso", test))]
use base64::Engine;
#[cfg(feature = "sso")]
use crosslane_core::account::AuthMethod;
use crosslane_core::account::Credentials;
#[cfg(any(feature = "sso", test))]
use rand::RngCore;
#[cfg(feature = "sso")]
use serde::{Deserialize, Serialize};
#[cfg(any(feature = "sso", test))]
use sha2::{Digest, Sha256};
#[cfg(feature = "sso")]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(feature = "sso")]
use tokio::net::TcpListener;
#[cfg(feature = "sso")]
use url::Url;

#[cfg(feature = "sso")]
const SCOPES: &[&str] = &[
    "codewhisperer:completions",
    "codewhisperer:analysis",
    "codewhisperer:conversations",
    "codewhisperer:transformations",
    "codewhisperer:taskassist",
];

#[derive(Debug, Clone)]
pub struct SsoLoginRequest {
    pub email: String,
    pub password: String,
    pub start_url: String,
    pub region: String,
    pub headful: bool,
}

pub async fn login(request: SsoLoginRequest) -> Result<Credentials> {
    if !request.start_url.starts_with("https://") {
        return Err(anyhow!("SSO start URL must use https://"));
    }
    if request.email.trim().is_empty() || request.password.is_empty() {
        return Err(anyhow!("email and password are required"));
    }
    #[cfg(not(feature = "sso"))]
    {
        tracing::warn!(
            region = %request.region,
            headful = request.headful,
            "SSO login requested but browser support is not compiled in"
        );
        Err(anyhow!(
            "crosslaned was built without SSO browser support; rebuild with `--features sso` or import credentials"
        ))
    }
    #[cfg(feature = "sso")]
    login_full(request).await
}

#[cfg(feature = "sso")]
async fn login_full(request: SsoLoginRequest) -> Result<Credentials> {
    tracing::info!(region = %request.region, headful = request.headful, "starting SSO login");
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/oauth/callback");
    let oidc = format!("https://oidc.{}.amazonaws.com", request.region);
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .build()?;
    let registered = register_client(&client, &oidc, &redirect_uri, &request.start_url).await?;
    tracing::info!(region = %request.region, "SSO OIDC client registered");
    let verifier = random_urlsafe(32);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let state = uuid::Uuid::new_v4().to_string();
    let mut authorize = Url::parse(&format!("{oidc}/authorize"))?;
    authorize
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &registered.client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scopes", &SCOPES.join(","))
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    let mut session = browser::BrowserSession::launch(
        authorize.as_str(),
        &request.email,
        &request.password,
        request.headful,
    )
    .await?;
    let callback_result = tokio::time::timeout(
        Duration::from_secs(120),
        wait_for_callback(listener, &state),
    )
    .await;
    session.close().await;
    let callback =
        callback_result.map_err(|_| anyhow!("SSO login timed out after 120 seconds"))??;
    tracing::info!(region = %request.region, "SSO authorization callback received");
    let tokens = exchange_code(
        &client,
        &oidc,
        &registered,
        &redirect_uri,
        &callback,
        &verifier,
    )
    .await?;
    tracing::info!(region = %request.region, "SSO authorization-code exchange completed");
    Ok(Credentials {
        access_token: tokens.access_token,
        refresh_token: Some(tokens.refresh_token),
        client_id: Some(registered.client_id),
        client_secret: Some(registered.client_secret),
        region: request.region,
        expires_at: now_secs() + tokens.expires_in,
        auth_method: AuthMethod::Idc,
    })
}

#[cfg(feature = "sso")]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Registration {
    client_id: String,
    client_secret: String,
}

#[cfg(feature = "sso")]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RegistrationRequest<'a> {
    client_name: &'a str,
    client_type: &'a str,
    scopes: &'a [&'a str],
    grant_types: [&'a str; 2],
    redirect_uris: [&'a str; 1],
    issuer_url: &'a str,
}

#[cfg(feature = "sso")]
async fn register_client(
    client: &reqwest::Client,
    oidc: &str,
    redirect_uri: &str,
    issuer_url: &str,
) -> Result<Registration> {
    let response = client
        .post(format!("{oidc}/client/register"))
        .header(reqwest::header::USER_AGENT, "KiroIDE")
        .json(&RegistrationRequest {
            client_name: "crosslane",
            client_type: "public",
            scopes: SCOPES,
            grant_types: ["authorization_code", "refresh_token"],
            redirect_uris: [redirect_uri],
            issuer_url,
        })
        .send()
        .await?;
    decode_response(response, "client registration").await
}

#[cfg(feature = "sso")]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
}

#[cfg(feature = "sso")]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TokenRequest<'a> {
    client_id: &'a str,
    client_secret: &'a str,
    grant_type: &'a str,
    code: &'a str,
    redirect_uri: &'a str,
    code_verifier: &'a str,
}

#[cfg(feature = "sso")]
async fn exchange_code(
    client: &reqwest::Client,
    oidc: &str,
    registration: &Registration,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> Result<TokenResponse> {
    let response = client
        .post(format!("{oidc}/token"))
        .header(reqwest::header::USER_AGENT, "KiroIDE")
        .json(&TokenRequest {
            client_id: &registration.client_id,
            client_secret: &registration.client_secret,
            grant_type: "authorization_code",
            code,
            redirect_uri,
            code_verifier: verifier,
        })
        .send()
        .await?;
    decode_response(response, "authorization-code exchange").await
}

#[cfg(feature = "sso")]
async fn decode_response<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    operation: &str,
) -> Result<T> {
    let status = response.status();
    let bytes = response.bytes().await?;
    if !status.is_success() {
        let detail = String::from_utf8_lossy(&bytes);
        let detail = detail.chars().take(512).collect::<String>();
        return Err(anyhow!("SSO {operation} failed ({status}): {detail}"));
    }
    serde_json::from_slice(&bytes).with_context(|| format!("invalid SSO {operation} response"))
}

#[cfg(feature = "sso")]
async fn wait_for_callback(listener: TcpListener, expected_state: &str) -> Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut buffer = vec![0_u8; 16 * 1024];
        let read = stream.read(&mut buffer).await?;
        let request = String::from_utf8_lossy(&buffer[..read]);
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1));
        let Some(target) = target else {
            write_callback_response(&mut stream, 400, "Invalid callback request").await?;
            continue;
        };
        let url = Url::parse(&format!("http://127.0.0.1{target}"))?;
        if url.path() != "/oauth/callback" {
            write_callback_response(&mut stream, 404, "Not found").await?;
            continue;
        }
        let query = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        if let Some(error) = query.get("error") {
            let detail = query
                .get("error_description")
                .map(|value| value.as_ref())
                .unwrap_or_else(|| error.as_ref());
            write_callback_response(&mut stream, 400, "Login was rejected").await?;
            return Err(anyhow!("SSO provider rejected login: {detail}"));
        }
        if query.get("state").map(|value| value.as_ref()) != Some(expected_state) {
            write_callback_response(&mut stream, 400, "Invalid OAuth state").await?;
            return Err(anyhow!("SSO callback state mismatch"));
        }
        let code = query
            .get("code")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("SSO callback did not contain an authorization code"))?
            .to_string();
        write_callback_response(
            &mut stream,
            200,
            "Login complete. You can close this window and return to the terminal.",
        )
        .await?;
        return Ok(code);
    }
}

#[cfg(feature = "sso")]
async fn write_callback_response(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    message: &str,
) -> Result<()> {
    let reason = if status == 200 { "OK" } else { "Bad Request" };
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>crosslane login</title><style>body{{font:16px system-ui;max-width:680px;margin:15vh auto;padding:2rem}}</style><h1>crosslane</h1><p>{message}</p>"
    );
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

#[cfg(any(feature = "sso", test))]
fn random_urlsafe(bytes: usize) -> String {
    let mut random = vec![0_u8; bytes];
    rand::thread_rng().fill_bytes(&mut random);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random)
}

#[cfg(feature = "sso")]
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_values_are_urlsafe_and_unique() {
        let first = random_urlsafe(32);
        let second = random_urlsafe(32);
        assert_ne!(first, second);
        assert!(!first.contains(['+', '/', '=']));
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(first.as_bytes()));
        assert!(!challenge.contains(['+', '/', '=']));
    }
}
