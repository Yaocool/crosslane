use super::*;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

struct StreamingServer {
    url: String,
    task: JoinHandle<()>,
}

impl Drop for StreamingServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn usable_catalog_does_not_wait_for_refresh_or_missing_account_catalogs() {
    let server = MockServer::start().await;
    let directory = tempfile::tempdir().unwrap();
    mount_account_exchange(&server, "github-token").await;
    let provider = mock_provider(&server, directory.path()).await;
    let account = provider
        .import_token("github-token".into(), None)
        .await
        .unwrap();
    let mut unrelated = provider.accounts.read().await[0].clone();
    unrelated.id = "acc_unrelated_missing_catalog".into();
    provider.accounts.write().await.push(unrelated);
    provider
        .model_cache
        .write()
        .await
        .accounts
        .get_mut(&account.id)
        .unwrap()
        .updated_at = now_secs() - provider.settings.model_cache_ttl_secs - 1;
    let _refresh = provider.model_refresh.lock().await;
    let models = tokio::time::timeout(Duration::from_secs(1), provider.models(false))
        .await
        .expect("usable catalog must not queue behind background refresh")
        .unwrap();
    assert_eq!(models[0].id, "gpt-test");
    server.verify().await;
}

#[tokio::test]
async fn model_refresh_overlaps_account_queries() {
    let server = MockServer::start().await;
    let directory = tempfile::tempdir().unwrap();
    mount_account_exchange(&server, "github-token").await;
    let provider = mock_provider(&server, directory.path()).await;
    let first = provider
        .import_token("github-token".into(), None)
        .await
        .unwrap();
    let mut second = provider.accounts.read().await[0].clone();
    second.id = "acc_second".into();
    second.github_user_id += 1;
    provider.accounts.write().await.push(second.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let mut handlers = tokio::task::JoinSet::new();
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let barrier = barrier.clone();
            handlers.spawn(async move {
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let count = socket.read(&mut buf).await.unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&buf[..count]);
                }
                // Neither query completes until both are in flight.
                barrier.wait().await;
                let body = json!({"data":[{"id":"gpt-test","capabilities":{"supported_endpoints":["/chat/completions"]}}]}).to_string();
                let wire = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len());
                socket.write_all(wire.as_bytes()).await.unwrap();
            });
        }
        while let Some(result) = handlers.join_next().await {
            result.unwrap();
        }
    });
    let upstream = StreamingServer { url, task };
    let mut token = provider.token_cache.read().await[&first.id].clone();
    token.endpoint = upstream.url.clone();
    for id in [&first.id, &second.id] {
        provider
            .token_cache
            .write()
            .await
            .insert(id.clone(), token.clone());
    }
    let models = tokio::time::timeout(Duration::from_secs(2), provider.models(true))
        .await
        .expect("catalog queries must overlap")
        .unwrap();
    assert_eq!(models[0].id, "gpt-test");
}

#[test]
fn model_refresh_concurrency_is_configurable_and_rejects_zero() {
    let mut config = ProviderConfig::default();
    assert_eq!(
        CopilotSettings::from_config(&config).model_refresh_concurrency,
        32
    );
    config
        .settings
        .insert("model_refresh_concurrency".into(), json!(64));
    let settings = CopilotSettings::from_config(&config);
    assert_eq!(settings.model_refresh_concurrency, 64);
    settings.validate().unwrap();
    for value in [json!(0), json!(-1), json!(1.5), json!("64"), Value::Null] {
        config
            .settings
            .insert("model_refresh_concurrency".into(), value);
        assert!(CopilotSettings::from_config(&config)
            .validate()
            .unwrap_err()
            .message
            .contains("model_refresh_concurrency"));
    }
}

