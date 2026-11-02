use super::*;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn review_http_success_does_not_hide_an_aws_error_envelope() {
    for body in [
        json!({"code":"AccessDenied","message":SECRET}),
        json!({"__type":null,"code":"AccessDenied","message":SECRET}),
        json!({"__type":"","code":"AccessDenied","message":SECRET}),
        json!({"code":"InternalServerError","message":SECRET}),
        json!({"code":"FutureError","message":SECRET}),
        json!({"__type":null,"message":SECRET}),
    ] {
        let (server, client) = fixture(vec![]).await;
        Mock::given(header(
            "x-amz-target",
            "KiroControlPlaneQDeveloperService.CreateAssignment",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(body.clone()))
        .expect(1)
        .mount(&server)
        .await;
        let error = client
            .mutate(
                &ScopeSelector::default(),
                USER,
                Mutation::Create(Plan::Pro),
                false,
            )
            .await
            .expect_err("an AWS error body must never be reported as api_accepted");
        assert_eq!(error.upstream_status, Some(200));
        assert_eq!(
            error.write_outcome,
            if body["code"] == "AccessDenied" {
                WriteOutcome::Rejected
            } else {
                WriteOutcome::Unknown
            }
        );
        assert!(!serde_json::to_string(&error).unwrap().contains(SECRET));
        assert_eq!(
            write_requests(&server.received_requests().await.unwrap()).len(),
            1
        );
    }
}

#[tokio::test]
async fn review_foreign_identity_store_responses_cannot_authorize_a_subscription() {
    for operation in [
        "AWSIdentityStore.GetUserId",
        "AWSIdentityStore.DescribeUser",
    ] {
        let (server, client) = fixture(vec![]).await;
        let mut response = user();
        response["IdentityStoreId"] = json!("d-9999888877");
        Mock::given(header("x-amz-target", operation))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .with_priority(1)
            .mount(&server)
            .await;
        target(
            &server,
            "KiroControlPlaneQDeveloperService.CreateAssignment",
            json!({}),
        )
        .await;
        let error = client
            .mutate(
                &ScopeSelector::default(),
                "user@example.invalid",
                Mutation::Create(Plan::Pro),
                false,
            )
            .await
            .expect_err(
                "a user resolved from another identity store must not receive a subscription",
            );
        assert_eq!(error.code, "upstream_protocol_error");
        assert_eq!(error.write_outcome, WriteOutcome::NotSubmitted);
        assert!(write_requests(&server.received_requests().await.unwrap()).is_empty());
    }
}

#[tokio::test]
async fn review_user_list_cannot_mix_another_identity_stores_users() {
    let (server, client) = fixture(vec![]).await;
    let mut foreign = user();
    foreign["IdentityStoreId"] = json!("d-9999888877");
    Mock::given(header("x-amz-target", "AWSIdentityStore.ListUsers"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"Users":[foreign]})))
        .with_priority(1)
        .mount(&server)
        .await;
    assert_eq!(
        client
            .users(&ScopeSelector::default())
            .await
            .expect_err("foreign directory user must not appear in the selected scope")
            .code,
        "upstream_protocol_error"
    );
}

#[tokio::test]
async fn review_unicode_usernames_follow_the_aws_character_limit() {
    let (server, client) = fixture(vec![]).await;
    let name = "用".repeat(128);
    Mock::given(header("x-amz-target", "AWSIdentityStore.GetUserId"))
        .and(body_json(json!({"IdentityStoreId":"d-1111222233","AlternateIdentifier":{"UniqueAttribute":{"AttributePath":"userName","AttributeValue":name}}})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"UserId":USER,"IdentityStoreId":"d-1111222233"})))
        .with_priority(1).mount(&server).await;
    let result = client
        .mutate(
            &ScopeSelector::default(),
            &name,
            Mutation::Create(Plan::Pro),
            true,
        )
        .await
        .expect("128 Unicode characters are a valid Identity Center username length");
    assert_eq!(result.user_id, USER);
    assert_eq!(result.status, "preview");
    assert!(write_requests(&server.received_requests().await.unwrap()).is_empty());
}

