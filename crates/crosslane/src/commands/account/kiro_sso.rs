//! Kiro IAM Identity Center login. CSV and stdin carry one login password.

use anyhow::{anyhow, Context, Result};
use crosslane_ipc::protocol::{method, AccountAddSsoParams, AccountSummary};
use futures::{stream, StreamExt};

use crate::client::AdminClient;
use crate::output::print_json;

#[derive(Clone, Copy)]
pub(super) struct LoginOptions<'a> {
    pub start_url: &'a str,
    pub region: &'a str,
    pub headful: bool,
    pub tags: &'a [String],
}

pub(super) struct BatchOptions<'a> {
    pub login: LoginOptions<'a>,
    pub concurrency: usize,
    pub json: bool,
}

/// Both public Kiro commands and CSV rows use this exact RPC contract.
pub(super) async fn add(
    client: &mut AdminClient,
    email: &str,
    password: &str,
    options: LoginOptions<'_>,
) -> Result<AccountSummary> {
    let params = AccountAddSsoParams {
        email: email.to_owned(),
        password: password.to_owned(),
        start_url: options.start_url.to_owned(),
        region: options.region.to_owned(),
        headful: options.headful,
        tags: options.tags.to_vec(),
    };
    client
        .call(method::ACCOUNT_ADD_SSO, serde_json::to_value(params)?)
        .await
}

pub(super) async fn run_batch(
    client: &AdminClient,
    file: &str,
    options: BatchOptions<'_>,
) -> Result<()> {
    if !(1..=8).contains(&options.concurrency) {
        return Err(anyhow!("并发数必须在 1..=8 之间"));
    }
    // Validate every row before starting any account login.
    let raw = super::read_batch_source(file).await?;
    let rows = parse_csv(&raw)?;
    let count = rows.len();
    let socket = client.socket_path();
    let login = options.login;
    let mut pending = stream::iter(rows.into_iter().enumerate().map(
        |(index, (email, password))| {
            let socket = socket.clone();
            async move {
                eprintln!("[{}/{}] 正在添加 Kiro 账号 {email}", index + 1, count);
                let mut client = AdminClient::connect(socket);
                let result = add(&mut client, &email, &password, login).await;
                (index + 1, email, result)
            }
        },
    ))
    .buffer_unordered(options.concurrency);

    let mut failures = Vec::new();
    let mut successes = Vec::new();
    while let Some((index, email, result)) = pending.next().await {
        match result {
            Ok(summary) => {
                if !options.json {
                    println!("已添加 {}（{}）", summary.email, summary.id);
                }
                eprintln!("[{index}/{count}] Kiro 账号 {email} 添加成功");
                successes.push(summary);
            }
            Err(error) => {
                eprintln!("[{index}/{count}] Kiro 账号 {email} 添加失败：{error}");
                failures.push(format!("{email}: {error}"));
            }
        }
    }
    if options.json {
        print_json(&serde_json::json!({
            "accounts": successes,
            "errors": failures,
            "complete": failures.is_empty()
        }))?;
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "{} 个账号登录失败：\n{}",
            failures.len(),
            failures.join("\n")
        ))
    }
}

pub(super) fn parse_csv(raw: &str) -> Result<Vec<(String, String)>> {
    let mut rows = Vec::new();
    let mut first_record = true;
    for (index, line) in raw.trim_start_matches('\u{feff}').lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let fields = super::parse_csv_line(line.trim_start_matches('\u{feff}'))
            .with_context(|| format!("CSV 第 {} 行无效", index + 1))?;
        if first_record {
            first_record = false;
            if fields
                .first()
                .is_some_and(|field| field.trim().eq_ignore_ascii_case("email"))
            {
                if fields.len() != 2 || !fields[1].trim().eq_ignore_ascii_case("password") {
                    return Err(anyhow!("Kiro CSV 表头必须为 email,password"));
                }
                continue;
            }
            if fields.first().is_some_and(|field| {
                matches!(
                    field.trim(),
                    "github_username" | "username" | "sso_username"
                )
            }) {
                return Err(anyhow!(
                    "Kiro CSV 仅使用 email,password，不使用 Copilot 凭据列"
                ));
            }
        }
        if fields.len() != 2 || fields[0].trim().is_empty() || fields[1].is_empty() {
            return Err(anyhow!(
                "CSV 第 {} 行必须只有 email,password 两列",
                index + 1
            ));
        }
        rows.push((fields[0].trim().to_ascii_lowercase(), fields[1].clone()));
    }
    if rows.is_empty() {
        return Err(anyhow!("CSV 中没有账号"));
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kiro_csv_preserves_the_login_password_and_accepts_bom_crlf_and_quotes() {
        for raw in [
            "email,password\n\"Example@example.com\",\" p,\"\"w \"\n",
            "\u{feff}email,password\r\n\"Example@example.com\",\" p,\"\"w \"\r\n",
            "\nemail,password\n\"Example@example.com\",\" p,\"\"w \"\n",
        ] {
            assert_eq!(
                parse_csv(raw).unwrap(),
                [("example@example.com".into(), " p,\"w ".into())]
            );
        }
        assert_eq!(parse_csv("example@example.com,password").unwrap().len(), 1);
    }

    #[test]
    fn kiro_csv_rejects_copilot_columns_and_invalid_rows_before_login() {
        for raw in [
            "github_username,github_password\nexample,password",
            "username,password\nexample,password",
            "email,new_password\nexample@example.com,password",
            "email,password\nexample@example.com,password,extra",
            "email,password\nexample@example.com,\"unterminated",
        ] {
            assert!(parse_csv(raw).is_err());
        }
    }
}
