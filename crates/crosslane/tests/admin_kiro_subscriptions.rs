#![cfg(unix)]

use crosslane_ipc::protocol::{decode_line, encode_line, method, Request, Response, RpcError};
use serde_json::{json, Value};
use std::{process::Output, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::process::Command;

const USER: &str = "11111111-1111-4111-8111-111111111111";

// Exercise the real CLI parser and transport without AWS or a real daemon.
async fn cli(args: &[&str], result: Result<Value, RpcError>) -> (Request, Output) {
    let home = tempfile::tempdir().unwrap();
    let socket = home.path().join("admin.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let (reader, mut writer) = stream.into_split();
        let line = BufReader::new(reader)
            .lines()
            .next_line()
            .await
            .unwrap()
            .unwrap();
        let request: Request = decode_line(&line).unwrap();
        let response = match result {
            Ok(value) => Response::ok(request.id, value),
            Err(error) => Response::err(request.id, error),
        };
        writer
            .write_all(encode_line(&response).unwrap().as_bytes())
            .await
            .unwrap();
        request
    });
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        Command::new(env!("CARGO_BIN_EXE_crosslane"))
            .current_dir(home.path())
            .env("CROSSLANE_HOME", home.path())
            .env("CROSSLANE_PROVIDER", "copilot")
            .env_remove("CROSSLANE_WRAPPER_LOCAL_ONLY")
            .arg("--socket")
            .arg(socket)
            .args(args)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    (server.await.unwrap(), output)
}

fn accepted(preview: bool) -> Value {
    json!({
        "status":if preview {"preview"} else {"api_accepted"},
        "operation":"CreateAssignment","user_id":USER,"requested_plan":"power",
        "application_assignment_requested":false,"upstream_request_id":"fixture-aws-request"
    })
}

fn successful(output: &Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}

#[tokio::test]
async fn activation_forwards_exact_user_plan_and_scope_without_client_secrets() {
    let user_arn = format!("arn:aws:identitystore:::user/{USER}");
    let result = accepted(false);
    let (request, output) = cli(
        &[
            "--json",
            "admin",
            "kiro",
            "subscription",
            "add",
            &user_arn,
            "--plan",
            "power",
            "--region",
            "us-east-1",
            "--profile-arn",
            "profile-fixture",
            "--application-arn",
            "application-fixture",
            "--identity-center-region",
            "eu-west-1",
            "--legacy",
        ],
        Ok(result.clone()),
    )
    .await;
    assert_eq!(request.method, method::ADMIN_KIRO_SUBSCRIPTION_ADD);
    assert_eq!(
        request.params,
        json!({
            "user":user_arn,"plan":"power","dry_run":false,
            "scope":{"region":"us-east-1","profile_arn":"profile-fixture",
                "application_arn":"application-fixture","identity_center_region":"eu-west-1","legacy":true}
        })
    );
    assert_eq!(
        serde_json::from_str::<Value>(&successful(&output)).unwrap(),
        result
    );
}

#[tokio::test]
async fn preview_and_aliases_keep_write_intent_and_acceptance_explicit() {
    for (alias, preview) in [("create", true), ("activate", false)] {
        let mut args = vec![
            "admin",
            "kiro",
            "subscriptions",
            alias,
            "fixture@example.com",
            "--plan",
            "power",
        ];
        if preview {
            args.extend([
                "--dry-run",
                "--region",
                "eu-west-1",
                "--profile-arn",
                "profile-fixture",
            ]);
        }
        let (request, output) = cli(&args, Ok(accepted(preview))).await;
        assert_eq!(request.method, method::ADMIN_KIRO_SUBSCRIPTION_ADD);
        assert_eq!(request.params["dry_run"], preview);
        assert_eq!(
            request.params["scope"],
            if preview {
                json!({"legacy":false,"region":"eu-west-1","profile_arn":"profile-fixture"})
            } else {
                json!({"legacy":false})
            }
        );
        let text = successful(&output);
        assert!(text.contains("crosslane --socket "), "{text}");
        if preview {
            assert!(
                text.contains("--region eu-west-1 --profile-arn profile-fixture"),
                "{text}"
            );
        }
        assert!(
            text.contains(if preview {
                "未提交"
            } else {
                "AWS 已受理"
            }),
            "{text}"
        );
        assert!(text.contains(&format!("subscription show {USER}")));
    }
}

