//! 进程启动时的环境变量加载。

use anyhow::{Context, Result};

/// 从当前目录开始向上查找并加载 `.env`。
///
/// 未找到文件时继续使用进程环境；已存在的系统环境变量不会被覆盖。
pub fn load_dotenv() -> Result<()> {
    match dotenvy::dotenv() {
        Ok(_) => Ok(()),
        Err(dotenvy::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("failed to load .env file"),
    }
}
