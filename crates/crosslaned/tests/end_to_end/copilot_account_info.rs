use super::*;
use serde_json::{json, Value};

#[tokio::test]
async fn copilot_account_list_tracks_safe_metadata_and_json_stream_aiu() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    let upstream = MockServer::start().await;
    for (endpoint, body) in [
        ("/user", json!({"id":7,"login":"account-info-test"})),
        (
            "/copilot_internal/v2/token",
            json!({
                "token":"fake-api-secret","expires_at":4_000_000_000i64,
                "refresh_in":86340,"sku":"business-seat","endpoints":{"api":upstream.uri()}
            }),
        ),
        (
            "/models",
            json!({"data":[{
                "id":"gpt-test","capabilities":{"supported_endpoints":["/v1/messages"]}
            }]}),
        ),
        (
            "/copilot_internal/user",
            json!({
                "copilot_plan":"business","access_type_sku":"business-seat","token_based_billing":true,
                "quota_reset_date_utc":"2026-11-01T00:00:00.000Z",
                "quota_snapshots":{"premium_interactions":{
                    "unlimited":true,"entitlement":0,"remaining":0,"quota_remaining":0,
                    "percent_remaining":100,"token_based_billing":true,"credits_used":1
                }},"unknown_private_field":"must-not-be-stored"
            }),
        ),
    ] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&upstream)
            .await;
    }
    Mock::given(method("POST")).and(path("/v1/messages"))
        .and(header("authorization", "Bearer fake-api-secret"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let usage = json!({"input_tokens":14,"output_tokens":4,"cache_read_input_tokens":7});
            let billing = json!({"total_nano_aiu":17_000_000});
            if body["stream"] == true {
                ResponseTemplate::new(200).insert_header("content-type", "text/event-stream")
                    .set_body_string(format!(
                        "event: message_start\ndata: {}\n\nevent: message_delta\ndata: {}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
                        json!({"type":"message_start","message":{"id":"msg-test","type":"message","model":"gpt-test","role":"assistant","content":[],"usage":{"input_tokens":14,"output_tokens":0},"copilot_usage":{"total_nano_aiu":1_000_000}}}),
                        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":usage,"copilot_usage":billing})
                    ))
            } else {
                ResponseTemplate::new(200).set_body_json(json!({
                    "id":"msg-test","type":"message","model":"gpt-test","role":"assistant",
                    "content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn",
                    "usage":usage,"copilot_usage":billing
                }))
            }
        }).expect(2).mount(&upstream).await;
    let port = unused_tcp_port();
    let daemon = Daemon::start_copilot(port, &upstream.uri()).await;
    let imported = expect_ok(
        daemon
            .call(
                "v2.account.importToken",
                json!({
                    "provider":"copilot","token":"fake-github-secret"
                }),
            )
            .await,
    );
    let account_id = imported["id"].as_str().unwrap();
    let initial = expect_ok(
        daemon
            .call("v2.account.list", json!({"provider":"copilot"}))
            .await,
    );
    let details = &initial["accounts"][0]["details"];
    assert_eq!(details["info"]["plan"], "business");
    assert_eq!(details["info"]["api_token_expires_at"], 4_000_000_000i64);
    assert!(details["info"]["github_token_expires_at"].is_null());
    assert_eq!(
        details["info"]["quota_snapshots"]["premium_interactions"]["unlimited"],
        true
    );
    assert_eq!(
        details["info"]["quota_snapshots"]["premium_interactions"]["credits_used"],
        1.0
    );
    assert_eq!(
        details["quota_summary"],
        json!({
            "source":"copilot_internal/user","scope":"personal","unit":"ai_credits",
            "used":1.0,"total":null,"remaining":null,"percent_remaining":null,
            "unlimited":true,"cached":false
        })
    );
    assert!(details["local_aiu_usage"].is_null());
    for secret in [
        "fake-github-secret",
        "fake-api-secret",
        "must-not-be-stored",
    ] {
        assert!(!initial.to_string().contains(secret));
    }
    for stream in [false, true] {
        let response = reqwest::Client::new().post(format!("http://127.0.0.1:{port}/v1/messages"))
            .bearer_auth(daemon.api_key.as_deref().unwrap())
            .header("anthropic-version", "2023-06-01")
            .json(&json!({"model":"gpt-test","messages":[{"role":"user","content":"hello"}],"max_tokens":16,"stream":stream}))
            .send().await.unwrap();
        assert_eq!(response.status(), 200);
        let _body = response.text().await.unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let details = loop {
        let account = expect_ok(
            daemon
                .call(
                    "v2.account.show",
                    json!({"provider":"copilot","id":account_id}),
                )
                .await,
        );
        if account["details"]["local_aiu_usage"]["billed_requests"] == 2 {
            break account["details"].clone();
        }
        assert!(Instant::now() < deadline, "{account}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(details["local_aiu_usage"]["total_nano_aiu"], 34_000_000);
    let keys = expect_ok(
        daemon
            .call("apikey.list", json!({"provider":"copilot"}))
            .await,
    );
    assert_eq!(keys[0]["usage"]["total_credits"], 0.0);
    let records = keys[0]["usage"]["history"].as_array().unwrap();
    assert_eq!(records.len(), 2);
    assert!(records
        .iter()
        .all(|record| record["account_id"] == account_id
            && record["provider_billing"]["amount"] == 17_000_000
            && record["cache_read_tokens"] == 7));
    expect_ok(
        daemon
            .call("apikey.resetUsage", json!({"id":"ak_copilot_e2e"}))
            .await,
    );
    let persisted: Value = serde_json::from_slice(
        &tokio::fs::read(daemon.home().join("daily.json"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        persisted["account_aiu_usage"][format!("copilot/{account_id}")]["total_nano_aiu"],
        34_000_000
    );
    let mixed = expect_ok(
        daemon
            .call("v2.account.list", json!({"provider":"all"}))
            .await,
    );
    assert_eq!(
        mixed["accounts"][0]["details"]["local_aiu_usage"],
        details["local_aiu_usage"]
    );
    assert_eq!(
        mixed["accounts"][0]["details"]["quota_summary"],
        details["quota_summary"]
    );
    daemon.stop().await;
}