#[tokio::test]
async fn subscription_queries_display_direct_group_and_pending_plans() {
    let claims = json!([
        {"principal_type":"USER","principal_id":USER,"plan":"power","pending_plan":"pro-plus"},
        {"principal_type":"GROUP","principal_id":"group-fixture","plan":null,"subscription_type":"FUTURE_PLAN"}
    ]);
    let (request, output) = cli(
        &["admin", "kiro", "subscription", "list"],
        Ok(json!({"scope":{},"subscriptions":claims})),
    )
    .await;
    assert_eq!(request.method, method::ADMIN_KIRO_SUBSCRIPTION_LIST);
    let text = successful(&output);
    for expected in ["USER", "GROUP", "power", "pro-plus", "FUTURE_PLAN"] {
        assert!(text.contains(expected), "{text}");
    }
    let (request, output) = cli(
        &[
            "admin",
            "kiro",
            "subscription",
            "show",
            "fixture@example.com",
        ],
        Ok(
            json!({"user":{"user_name":"fixture@example.com","user_id":USER,
            "direct_subscriptions":[claims[0]]},"group_subscriptions":[claims[1]]}),
        ),
    )
    .await;
    assert_eq!(request.method, method::ADMIN_KIRO_SUBSCRIPTION_SHOW);
    assert_eq!(request.params["user"], "fixture@example.com");
    let text = successful(&output);
    for expected in ["fixture@example.com", "直接订阅", "组继承", "pro-plus"] {
        assert!(text.contains(expected), "{text}");
    }
    let (request, output) = cli(
        &["admin", "kiro", "subscription", "plans"],
        Ok(json!([{"plan":"power","subscription_type":"Q_DEVELOPER_STANDALONE_POWER"}])),
    )
    .await;
    assert_eq!(request.method, method::ADMIN_KIRO_SUBSCRIPTION_PLANS);
    assert_eq!(request.params, Value::Null);
    assert!(successful(&output).contains("Q_DEVELOPER_STANDALONE_POWER"));
}

#[tokio::test]
async fn upgrades_and_downgrades_preserve_plan_scope_and_write_intent() {
    for (command, method, plan, previous, effect, label) in [
        (
            "upgrade",
            method::ADMIN_KIRO_SUBSCRIPTION_UPGRADE,
            "power",
            "pro",
            "immediate",
            "升级",
        ),
        (
            "downgrade",
            method::ADMIN_KIRO_SUBSCRIPTION_DOWNGRADE,
            "pro",
            "power",
            "next_month",
            "降级",
        ),
    ] {
        for preview in [false, true] {
            let mut args = vec![
                "admin",
                "kiro",
                "subscription",
                command,
                USER,
                "--plan",
                plan,
                "--region",
                "us-east-1",
                "--profile-arn",
                "profile-fixture",
            ];
            if preview {
                args.extend(["--dry-run", "--json"]);
            }
            let mut result = accepted(preview);
            result["operation"] = "UpdateAssignment".into();
            result["requested_plan"] = plan.into();
            result["previous_plan"] = previous.into();
            result["expected_effect"] = effect.into();
            let (request, output) = cli(&args, Ok(result.clone())).await;
            assert_eq!(request.method, method);
            assert_eq!(
                request.params,
                json!({"user":USER,"plan":plan,"dry_run":preview,
                "scope":{"region":"us-east-1","profile_arn":"profile-fixture","legacy":false}})
            );
            let text = successful(&output);
            if preview {
                assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), result);
            } else {
                assert!(text.contains(&format!("订阅{label}请求")), "{text}");
                assert!(text.contains("AWS 已受理"), "{text}");
                assert!(
                    text.contains("--region us-east-1 --profile-arn profile-fixture"),
                    "{text}"
                );
                assert!(
                    text.contains(if command == "upgrade" {
                        "即时生效"
                    } else {
                        "下月生效"
                    }),
                    "{text}"
                );
            }
        }
    }
}

