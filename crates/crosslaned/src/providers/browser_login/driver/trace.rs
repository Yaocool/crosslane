//! Opt-in sensitive header capture. Never record form bodies or normal daemon logs.

use std::path::Path;

use anyhow::{Context, Result};
use chromiumoxide::cdp::browser_protocol::network::{
    EnableParams, EventRequestWillBeSent, EventRequestWillBeSentExtraInfo, EventResponseReceived,
    EventResponseReceivedExtraInfo,
};
use chromiumoxide::Page;
use futures::StreamExt;
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub(super) struct HeaderTrace {
    cancel: CancellationToken,
    writer: JoinHandle<Result<()>>,
}

impl HeaderTrace {
    pub(super) async fn start(page: &Page, path: &Path) -> Result<Self> {
        let directory = path
            .parent()
            .context("header trace needs a parent directory")?;
        tokio::fs::create_dir_all(directory).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).await?;
        }
        let mut options = tokio::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(path)
            .await
            .context("cannot create protected header trace")?;
        let mut requests = page.event_listener::<EventRequestWillBeSent>().await?;
        let mut request_extra = page
            .event_listener::<EventRequestWillBeSentExtraInfo>()
            .await?;
        let mut responses = page.event_listener::<EventResponseReceived>().await?;
        let mut response_extra = page
            .event_listener::<EventResponseReceivedExtraInfo>()
            .await?;
        page.execute(EnableParams::default()).await?;
        let cancel = CancellationToken::new();
        let shutdown = cancel.clone();
        let writer = tokio::spawn(async move {
            loop {
                let value = tokio::select! {
                    _ = shutdown.cancelled() => break,
                    Some(event) = requests.next() => json!({
                        "kind":"request", "request_id":event.request_id,
                        "url":event.request.url, "method":event.request.method,
                        "headers":event.request.headers, "redirect":event.redirect_response,
                    }),
                    Some(event) = request_extra.next() => json!({
                        "kind":"request-extra", "request_id":event.request_id,
                        "headers":event.headers,
                    }),
                    Some(event) = responses.next() => json!({
                        "kind":"response", "request_id":event.request_id,
                        "url":event.response.url, "status":event.response.status,
                        "headers":event.response.headers,
                    }),
                    Some(event) = response_extra.next() => json!({
                        "kind":"response-extra", "request_id":event.request_id,
                        "status":event.status_code, "headers":event.headers,
                    }),
                    else => break,
                };
                let mut value = value;
                value["captured_at"] = json!(super::super::now_secs());
                let mut line = serde_json::to_vec(&value)?;
                line.push(b'\n');
                file.write_all(&line).await?;
            }
            file.flush().await?;
            file.sync_all().await?;
            Ok(())
        });
        Ok(Self { cancel, writer })
    }

    pub(super) async fn close(self) -> Result<()> {
        self.cancel.cancel();
        self.writer.await.context("header trace writer stopped")?
    }
}
