#![cfg(feature = "crs")]

use super::crs_isolated_runtime::{
    catalog, configure_daemon, import_profile_account, request, send,
};
use super::*;
use serde_json::json;

#[derive(Clone, Copy)]
enum Round {
    ContextOnly,
    ChargedEmpty(u64, f64),
    ChargedText(u64, u64, f64),
    Http500,
}

fn response(round: Round) -> ResponseTemplate {
    let mut body = match round {
        Round::ContextOnly => {
            event_stream_frame("contextUsageEvent", json!({"contextUsagePercentage":0.7}))
        }
        Round::ChargedEmpty(input, credits) => reported_usage(input, 0, credits),
        Round::ChargedText(input, output, credits) => {
            let mut body = event_stream_frame(
                "assistantResponseEvent",
                json!({"content":"ACCOUNTED_ONCE"}),
            );
            body.extend(reported_usage(input, output, credits));
            body
        }
        Round::Http500 => {
            return ResponseTemplate::new(500)
                .set_body_json(json!({"message":"Owned later-round supplier failure."}));
        }
    };
    // Repeated cumulative metadata in a single round is not a second charge.
    if let Round::ChargedEmpty(input, credits) = round {
        body.extend(reported_usage(input, 0, credits));
    }
    ResponseTemplate::new(200)
        .insert_header("content-type", "application/vnd.amazon.eventstream")
        .set_body_bytes(body)
}

fn reported_usage(input: u64, output: u64, credits: f64) -> Vec<u8> {
    event_stream_frame(
        "messageMetadataEvent",
        json!({"messageMetadataEvent":{"usage":{
            "inputTokens":input,"outputTokens":output,"creditsConsumed":credits
        }}}),
    )
}

async fn run_case(
    rounds: [Round; 2],
    isolated: bool,
    success: bool,
    expected_input: u64,
    expected_credits: f64,
) {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ListAvailableModels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(catalog()))
        .mount(&mock)
        .await;
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    Mock::given(method("POST"))
        .and(path("/generateAssistantResponse"))
        .respond_with(move |request: &wiremock::Request| {
            let index = observed.fetch_add(1, Ordering::SeqCst);
            assert!(index < rounds.len(), "a third generation must not occur");
            let target = request.headers.get("x-amz-target");
            if isolated && index == 1 {
                assert_eq!(
                    target.unwrap().to_str().unwrap(),
                    "KiroRuntimeService.GenerateAssistantResponse"
                );
            } else {
                assert!(target.is_none(), "same-policy attempt must use AmazonQ");
            }
            response(rounds[index])
        })
        .expect(2)
        .mount(&mock)
        .await;
    let port = unused_tcp_port();
    let daemon =
        Daemon::start_http(port, &format!("{}/generateAssistantResponse", mock.uri())).await;
    configure_daemon(
        &daemon,
        (!isolated).then_some(crosslane_core::config::Endpoint::Amazonq),
    )
    .await;
    model_policy::configure(&daemon, |config| config.upstream.max_retries = 0).await;
    import_profile_account(&daemon).await;
    let (status, body) = send(&daemon, port, &request(false)).await;
    assert_eq!(status == 200, success, "status={status}, body={body}");
    if success {
        assert!(body.contains("ACCOUNTED_ONCE"), "{body}");
    } else if matches!(rounds[1], Round::Http500) {
        assert_eq!(status, 500, "preserve supplier status: {body}");
    } else {
        assert_eq!(status, 502, "bounded empty failure: {body}");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    let stats = expect_ok(
        daemon
            .call("stats", json!({"detail":true,"recent":20}))
            .await,
    );
    let rows = stats["stats"]["recent_requests"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "one logical request record: {stats}");
    let row = &rows[0];
    assert_eq!(row["status"], status);
    assert_eq!(row["credits"], expected_credits);
    assert_eq!(
        row["account_id"],
        if expected_input > 0 {
            "acc_99999999"
        } else {
            ""
        }
    );
    if !success {
        assert_eq!(
            row["input_tokens"], expected_input,
            "no estimated input: {row}"
        );
        assert_eq!(row["output_tokens"], 0, "no estimated output: {row}");
    }
    let total = &stats["stats"]["total"];
    assert_eq!(total["requests"], 1, "{stats}");
    assert_eq!(total["successes"], u64::from(success), "{stats}");
    assert_eq!(total["failures"], u64::from(!success), "{stats}");
    assert_eq!(total["credits"], expected_credits, "{stats}");
    let keys = expect_ok(daemon.call("apikey.list", json!({"provider":"kiro"})).await);
    let usage = &keys[0]["usage"];
    assert_eq!(usage["total_credits"], expected_credits, "{keys}");
    let history = usage["history"].as_array().unwrap();
    if expected_input > 0 {
        assert_eq!(usage["total_requests"], 1, "one settlement: {keys}");
        assert_eq!(history.len(), 1, "one settlement: {keys}");
        assert_eq!(history[0]["credits"], expected_credits);
        assert_eq!(history[0]["input_tokens"], expected_input);
        assert_eq!(history[0]["token_usage_source"], "server");
        if !success {
            assert_eq!(history[0]["output_tokens"], 0);
        }
    } else {
        assert_eq!(
            usage["total_requests"], 0,
            "unreported usage stays uncharged: {keys}"
        );
        assert!(
            history.is_empty(),
            "unreported usage stays uncharged: {keys}"
        );
    }
    daemon.stop().await;
}

#[tokio::test]
async fn nonstream_later_http_failure_retains_completed_reported_usage() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    run_case(
        [Round::ChargedEmpty(10, 0.125), Round::Http500],
        false,
        false,
        10,
        0.125,
    )
    .await;
}

#[tokio::test]
async fn nonstream_isolated_empty_failure_retains_runtime_reported_usage() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    run_case(
        [Round::ContextOnly, Round::ChargedEmpty(20, 0.25)],
        true,
        false,
        20,
        0.25,
    )
    .await;
}

#[tokio::test]
async fn nonstream_success_settles_accumulated_usage_only_once() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    run_case(
        [
            Round::ChargedEmpty(10, 0.125),
            Round::ChargedText(20, 2, 0.25),
        ],
        false,
        true,
        30,
        0.375,
    )
    .await;
}

#[tokio::test]
async fn nonstream_bounded_empty_failure_settles_each_completed_round_once() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    run_case(
        [
            Round::ChargedEmpty(10, 0.125),
            Round::ChargedEmpty(20, 0.25),
        ],
        false,
        false,
        30,
        0.375,
    )
    .await;
}

#[tokio::test]
async fn nonstream_unreported_usage_is_not_estimated_on_later_failure() {
    let _guard = HTTP_TEST_LOCK.lock().await;
    run_case([Round::ContextOnly, Round::Http500], false, false, 0, 0.0).await;
}