#[tokio::test]
async fn model_refresh_scales_to_large_pools_with_the_configured_bound() {
    for (configured, account_count) in [(None, 65), (Some(64), 65), (Some(3), 11)] {
        let server = MockServer::start().await;
        let directory = tempfile::tempdir().unwrap();
        mount_account_exchange(&server, "github-token").await;
        let provider = mock_provider_with_settings(&server, directory.path(), |settings| {
            if let Some(value) = configured {
                settings.model_refresh_concurrency = value;
            }
        })
        .await;
        let first = provider
            .import_token("github-token".into(), None)
            .await
            .unwrap();
        let template = provider.accounts.read().await[0].clone();
        let accounts = (0..account_count)
            .map(|index| {
                let mut account = template.clone();
                account.id = format!("acc_{index:08x}");
                account.github_user_id += index as u64;
                account
            })
            .collect::<Vec<_>>();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(Semaphore::new(0));
        let (count, in_flight, maximum, arrival, gate) = (
            calls.clone(),
            active.clone(),
            peak.clone(),
            entered.clone(),
            release.clone(),
        );
        let task = tokio::spawn(async move {
            let mut handlers = tokio::task::JoinSet::new();
            for _ in 0..account_count {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (count, in_flight, maximum, arrival, gate) = (
                    count.clone(),
                    in_flight.clone(),
                    maximum.clone(),
                    arrival.clone(),
                    gate.clone(),
                );
                handlers.spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0; 1024];
                    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                        let count = socket.read(&mut buf).await.unwrap();
                        assert!(count > 0);
                        request.extend_from_slice(&buf[..count]);
                    }
                    let current = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(current, Ordering::SeqCst);
                    count.fetch_add(1, Ordering::SeqCst);
                    arrival.notify_one();
                    gate.acquire().await.unwrap().forget();
                    let body = json!({"data":[{"id":"gpt-test","capabilities":{"supported_endpoints":["/chat/completions"]}}]}).to_string();
                    let wire = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    socket.write_all(wire.as_bytes()).await.unwrap();
                });
            }
            while let Some(result) = handlers.join_next().await {
                result.unwrap();
            }
        });
        let upstream = StreamingServer { url, task };
        let mut token = provider.token_cache.read().await[&first.id].clone();
        token.endpoint = upstream.url.clone();
        for account in &accounts {
            provider
                .token_cache
                .write()
                .await
                .insert(account.id.clone(), token.clone());
        }
        *provider.accounts.write().await = accounts;
        let bound = provider
            .settings
            .model_refresh_concurrency
            .min(account_count);
        let refresh = provider.models(true);
        tokio::pin!(refresh);
        tokio::time::timeout(Duration::from_secs(2), async {
            while calls.load(Ordering::SeqCst) < bound {
                tokio::select! {
                    result = &mut refresh => panic!("refresh completed before the first wave: {result:?}"),
                    _ = entered.notified() => {}
                }
            }
        }).await.expect("configured concurrency must be reachable");
        // Keep polling discovery with a full wave blocked. An implementation
        // that starts more than the configured number would reach the server.
        tokio::select! {
            result = &mut refresh => panic!("refresh completed before release: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
        assert_eq!(calls.load(Ordering::SeqCst), bound);
        release.add_permits(account_count);
        let models = tokio::time::timeout(Duration::from_secs(5), &mut refresh)
            .await
            .expect("all accounts must finish discovery")
            .unwrap();
        assert_eq!(models[0].id, "gpt-test");
        assert_eq!(calls.load(Ordering::SeqCst), account_count);
        assert_eq!(peak.load(Ordering::SeqCst), bound);
        assert_eq!(
            provider.model_cache.read().await.accounts.len(),
            account_count
        );
    }
}

#[tokio::test]
async fn catalog_older_than_max_stale_is_not_used_for_requests() {
    let server = MockServer::start().await;
    let directory = tempfile::tempdir().unwrap();
    mount_account_exchange(&server, "github-token").await;
    let provider = mock_provider(&server, directory.path()).await;
    let account = provider
        .import_token("github-token".into(), None)
        .await
        .unwrap();
    server.verify().await;
    server.reset().await;
    provider
        .model_cache
        .write()
        .await
        .accounts
        .get_mut(&account.id)
        .unwrap()
        .updated_at = now_secs() - provider.settings.model_max_stale_secs;
    assert!(provider.models(false).await.is_err());
    assert!(provider
        .routing_snapshot(
            &Default::default(),
            "gpt-test",
            ProviderProtocol::OpenAiChat
        )
        .await
        .is_none());
}