#[tokio::test]
async fn cancellation_defaults_to_period_end_and_keeps_explicit_modes_and_aliases() {
    for (command, mode, preview, legacy) in [
        ("cancel", None, false, false),
        ("cancel", Some("end-of-period"), true, false),
        ("delete", Some("end_of_period"), true, false),
        ("remove", Some("immediate"), false, true),
        ("rm", Some("immediate"), true, false),
    ] {
        let mut args = vec!["admin", "kiro", "subscription", command, USER];
        if let Some(mode) = mode {
            args.extend(["--mode", mode]);
        }
        if preview {
            args.push("--dry-run");
        }
        if legacy {
            args.push("--legacy");
        }
        let wire_mode = if mode == Some("immediate") {
            "immediate"
        } else {
            "end_of_period"
        };
        let mut result = accepted(preview);
        result["operation"] = "DeleteAssignment".into();
        result["requested_plan"] = Value::Null;
        result["previous_plan"] = "power".into();
        result["deletion_mode"] = wire_mode.into();
        result["expected_effect"] = if wire_mode == "immediate" {
            "access_immediate_billing_end_of_month"
        } else {
            "end_of_month"
        }
        .into();
        let (request, output) = cli(&args, Ok(result)).await;
        assert_eq!(request.method, method::ADMIN_KIRO_SUBSCRIPTION_CANCEL);
        assert_eq!(
            request.params,
            json!({"user":USER,"mode":wire_mode,"dry_run":preview,"scope":{"legacy":legacy}})
        );
        let text = successful(&output);
        assert!(
            text.contains(if preview {
                "未提交订阅取消请求"
            } else {
                "已受理 Kiro 订阅取消请求"
            }),
            "{text}"
        );
        assert!(
            text.contains(if wire_mode == "immediate" {
                "计费至当月底"
            } else {
                "周期结束时取消"
            }),
            "{text}"
        );
        if legacy {
            assert!(text.contains("--legacy"), "{text}");
        }
    }
}

#[tokio::test]
async fn discovery_and_user_queries_preserve_native_results_and_group_boundaries() {
    let profiles = json!({"account_id":"123456789012","kiro_region":"us-east-1","profiles":[
        {"arn":"profile-fixture","instance_arn":"instance-fixture","identity_center_region":"eu-west-1","application_arns":["app-one","app-two"]}
    ]});
    let (request, output) = cli(
        &[
            "admin",
            "kiro",
            "subscription",
            "profiles",
            "--region",
            "us-east-1",
            "--legacy",
        ],
        Ok(profiles),
    )
    .await;
    assert_eq!(request.method, method::ADMIN_KIRO_SUBSCRIPTION_PROFILES);
    assert_eq!(
        request.params,
        json!({"scope":{"region":"us-east-1","legacy":true}})
    );
    let text = successful(&output);
    for value in [
        "123456789012",
        "profile-fixture",
        "eu-west-1",
        "app-one, app-two",
    ] {
        assert!(text.contains(value), "{text}");
    }

    let scope = json!({"account_id":"123456789012","kiro_region":"us-east-1","profile_arn":"profile-fixture",
        "instance_arn":"instance-fixture","identity_center_region":"eu-west-1","application_arn":"app-one","identity_store_id":"d-fixture","legacy":false});
    let (request, output) = cli(
        &[
            "admin",
            "kiro",
            "subscription",
            "scope",
            "--profile-arn",
            "profile-fixture",
            "--json",
        ],
        Ok(scope.clone()),
    )
    .await;
    assert_eq!(request.method, method::ADMIN_KIRO_SUBSCRIPTION_SCOPE);
    assert_eq!(request.params["scope"]["profile_arn"], "profile-fixture");
    assert_eq!(
        serde_json::from_str::<Value>(&successful(&output)).unwrap(),
        scope
    );

    let users = json!({"scope":scope,"users":[{"user_id":USER,"user_name":"fixture@example.com","display_name":"Fixture User",
        "direct_subscriptions":[{"plan":"power","pending_plan":"pro"}]}],
        "group_subscriptions":[{"principal_id":"group-fixture","plan":"pro-plus"}]});
    let (request, output) = cli(
        &[
            "admin",
            "kiro",
            "subscription",
            "users",
            "--application-arn",
            "app-one",
        ],
        Ok(users),
    )
    .await;
    assert_eq!(request.method, method::ADMIN_KIRO_SUBSCRIPTION_USERS);
    assert_eq!(request.params["scope"]["application_arn"], "app-one");
    let text = successful(&output);
    for value in [
        "fixture@example.com",
        "Fixture User",
        "power",
        "group-fixture",
        "pro-plus",
        "未逐一展开",
    ] {
        assert!(text.contains(value), "{text}");
    }
}

