//! 管理面 Unix socket 服务端。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use crosslane_ipc::protocol::{decode_line, encode_line, Request, Response, RpcError};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use crate::admin::handlers::dispatch;
use crate::state::AppState;

/// 启动管理面监听，直到 shutdown 触发。
pub async fn serve(
    state: Arc<AppState>,
    socket_path: PathBuf,
    shutdown: CancellationToken,
) -> Result<()> {
    crosslane_store::bootstrap::ensure_socket_parent(&socket_path).await?;
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("bind {}", socket_path.display()))?;
    restrict_socket_permissions(&socket_path)?;
    info!(socket = %socket_path.display(), "admin plane listening");

    let mut accept_backoff = std::time::Duration::from_millis(25);
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _address)) => {
                    accept_backoff = std::time::Duration::from_millis(25);
                    let connection_state = Arc::clone(&state);
                    tokio::spawn(async move {
                        if let Err(connection_error) = handle_connection(connection_state, stream).await {
                            debug!(error = %connection_error, "admin connection ended with error");
                        }
                    });
                }
                Err(accept_error) => {
                    error!(error = %accept_error, retry_ms = accept_backoff.as_millis(), "admin accept failed");
                    tokio::select! {
                        () = shutdown.cancelled() => break,
                        () = tokio::time::sleep(accept_backoff) => {}
                    }
                    accept_backoff = (accept_backoff * 2).min(std::time::Duration::from_secs(5));
                },
            },
        }
    }

    drop(listener);
    let _cleanup = tokio::fs::remove_file(&socket_path).await;
    info!("admin plane stopped");
    Ok(())
}

#[cfg(unix)]
fn restrict_socket_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 600 {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_socket_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

async fn handle_connection(state: Arc<AppState>, stream: UnixStream) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = match decode_line::<Request>(&line) {
            Ok(request) => dispatch(&state, request).await,
            Err(decode_error) => Response::err(0, RpcError::bad_params(decode_error.to_string())),
        };
        let encoded = encode_line(&response).unwrap_or_else(|_| {
            "{\"id\":0,\"error\":{\"code\":500,\"message\":\"encode failed\"}}\n".into()
        });
        write_half.write_all(encoded.as_bytes()).await?;
        write_half.flush().await?;
    }
    Ok(())
}