#[tokio::test]
async fn review_invalid_user_selectors_fail_before_any_aws_request() {
    for name in ["has space".to_owned(), "\ninvalid".into(), "x".repeat(129)] {
        let (server, client) = fixture(vec![]).await;
        let error = client
            .mutate(
                &ScopeSelector::default(),
                &name,
                Mutation::Create(Plan::Pro),
                false,
            )
            .await
            .unwrap_err();
        assert_eq!(error.http_status, 400);
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "invalid selector must not consume AWS discovery calls"
        );
        let error = client
            .user_subscription(&ScopeSelector::default(), &name)
            .await
            .unwrap_err();
        assert_eq!(error.http_status, 400);
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn review_overlapping_plan_updates_for_the_same_user_do_not_race() {
    let (server, client) = fixture(vec![direct("KIRO_ENTERPRISE_PRO_PLUS")]).await;
    Mock::given(header(
        "x-amz-target",
        "KiroControlPlaneQDeveloperService.UpdateAssignment",
    ))
    .respond_with(
        ResponseTemplate::new(200)
            .set_body_json(json!({}))
            .set_delay(Duration::from_millis(200)),
    )
    .mount(&server)
    .await;
    let client = Arc::new(client);
    let second_client = EnterpriseClient {
        transport: AwsTransport::new(credentials(), "us-east-1".into(), false)
            .unwrap()
            .with_test_endpoint(server.uri()),
        mutations: Arc::new(MutationCoordinator::default()),
    }
    .with_mutation_coordinator(client.mutations.clone());
    let first_client = client.clone();
    let first = tokio::spawn(async move {
        first_client
            .mutate(
                &ScopeSelector::default(),
                USER,
                Mutation::Upgrade(Plan::Power),
                false,
            )
            .await
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if !write_requests(&server.received_requests().await.unwrap()).is_empty() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "first mutation did not reach AWS"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Without exclusion this is accepted against the old Pro+ snapshot, but
    // arrives after the Power update and becomes an unintended downgrade.
    let second = second_client
        .mutate(
            &ScopeSelector::default(),
            "user@example.invalid",
            Mutation::Upgrade(Plan::ProMax),
            false,
        )
        .await;
    let first = first.await.unwrap().unwrap();
    assert_eq!(first.requested_plan, Some(Plan::Power));
    assert_eq!(
        second
            .expect_err("aliases of the same user must share write exclusion")
            .http_status,
        409
    );
    assert_eq!(
        write_requests(&server.received_requests().await.unwrap()).len(),
        1
    );
    // Finishing the write releases the guard; previews can subsequently run.
    assert!(client
        .mutate(
            &ScopeSelector::default(),
            USER,
            Mutation::Upgrade(Plan::Power),
            true
        )
        .await
        .is_ok());
}

#[tokio::test]
async fn review_foreign_group_memberships_do_not_become_inherited_entitlements() {
    for mismatch in ["store", "member"] {
        let (server, client) = fixture(vec![claim(
            json!({"group":"group-1"}),
            "KIRO_ENTERPRISE_POWER",
        )])
        .await;
        let membership = json!({
            "GroupId":"group-1", "IdentityStoreId":if mismatch == "store" {"d-9999888877"} else {"d-1111222233"},
            "MemberId":{"UserId":if mismatch == "member" {"22222222-2222-4222-8222-222222222222"} else {USER}}
        });
        Mock::given(header(
            "x-amz-target",
            "AWSIdentityStore.ListGroupMembershipsForMember",
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"GroupMemberships":[membership]})),
        )
        .with_priority(1)
        .mount(&server)
        .await;
        assert_eq!(
            client
                .user_subscription(&ScopeSelector::default(), USER)
                .await
                .unwrap_err()
                .code,
            "upstream_protocol_error"
        );
    }
}

#[tokio::test]
async fn review_malformed_user_ids_cannot_be_returned_as_selectable_accounts() {
    let (server, client) = fixture(vec![]).await;
    let mut malformed = user();
    malformed["UserId"] = json!("ordinary-username");
    Mock::given(header("x-amz-target", "AWSIdentityStore.ListUsers"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"Users":[malformed]})))
        .with_priority(1)
        .mount(&server)
        .await;
    assert_eq!(
        client
            .users(&ScopeSelector::default())
            .await
            .unwrap_err()
            .code,
        "upstream_protocol_error"
    );
}

