use anyhow::{anyhow, Context, Result};
use clap::Subcommand;
use crosslane_core::kiro_admin::KiroAdminCredentials;
use crosslane_ipc::protocol::{method, KiroAdminCredentialsView};
use zeroize::Zeroizing;

use crate::{client::AdminClient, output::print_json};

#[derive(Debug, Subcommand)]
pub enum KiroAdminCommand {
    /// 完整替换 Kiro 管理凭证；AK/SK 隐藏输入，保存后热生效。
    #[command(
        visible_alias = "set",
        after_help = "交互输入 AK、SK、可选 STS SessionToken 以及区域/范围。再次 configure 为完整替换，省略的范围设置会恢复默认。\n--stdin 从管道或文件读取 JSON，字段为 access_key_id、secret_access_key、session_token、region、profile_arn、application_arn、identity_center_region；前两项必填，region 默认 us-east-1，其余可省略。\n密钥不放在命令行参数中，避免进入历史和进程列表；不会打印、返回或写入 config.toml。\n若返回 persistence_warning，修改已热生效，但磁盘持久性未确认；请检查存储后重试原操作。\n\n示例：\n  crosslane admin kiro configure\n  crosslane admin kiro configure --stdin < /secure/kiro-admin-input.json"
    )]
    Configure {
        /// 从非终端 stdin 读取完整 JSON 凭证；错误信息不回显输入。
        #[arg(long)]
        stdin: bool,
    },
    /// 查看配置来源和状态，不显示 AK、SK、SessionToken。
    #[command(
        after_help = "示例：\n  crosslane admin kiro show\n  crosslane --json admin kiro show\n\n只显示本地配置状态；用企业管理 GET /profiles 验证 AWS 身份与权限。"
    )]
    Show,
    /// 删除保存的凭证并停用 Kiro 远程管理；不会回落到旧环境变量。
    #[command(
        after_help = "示例：\n  crosslane admin kiro clear\n\n移除当前保存的凭证并持久化停用标记，后续管理请求热停用；业务服务继续运行。恢复可重新 configure，或明确 use-env。"
    )]
    Clear,
    /// 显式恢复 daemon 启动时的环境变量凭证。
    #[command(
        after_help = "示例：\n  crosslane admin kiro use-env\n\n写入环境来源标记，热切换回 daemon 继承的 CROSSLANE_KIRO_ADMIN_* 设置。若环境凭证无效，保留当前来源；该命令不读取调用方 shell 或修改进程环境。"
    )]
    UseEnv,
}

async fn input(stdin: bool) -> Result<KiroAdminCredentials> {
    if stdin {
        use std::io::IsTerminal;
        use tokio::io::AsyncReadExt;
        if std::io::stdin().is_terminal() {
            return Err(anyhow!(
                "--stdin 需要管道或文件；终端输入请直接使用 configure，密钥会隐藏输入"
            ));
        }
        let mut bytes = Zeroizing::new(Vec::new());
        tokio::io::stdin()
            .take(65537)
            .read_to_end(&mut bytes)
            .await
            .context("读取 Kiro 凭证失败")?;
        if bytes.len() > 65536 {
            return Err(anyhow!("Kiro 凭证输入超过 64 KiB"));
        }
        serde_json::from_slice(&bytes).map_err(|_| {
            anyhow!(
                "Kiro 凭证 JSON 无效；须提供 access_key_id 和 secret_access_key，且不能有未知字段"
            )
        })
    } else {
        tokio::task::spawn_blocking(interactive)
            .await
            .context("凭证交互输入中断")?
    }
}

