//! Ephemeral, origin-specific Copilot credentials from CSV or a local terminal.

use std::collections::BTreeSet;

use anyhow::{anyhow, Context, Result};
use crosslane_ipc::protocol::CopilotBrowserCredentials;

pub(super) struct CsvLogin {
    pub credentials: CopilotBrowserCredentials,
    pub label: Option<String>,
}

pub(super) fn parse_csv(raw: &str) -> Result<Vec<CsvLogin>> {
    let mut lines = raw
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty());
    let (_, header) = lines.next().ok_or_else(|| anyhow!("CSV 中没有账号"))?;
    let headers = super::parse_csv_line(header.trim_start_matches('\u{feff}'))?
        .into_iter()
        .map(|column| match column.trim() {
            "username" => "github_username".to_owned(),
            "password" => "github_password".to_owned(),
            other => other.to_owned(),
        })
        .collect::<Vec<_>>();
    let allowed = [
        "github_username",
        "github_password",
        "sso_username",
        "sso_password",
        "sso_start_url",
        "label",
    ];
    let unique = headers.iter().collect::<BTreeSet<_>>();
    if !headers.iter().any(|field| field == "github_username")
        || unique.len() != headers.len()
        || headers
            .iter()
            .any(|field| !allowed.contains(&field.as_str()))
    {
        return Err(anyhow!("CSV 必须包含 github_username 表头；可选 github_password,sso_username,sso_password,sso_start_url,label，且列名不能重复"));
    }
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for (index, line) in lines {
        let fields = super::parse_csv_line(line)
            .with_context(|| format!("CSV 第 {} 行格式无效", index + 1))?;
        if fields.len() != headers.len() {
            return Err(anyhow!("CSV 第 {} 行列数与表头不同", index + 1));
        }
        let value = |name: &str| {
            headers
                .iter()
                .position(|field| field == name)
                .map(|column| fields[column].clone())
                .filter(|value| !value.is_empty())
        };
        let credentials = CopilotBrowserCredentials {
            github_username: value("github_username")
                .unwrap_or_default()
                .trim()
                .to_owned(),
            github_password: value("github_password"),
            sso_username: value("sso_username").map(|value| value.trim().to_owned()),
            sso_password: value("sso_password"),
            sso_start_url: value("sso_start_url").map(|value| value.trim().to_owned()),
        };
        validate(&credentials).with_context(|| format!("CSV 第 {} 行凭证配置无效", index + 1))?;
        if !seen.insert(credentials.github_username.to_ascii_lowercase()) {
            return Err(anyhow!("CSV 第 {} 行包含重复 GitHub 用户名", index + 1));
        }
        result.push(CsvLogin {
            credentials,
            label: value("label"),
        });
    }
    if result.is_empty() {
        return Err(anyhow!("CSV 中没有账号"));
    }
    Ok(result)
}

fn validate(credentials: &CopilotBrowserCredentials) -> Result<()> {
    if credentials.github_username.is_empty()
        || credentials.github_username.chars().any(char::is_whitespace)
    {
        return Err(anyhow!("需提供 GitHub 用户名或邮箱，不能包含空白字符"));
    }
    if credentials.github_username.contains('@')
        && (credentials.sso_username.is_some()
            || credentials.sso_password.is_some()
            || credentials.sso_start_url.is_some())
    {
        return Err(anyhow!(
            "SSO 登录需提供完整 GitHub 用户名，不能用 Azure 邮箱代替"
        ));
    }
    let azure_user = credentials
        .sso_username
        .as_deref()
        .is_some_and(|value| !value.is_empty());
    let azure_password = credentials
        .sso_password
        .as_deref()
        .is_some_and(|value| !value.is_empty());
    if azure_user != azure_password {
        return Err(anyhow!("sso_username 和 sso_password 必须同时填写"));
    }
    if !azure_password
        && credentials
            .github_password
            .as_deref()
            .is_none_or(str::is_empty)
    {
        return Err(anyhow!("需提供 GitHub 密码或 Azure SSO 凭证"));
    }
    Ok(())
}

pub(super) async fn prompt(
    username: Option<String>,
    sso_username: Option<String>,
    sso_start_url: Option<String>,
) -> Result<CopilotBrowserCredentials> {
    tokio::task::spawn_blocking(move || interactive(username, sso_username, sso_start_url))
        .await
        .context("交互输入任务中断")?
}