#[tokio::test]
async fn review_mutation_guards_do_not_block_other_users_accounts_or_regions() {
    let (_server, client) = fixture(vec![]).await;
    let scope = client.scope(&ScopeSelector::default()).await.unwrap();
    let coordinator = MutationCoordinator::default();
    let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let held = coordinator.try_begin(&scope, id).unwrap();
    assert!(coordinator
        .try_begin(&scope, &id.to_ascii_uppercase())
        .is_err());
    let other_user = coordinator.try_begin(&scope, USER).unwrap();
    let mut other = scope.clone();
    other.account_id = "000000000000".into();
    let other_account = coordinator.try_begin(&other, id).unwrap();
    other = scope.clone();
    other.kiro_region = "us-west-2".into();
    let other_region = coordinator.try_begin(&other, id).unwrap();
    drop((held, other_user, other_account, other_region));
    assert!(coordinator.try_begin(&scope, id).is_ok());
}

#[tokio::test]
async fn review_uuid_case_does_not_change_the_resolved_user_or_plan_binding() {
    let (server, client) = fixture(vec![]).await;
    let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let mut response = user();
    response["UserId"] = json!(id.to_ascii_uppercase());
    Mock::given(header("x-amz-target", "AWSIdentityStore.DescribeUser"))
        .and(body_json(
            json!({"IdentityStoreId":"d-1111222233","UserId":id}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .with_priority(1)
        .mount(&server)
        .await;
    let result = client
        .mutate(
            &ScopeSelector::default(),
            &id.to_ascii_uppercase(),
            Mutation::Create(Plan::Pro),
            true,
        )
        .await
        .unwrap();
    assert_eq!(result.user_id, id);
    let requests = server.received_requests().await.unwrap();
    assert!(!requests.iter().any(|request| request
        .headers
        .get("x-amz-target")
        .is_some_and(|value| value == "AWSIdentityStore.GetUserId")));
    assert!(write_requests(&requests).is_empty());
}

#[tokio::test]
async fn review_many_valid_pages_cannot_accumulate_unbounded_response_memory() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (server, client) = fixture(vec![]).await;
    let pages = Arc::new(AtomicUsize::new(0));
    let page_count = pages.clone();
    Mock::given(header(
        "x-amz-target",
        "KiroControlPlaneService.ListProfiles",
    ))
    .respond_with(move |_: &wiremock::Request| {
        let number = page_count.fetch_add(1, Ordering::SeqCst) + 1;
        let mut item = profile();
        item["arn"] = json!(format!("{PROFILE}-{number}"));
        item["metadata"] = json!("x".repeat(1024 * 1024));
        let mut response = json!({"profiles":[item]});
        if number < 20 {
            response["nextToken"] = json!(format!("page-{number}"));
        }
        ResponseTemplate::new(200).set_body_json(response)
    })
    .with_priority(1)
    .mount(&server)
    .await;
    let error = client
        .profiles()
        .await
        .expect_err("individually bounded pages must also have a total collection limit");
    assert!(error.message.contains("aggregate byte limit"));
    assert!(pages.load(Ordering::SeqCst) < 20);
}

#[tokio::test]
async fn review_truncated_write_response_preserves_the_aws_request_id() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let responder = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let read = socket.read(&mut buffer).await.unwrap();
            assert!(
                read > 0 && request.len() + read <= 8192,
                "fixture HTTP request must be complete and bounded"
            );
            request.extend_from_slice(&buffer[..read]);
            if let Some(end) = request.windows(4).position(|value| value == b"\r\n\r\n") {
                let headers = std::str::from_utf8(&request[..end]).unwrap();
                let length = headers
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .unwrap()
                    .1
                    .trim()
                    .parse::<usize>()
                    .unwrap();
                if request.len() >= end + 4 + length {
                    break;
                }
            }
        }
        socket.write_all(b"HTTP/1.1 200 OK\r\nx-amzn-requestid: review-truncated-write\r\nContent-Length: 512\r\nConnection: close\r\n\r\n{").await.unwrap();
        socket.shutdown().await.unwrap();
    });
    let transport = AwsTransport::new(credentials(), "us-east-1".into(), false)
        .unwrap()
        .with_test_endpoint(format!("http://{address}"));
    let error = transport.call(Service::Assignments, "us-east-1", "CreateAssignment", json!({"principalId":USER,"principalType":"USER","subscriptionType":Plan::Pro.aws_type()})).await
        .err().expect("a truncated reply cannot prove successful provisioning");
    responder.await.unwrap();
    assert_eq!(error.write_outcome, WriteOutcome::Unknown);
    assert_eq!(error.upstream_status, Some(200));
    assert_eq!(
        error.upstream_request_id.as_deref(),
        Some("review-truncated-write")
    );
}