#[tokio::test]
async fn aws_errors_keep_request_id_outcome_and_nonzero_exit_in_both_formats() {
    let details = json!({"code":"aws_access_denied","http_status":403,
        "message":"AWS rejected the request","operation":"CreateAssignment",
        "upstream_code":"AccessDeniedException","upstream_request_id":"fixture-failed-request",
        "write_outcome":"rejected"});
    for json_output in [false, true] {
        let mut args = vec![
            "admin",
            "kiro",
            "subscription",
            "add",
            USER,
            "--plan",
            "pro",
        ];
        if json_output {
            args.push("--json");
        }
        let (_, output) = cli(
            &args,
            Err(RpcError {
                code: 403,
                message: details.to_string(),
            }),
        )
        .await;
        assert!(!output.status.success());
        if json_output {
            let error: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(error["error"], details);
        } else {
            let text = String::from_utf8_lossy(&output.stderr);
            for expected in [
                "aws_access_denied",
                "AccessDeniedException",
                "fixture-failed-request",
                "rejected",
            ] {
                assert!(text.contains(expected), "{text}");
            }
        }
    }
    let (_, output) = cli(
        &["admin", "kiro", "subscription", "plans"],
        Err(RpcError::unknown_method(
            method::ADMIN_KIRO_SUBSCRIPTION_PLANS,
        )),
    )
    .await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("请同时升级 crosslane 和 crosslaned"));
}

#[test]
fn help_and_invalid_plan_are_handled_without_loading_environment_or_contacting_daemon() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join(".env"), b"\xff\xfe").unwrap();
    for args in [
        vec!["admin", "kiro", "subscription"],
        vec!["admin", "kiro", "subscriptions"],
        vec!["admin", "kiro", "subscription", "add", "--help"],
        vec!["help", "admin", "kiro", "subscription", "add"],
        vec!["admin", "kiro", "subscription", "help", "--all"],
        vec!["admin", "kiro", "subscription", "profiles", "--help"],
        vec!["admin", "kiro", "subscription", "scope", "--help"],
        vec!["admin", "kiro", "subscription", "users", "--help"],
        vec!["admin", "kiro", "subscription", "upgrade", "--help"],
        vec!["admin", "kiro", "subscription", "downgrade", "--help"],
        vec!["admin", "kiro", "subscription", "cancel", "--help"],
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_crosslane"))
            .current_dir(home.path())
            .env_remove("CROSSLANE_WRAPPER_LOCAL_ONLY")
            .args(["--socket", "/missing-kiro-subscription-fixture.sock"])
            .args(&args)
            .output()
            .unwrap();
        let text = successful(&output);
        if args.contains(&"--all") {
            for name in [
                "plans",
                "profiles",
                "scope",
                "users",
                "list",
                "show",
                "add",
                "upgrade",
                "downgrade",
                "cancel",
            ] {
                assert!(text.contains(name), "{text}");
            }
        } else if args
            .iter()
            .any(|arg| ["profiles", "scope", "users"].contains(arg))
        {
            assert!(text.contains("--region"), "{text}");
        } else {
            assert!(text.contains("--dry-run"), "{text}");
        }
    }
    for command in ["add", "upgrade", "downgrade"] {
        for plan in [None, Some("invalid")] {
            let mut args = vec!["admin", "kiro", "subscription", command, USER];
            if let Some(plan) = plan {
                args.extend(["--plan", plan]);
            }
            let output = std::process::Command::new(env!("CARGO_BIN_EXE_crosslane"))
                .current_dir(home.path())
                .env_remove("CROSSLANE_WRAPPER_LOCAL_ONLY")
                .args(["--socket", "/missing-kiro-subscription-fixture.sock"])
                .args(args)
                .output()
                .unwrap();
            assert!(!output.status.success());
            let text = String::from_utf8_lossy(&output.stderr);
            assert!(text.contains("--plan"), "{text}");
            assert!(!text.contains("无法连接"), "{text}");
        }
    }
}

#[test]
fn invalid_cancellation_modes_and_discovery_flags_never_reach_the_daemon() {
    let home = tempfile::tempdir().unwrap();
    for args in [
        vec!["cancel", USER, "--mode", "invalid"],
        vec!["cancel", USER, "--plan", "pro"],
        vec!["cancel", USER, "--legacy"],
        vec!["cancel", USER, "--legacy", "--mode", "end-of-period"],
        vec!["profiles", "--profile-arn", "profile-fixture"],
        vec!["profiles", "--application-arn", "app-fixture"],
        vec!["profiles", "--identity-center-region", "us-east-1"],
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_crosslane"))
            .current_dir(home.path())
            .env_remove("CROSSLANE_WRAPPER_LOCAL_ONLY")
            .args([
                "--socket",
                "/missing-kiro-subscription-fixture.sock",
                "admin",
                "kiro",
                "subscription",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!error.contains("无法连接"), "{error}");
    }
}