#[cfg(unix)]
fn interactive(
    username: Option<String>,
    sso_username: Option<String>,
    sso_start_url: Option<String>,
) -> Result<CopilotBrowserCredentials> {
    use std::io::Write;
    let mut tty = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty")
        .context("交互登录需要终端；请使用 --batch、--credentials-stdin，或 --username 配合 --password-stdin")?;
    writeln!(
        tty,
        "远端无痕浏览器登录；密码仅用于本次授权，不写入账号配置。"
    )?;
    let username = match username {
        Some(value) => value,
        None => line(&mut tty, "GitHub 用户名或邮箱（SSO 必须用完整用户名）：")?,
    };
    let mode = if sso_username.is_some() {
        "1".to_owned()
    } else {
        let mode = line(
            &mut tty,
            "登录方式 [1=Azure SSO，2=GitHub 密码，3=两套凭证]（默认 1）：",
        )?;
        if mode.is_empty() {
            "1".to_owned()
        } else {
            mode
        }
    };
    if !matches!(mode.as_str(), "1" | "2" | "3") {
        return Err(anyhow!("登录方式必须为 1、2 或 3"));
    }
    let github_password = if mode != "1" {
        Some(secret(&mut tty, "GitHub 密码（隐藏输入）：")?)
    } else {
        None
    };
    let (sso_username, sso_password) = if mode != "2" {
        let user = match sso_username {
            Some(value) => value,
            None => line(&mut tty, "Azure SSO 登录名：")?,
        };
        (
            Some(user),
            Some(secret(&mut tty, "Azure SSO 密码（隐藏输入）：")?),
        )
    } else {
        (None, None)
    };
    let credentials = CopilotBrowserCredentials {
        github_username: username.trim().to_owned(),
        github_password,
        sso_username,
        sso_password,
        sso_start_url,
    };
    validate(&credentials)?;
    Ok(credentials)
}

#[cfg(not(unix))]
fn interactive(
    _: Option<String>,
    _: Option<String>,
    _: Option<String>,
) -> Result<CopilotBrowserCredentials> {
    Err(anyhow!(
        "当前平台请使用 --credentials-stdin 或 --batch 提供凭证"
    ))
}

#[cfg(unix)]
fn line(tty: &mut std::fs::File, prompt: &str) -> Result<String> {
    use std::io::{BufRead, Write};
    write!(tty, "{prompt}")?;
    tty.flush()?;
    let mut value = String::new();
    if std::io::BufReader::new(tty).read_line(&mut value)? == 0 {
        return Err(anyhow!("终端输入已结束"));
    }
    Ok(value.trim().to_owned())
}

#[cfg(unix)]
fn secret(tty: &mut std::fs::File, prompt: &str) -> Result<String> {
    use std::io::Write;
    write!(tty, "{prompt}")?;
    tty.flush()?;
    let result = hidden_line(tty);
    writeln!(tty)?;
    result
}

#[cfg(unix)]
struct HiddenTerminal {
    fd: std::os::fd::RawFd,
    original: libc::termios,
}

#[cfg(unix)]
impl HiddenTerminal {
    fn new(fd: std::os::fd::RawFd) -> Result<Self> {
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: tcgetattr initializes the live output pointer on success.
        if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error()).context("读取终端模式失败");
        }
        // SAFETY: the successful tcgetattr call initialized this value.
        let original = unsafe { original.assume_init() };
        let mut hidden = original;
        // Read Ctrl-C ourselves, so every normal cancellation restores echo.
        hidden.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG);
        hidden.c_cc[libc::VMIN] = 1;
        hidden.c_cc[libc::VTIME] = 0;
        // SAFETY: fd is a live terminal and hidden is fully initialized.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &hidden) } != 0 {
            return Err(std::io::Error::last_os_error()).context("关闭终端密码回显失败");
        }
        Ok(Self { fd, original })
    }
}

