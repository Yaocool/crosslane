//! crosslane 子命令实现。

use anyhow::{Context, Result};

pub mod account;
pub mod runtime;

/// Read an explicit confirmation for a destructive operation.
pub async fn confirm(prompt: &str) -> Result<bool> {
    use std::io::Write;
    use tokio::io::{AsyncBufReadExt, BufReader};

    eprint!("{prompt} [y/N] ");
    let _flush_result = std::io::stderr().flush();
    let mut line = String::new();
    BufReader::new(tokio::io::stdin())
        .read_line(&mut line)
        .await
        .context("读取确认输入失败")?;
    Ok(is_confirmation(&line))
}

/// Skip the prompt only when the caller explicitly opted into non-interactive execution.
pub async fn confirm_unless(yes: bool, prompt: &str) -> Result<bool> {
    if yes {
        Ok(true)
    } else {
        confirm(prompt).await
    }
}

fn is_confirmation(value: &str) -> bool {
    matches!(value.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::is_confirmation;

    #[test]
    fn destructive_confirmation_accepts_only_y_or_yes() {
        for accepted in ["y", "Y", "yes", " YES "] {
            assert!(is_confirmation(accepted), "{accepted}");
        }
        for rejected in ["", "n", "no", "true", "1"] {
            assert!(!is_confirmation(rejected), "{rejected}");
        }
    }
}
