//! 管理面 Unix socket 客户端。

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use crosslane_ipc::protocol::{decode_line, encode_line, method, Request, Response};
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// 管理面客户端。
pub struct AdminClient {
    socket: PathBuf,
    next_id: u64,
}

impl AdminClient {
    /// 指定 socket 路径构造客户端。
    pub fn connect(socket: PathBuf) -> Self {
        Self { socket, next_id: 1 }
    }

    /// 返回当前管理 socket，供受限并发的批量命令创建独立连接。
    pub fn socket_path(&self) -> PathBuf {
        self.socket.clone()
    }

    /// 调用一个方法并反序列化结果。
    pub async fn call<T: DeserializeOwned>(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<T> {
        self.call_with_response_timeout(method, params, response_timeout(method))
            .await
    }

    async fn call_with_response_timeout<T: DeserializeOwned>(
        &mut self,
        method: &str,
        params: serde_json::Value,
        response_timeout: Duration,
    ) -> Result<T> {
        let id = self.next_id;
        self.next_id += 1;
        let connected =
            tokio::time::timeout(Duration::from_secs(5), UnixStream::connect(&self.socket))
                .await
                .context("连接管理 socket 超时（5 秒）")?;
        let stream = connected.with_context(|| {
            format!(
                "无法连接 {}。crosslaned 未运行？可用 `systemctl status crosslaned` 检查服务",
                self.socket.display()
            )
        })?;
        let (read_half, mut write_half) = stream.into_split();
        let line = encode_line(&Request::new(id, method, params))?;
        tokio::time::timeout(Duration::from_secs(5), async {
            write_half.write_all(line.as_bytes()).await?;
            write_half.flush().await
        })
        .await
        .context("发送管理请求超时（5 秒）")?
        .context("发送请求失败")?;

        let mut lines = BufReader::new(read_half).lines();
        let raw = tokio::time::timeout(response_timeout, lines.next_line())
            .await
            .with_context(|| {
                format!(
                    "等待 {method} 响应超时（{} 秒）",
                    response_timeout.as_secs()
                )
            })?
            .context("读取响应失败")?
            .ok_or_else(|| anyhow!("服务端在响应前关闭了连接"))?;
        match decode_line::<Response>(&raw)? {
            Response::Ok { result, .. } => {
                serde_json::from_value(result).context("解析响应结果失败")
            }
            Response::Err { error, .. } => {
                Err(anyhow!("服务端返回错误 {}: {}", error.code, error.message))
            }
        }
    }
}

fn response_timeout(method_name: &str) -> Duration {
    match method_name {
        method::STATUS | method::POOL => Duration::from_secs(15),
        method::LOGS => Duration::from_secs(45),
        method::ACCOUNT_ADD_SSO => Duration::from_secs(300),
        method::DIAGNOSE_ACCOUNT
        | method::DIAGNOSE_ENDPOINTS
        | method::ACCOUNT_REFRESH
        | method::V2_ACCOUNT_REFRESH
        | method::ACCOUNT_PROBE
        | method::V2_ACCOUNT_PROBE
        | method::TASK_RUN => Duration::from_secs(600),
        _ => Duration::from_secs(120),
    }
}

/// 解析 socket 路径：命令行参数、配置文件、默认值。
pub async fn resolve_socket(explicit: Option<String>) -> PathBuf {
    if let Some(path) = explicit {
        return PathBuf::from(path);
    }
    let paths = crosslane_core::paths::Paths::from_env();
    match crosslane_store::config_loader::load_config(&paths.config_file).await {
        Ok(config) => PathBuf::from(config.admin.socket),
        Err(_) => PathBuf::from(crosslane_core::config::Config::default().admin.socket),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncBufReadExt;
    use tokio::net::UnixListener;

    #[test]
    fn interactive_login_and_batch_probes_get_longer_response_windows() {
        assert!(response_timeout(method::ACCOUNT_ADD_SSO) > Duration::from_secs(120));
        assert!(response_timeout(method::ACCOUNT_REFRESH) > Duration::from_secs(120));
        assert!(response_timeout(method::DIAGNOSE_ACCOUNT) > Duration::from_secs(300));
    }

    #[tokio::test]
    async fn silent_admin_socket_times_out() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("admin.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, _write) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            lines.next_line().await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        let mut client = AdminClient::connect(socket);
        let error = client
            .call_with_response_timeout::<serde_json::Value>(
                method::STATUS,
                serde_json::json!({}),
                Duration::from_millis(25),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("响应超时"), "{error:?}");
        server.abort();
    }
}