#[cfg(unix)]
impl Drop for HiddenTerminal {
    fn drop(&mut self) {
        // SAFETY: the guard is dropped before its borrowed terminal is closed.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

#[cfg(unix)]
fn hidden_line(tty: &mut std::fs::File) -> Result<String> {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    let _guard = HiddenTerminal::new(tty.as_raw_fd())?;
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0u8];
        if tty.read(&mut byte)? == 0 {
            return Err(anyhow!("终端输入已结束"));
        }
        match byte[0] {
            b'\r' | b'\n' => break,
            3 | 4 | 27 => return Err(anyhow!("密码输入已取消")),
            8 | 127 => {
                if bytes.pop().is_some_and(|last| last & 0xc0 == 0x80) {
                    while bytes.last().is_some_and(|last| last & 0xc0 == 0x80) {
                        bytes.pop();
                    }
                    bytes.pop();
                }
            }
            21 => bytes.clear(),
            value if value >= 32 => bytes.push(value),
            _ => {}
        }
        if bytes.len() > 16384 {
            return Err(anyhow!("密码输入过长"));
        }
    }
    if bytes.is_empty() {
        return Err(anyhow!("密码不能为空"));
    }
    String::from_utf8(bytes).map_err(|_| anyhow!("密码输入必须是有效 UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn terminal_mode(fd: std::os::fd::RawFd) -> libc::termios {
        let mut mode = std::mem::MaybeUninit::uninit();
        // SAFETY: fd is owned by the test; tcgetattr initializes mode on success.
        assert_eq!(unsafe { libc::tcgetattr(fd, mode.as_mut_ptr()) }, 0);
        // SAFETY: tcgetattr succeeded.
        unsafe { mode.assume_init() }
    }

    #[cfg(unix)]
    #[test]
    fn hidden_password_input_does_not_echo_and_restores_terminal_after_cancel() {
        use std::io::Write;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::time::{Duration, Instant};

        for (input, expected) in [
            ("  pass密\u{7f}word  \n", Some("  password  ")),
            ("discard\u{15}replacement\n", Some("replacement")),
            ("private\u{3}", None),
            ("private\u{4}", None),
            ("\n", None),
        ] {
            let (mut master_fd, mut slave_fd) = (-1, -1);
            // SAFETY: output pointers are valid; optional attributes/name are null.
            assert_eq!(
                unsafe {
                    libc::openpty(
                        &mut master_fd,
                        &mut slave_fd,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                },
                0
            );
            // SAFETY: openpty returned two new, independently owned descriptors.
            let mut master = unsafe { std::fs::File::from_raw_fd(master_fd) };
            let slave = unsafe { std::fs::File::from_raw_fd(slave_fd) };
            let original = terminal_mode(slave.as_raw_fd());
            let mut reader = slave.try_clone().unwrap();
            let task = std::thread::spawn(move || hidden_line(&mut reader));
            let deadline = Instant::now() + Duration::from_secs(2);
            while terminal_mode(slave.as_raw_fd()).c_lflag & libc::ECHO != 0 {
                assert!(
                    Instant::now() < deadline,
                    "password reader did not disable echo"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            master.write_all(input.as_bytes()).unwrap();
            while !task.is_finished() {
                assert!(Instant::now() < deadline, "password reader did not finish");
                std::thread::sleep(Duration::from_millis(5));
            }
            let result = task.join().unwrap();
            match expected {
                Some(value) => assert_eq!(result.unwrap(), value),
                None => assert!(result.is_err()),
            }
            // BSD may set internal pending-input flags when restoring canonical mode.
            let restored = terminal_mode(slave.as_raw_fd());
            let changed = libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG;
            assert_eq!(restored.c_lflag & changed, original.c_lflag & changed);
            assert_eq!(restored.c_cc, original.c_cc);
            let mut poll = libc::pollfd {
                fd: master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll receives a valid single-element array and does not retain it.
            assert_eq!(
                unsafe { libc::poll(&mut poll, 1, 0) },
                0,
                "password was echoed"
            );
        }
    }

    #[test]
    fn csv_keeps_github_and_azure_credentials_separate() {
        let rows = parse_csv("github_username,github_password,sso_username,sso_password,label\nmanaged_company,,person@example.com,\"secret,with,commas\",team\noctocat,gh-secret,,,personal\n").unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].credentials.github_password.is_none());
        assert_eq!(
            rows[0].credentials.sso_password.as_deref(),
            Some("secret,with,commas")
        );
        assert_eq!(
            rows[1].credentials.github_password.as_deref(),
            Some("gh-secret")
        );
        assert!(rows[1].credentials.sso_password.is_none());
        assert_eq!(rows[0].label.as_deref(), Some("team"));
        assert!(parse_csv("username,password\noctocat,secret\n").is_ok());
        assert!(parse_csv("username,password\nuser@example.com,secret\n").is_ok());
    }

    #[test]
    fn malformed_csv_is_rejected_before_login_without_echoing_passwords() {
        for raw in [
            "octocat,private-secret\n",
            "github_username,sso_password\nmanaged_company,private-secret\n",
            "github_username,github_password\na,private-secret\nA,private-secret\n",
            "github_username,sso_username,sso_password\nuser@example.com,user@example.com,private-secret\n",
            "github_username,username,github_password\na,a,private-secret\n",
        ] {
            let error = parse_csv(raw).err().expect("invalid credentials");
            assert!(!format!("{error:#}").contains("private-secret"));
        }
    }
}