#[cfg(unix)]
fn interactive() -> Result<KiroAdminCredentials> {
    use crate::cli::secret_input::prompt_hidden_line;
    use std::io::{BufRead, Write};
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("交互配置需要终端；自动化请使用 configure --stdin")?;
    fn secret(tty: &mut std::fs::File, prompt: &str, optional: bool) -> Result<String> {
        prompt_hidden_line(tty, prompt, optional)
    }
    fn optional(tty: &mut std::fs::File, prompt: &str) -> Result<Option<String>> {
        write!(tty, "{prompt}")?;
        tty.flush()?;
        let mut line = String::new();
        if std::io::BufReader::new(tty).read_line(&mut line)? == 0 {
            return Err(anyhow!("终端输入已结束"));
        }
        Ok((!line.trim().is_empty()).then(|| line.trim().to_owned()))
    }
    writeln!(
        tty,
        "Kiro 管理凭证完整替换；密钥隐藏输入，仅发送给 daemon 并加密保存。"
    )?;
    let access_key_id = Zeroizing::new(secret(&mut tty, "AWS AccessKeyId（隐藏输入）：", false)?);
    let secret_access_key = Zeroizing::new(secret(
        &mut tty,
        "AWS SecretAccessKey（隐藏输入）：",
        false,
    )?);
    let session_token = Zeroizing::new(secret(
        &mut tty,
        "STS SessionToken（长期密钥留空，隐藏输入）：",
        true,
    )?);
    Ok(KiroAdminCredentials {
        access_key_id: access_key_id.to_string(),
        secret_access_key: secret_access_key.to_string(),
        session_token: (!session_token.is_empty()).then(|| session_token.to_string()),
        region: optional(&mut tty, "Kiro Profile 区域 [us-east-1]：")?
            .unwrap_or_else(|| "us-east-1".into()),
        profile_arn: optional(&mut tty, "Profile ARN（可选，留空自动发现）：")?,
        application_arn: optional(&mut tty, "Application ARN（可选）：")?,
        identity_center_region: optional(&mut tty, "Identity Center 区域（可选）：")?,
    })
}

#[cfg(not(unix))]
fn interactive() -> Result<KiroAdminCredentials> {
    Err(anyhow!("当前平台请使用 configure --stdin 提供凭证"))
}

pub(super) async fn run(
    client: &mut AdminClient,
    command: KiroAdminCommand,
    json: bool,
) -> Result<()> {
    let (method, params) = match command {
        KiroAdminCommand::Configure { stdin } => (
            method::ADMIN_KIRO_CREDENTIALS_CONFIGURE,
            serde_json::to_value(input(stdin).await?)?,
        ),
        KiroAdminCommand::Show => (method::ADMIN_KIRO_CREDENTIALS_SHOW, serde_json::Value::Null),
        KiroAdminCommand::Clear => (
            method::ADMIN_KIRO_CREDENTIALS_CLEAR,
            serde_json::Value::Null,
        ),
        KiroAdminCommand::UseEnv => (
            method::ADMIN_KIRO_CREDENTIALS_USE_ENV,
            serde_json::Value::Null,
        ),
    };
    let result: KiroAdminCredentialsView = client.call(method, params).await?;
    if json {
        return print_json(&result);
    }
    if let Some(warning) = &result.persistence_warning {
        eprintln!("注意：{warning}");
    }
    println!(
        "Kiro 管理凭证：{}",
        if result.configured {
            "已配置（尚未验证 AWS 权限）"
        } else {
            "未配置或已停用"
        }
    );
    println!("来源：{}", result.source);
    println!("加密凭证文件：{}", result.credentials_file);
    if let Some(region) = result.region {
        println!("区域：{region}");
    }
    println!(
        "STS SessionToken：{}",
        if result.has_session_token {
            "已设置"
        } else {
            "未设置"
        }
    );
    for (name, value) in [
        ("Profile ARN", result.profile_arn),
        ("Application ARN", result.application_arn),
        ("Identity Center 区域", result.identity_center_region),
    ] {
        if let Some(value) = value {
            println!("{name}：{value}");
        }
    }
    if method != method::ADMIN_KIRO_CREDENTIALS_SHOW {
        println!("后续 Kiro 管理请求已使用新设置，无需重启 crosslaned。");
    }
    Ok(())
}
