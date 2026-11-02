use super::*;
use wiremock::matchers::{body_json, body_string, header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zeroize::Zeroizing;

mod identifiers;
mod review;

const USER: &str = "11111111-1111-4111-8111-111111111111";
const PROFILE: &str = "arn:aws:codewhisperer:us-east-1:123456789012:profile/fixture";
const INSTANCE: &str = "arn:aws:sso:::instance/ssoins-1111222233334444";
const APPLICATION: &str =
    "arn:aws:sso::123456789012:application/ssoins-1111222233334444/apl-1111222233334444";
const ACCESS: &str = "AKIA000000000EXAMPLE";
const SECRET: &str = "fixture-private-aws-secret-access-key";
const TOKEN: &str = "fixture-private-aws-session-token";

fn credentials() -> AwsCredentials {
    AwsCredentials::new(ACCESS.into(), SECRET.into(), Some(TOKEN.into())).unwrap()
}

fn profile() -> Value {
    json!({"arn":PROFILE,"identityDetails":{"ssoIdentityDetails":{"instanceArn":INSTANCE,"ssoRegion":"us-west-2","oidcClientId":APPLICATION}},"applicationProperties":[{"applicationArn":APPLICATION}]})
}

fn user() -> Value {
    json!({"UserId":USER,"IdentityStoreId":"d-1111222233","UserName":"user@example.invalid","DisplayName":"Fixture User","Emails":[{"Value":"user@example.invalid","Primary":true}],"PrivateField":SECRET})
}
fn claim(principal: Value, plan: &str) -> Value {
    json!({"principal":principal,"type":{"amazonQ":plan},"status":"ACTIVE"})
}
fn direct(plan: &str) -> Value {
    claim(json!({"user":USER}), plan)
}

async fn target(server: &MockServer, target: &str, response: Value) {
    Mock::given(method("POST"))
        .and(header("x-amz-target", target))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .mount(server)
        .await;
}

async fn fixture(claims: Vec<Value>) -> (MockServer, EnterpriseClient) {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(body_string("Action=GetCallerIdentity&Version=2011-06-15"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<GetCallerIdentityResponse xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\"><GetCallerIdentityResult><Account>123456789012</Account></GetCallerIdentityResult></GetCallerIdentityResponse>"))
        .mount(&server).await;
    for prefix in ["KiroControlPlaneService", "AWSCodeWhispererService"] {
        target(
            &server,
            &format!("{prefix}.ListProfiles"),
            json!({"profiles":[profile()]}),
        )
        .await;
    }
    target(&server, "SWBExternalService.DescribeInstance", json!({"InstanceArn":INSTANCE,"IdentityStoreId":"d-1111222233","OwnerAccountId":"210987654321"})).await;
    target(
        &server,
        "SWBExternalService.DescribeApplication",
        json!({"ApplicationArn":APPLICATION,"InstanceArn":INSTANCE}),
    )
    .await;
    target(
        &server,
        "AWSZornControlPlaneService.ListApplicationClaims",
        json!({"claims":claims}),
    )
    .await;
    target(
        &server,
        "AWSIdentityStore.GetUserId",
        json!({"UserId":USER,"IdentityStoreId":"d-1111222233"}),
    )
    .await;
    target(&server, "AWSIdentityStore.DescribeUser", user()).await;
    target(
        &server,
        "AWSIdentityStore.ListUsers",
        json!({"Users":[user()]}),
    )
    .await;
    target(
        &server,
        "AWSIdentityStore.ListGroupMembershipsForMember",
        json!({"GroupMemberships":[]}),
    )
    .await;
    let client = EnterpriseClient {
        mutations: std::sync::Arc::new(MutationCoordinator::default()),
        transport: AwsTransport::new(credentials(), "us-east-1".into(), false)
            .unwrap()
            .with_test_endpoint(server.uri()),
    };
    (server, client)
}

fn write_requests(requests: &[wiremock::Request]) -> Vec<&wiremock::Request> {
    requests
        .iter()
        .filter(|request| {
            request
                .headers
                .get("x-amz-target")
                .is_some_and(|value| value.to_str().unwrap().contains("Assignment"))
        })
        .collect()
}

#[test]
fn sigv4_matches_the_official_aws_sdk_conformance_vector() {
    // https://github.com/boto/botocore/tree/develop/tests/unit/auth/aws4_testsuite/post-x-www-form-urlencoded
    let credentials = AwsCredentials {
        access_key_id: Zeroizing::new("AKIDEXAMPLE".into()),
        secret_access_key: Zeroizing::new("wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into()),
        session_token: None,
    };
    let headers = transport::sign(
        &credentials,
        "example.amazonaws.com",
        "service",
        "us-east-1",
        "application/x-www-form-urlencoded",
        None,
        b"Param1=value1",
        "20150830T123600Z",
    )
    .unwrap();
    assert_eq!(headers["authorization"], "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=ff11897932ad3f4e8b18135d722051e5ac45fc38421b1da7b9d196a0fe09473a");
    assert!(headers["authorization"].is_sensitive());
}

#[test]
fn temporary_credentials_are_signed_and_invalid_inputs_cannot_choose_hosts() {
    assert!(AwsCredentials::new("ASIA000000000EXAMPLE".into(), SECRET.into(), None).is_err());
    assert!(AwsCredentials::new(ACCESS.into(), "secret\nheader".into(), None).is_err());
    let headers = transport::sign(
        &credentials(),
        "management.us-east-1.kiro.dev",
        "q",
        "us-east-1",
        "application/x-amz-json-1.0",
        Some("KiroControlPlaneQDeveloperService.CreateAssignment"),
        b"{}",
        "20150830T123600Z",
    )
    .unwrap();
    assert_eq!(headers["x-amz-security-token"], TOKEN);
    assert!(headers["x-amz-security-token"].is_sensitive());
    assert!(headers["authorization"]
        .to_str()
        .unwrap()
        .contains("x-amz-security-token;x-amz-target"));
    assert!(!headers["authorization"].to_str().unwrap().contains(SECRET));
    for region in [
        "us-east-1.evil.invalid",
        "us-east-1/redirect",
        "cn-north-1",
        "us-gov-west-1",
        "US-east-1",
        "us--1",
    ] {
        assert!(EnterpriseClient::new(credentials(), region.into(), false).is_err());
    }
    assert!(EnterpriseClient::new(credentials(), "eu-west-1".into(), true).is_err());
}

#[tokio::test]
async fn activation_only_creates_the_subscription_and_never_assigns_the_application() {
    let (server, client) = fixture(vec![]).await;
    Mock::given(header("x-amz-target", "KiroControlPlaneQDeveloperService.CreateAssignment"))
        .and(body_json(json!({"principalId":USER,"principalType":"USER","subscriptionType":"Q_DEVELOPER_STANDALONE_PRO_PLUS"})))
        .respond_with(ResponseTemplate::new(200).insert_header("x-amzn-requestid", "fixture-write-request").set_body_json(json!({})))
        .expect(1).mount(&server).await;
    let result = client
        .mutate(
            &ScopeSelector::default(),
            "user@example.invalid",
            Mutation::Create(Plan::ProPlus),
            false,
        )
        .await
        .unwrap();
    assert_eq!(result.status, "api_accepted");
    assert_eq!(
        result.upstream_request_id.as_deref(),
        Some("fixture-write-request")
    );
    assert!(!result.application_assignment_requested);
    assert_eq!(result.scope.identity_center_region, "us-west-2");
    let requests = server.received_requests().await.unwrap();
    let writes = write_requests(&requests);
    assert_eq!(writes.len(), 1);
    assert!(writes[0].headers["authorization"]
        .to_str()
        .unwrap()
        .contains("/us-east-1/q/aws4_request"));
    for request in &requests {
        let operation = request
            .headers
            .get("x-amz-target")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        assert!(!operation.contains("ApplicationAssignment"));
        assert!(!operation.ends_with("CreateUser") && !operation.ends_with("DeleteUser"));
        if operation.starts_with("AWSZorn") {
            assert!(request.headers["authorization"]
                .to_str()
                .unwrap()
                .contains("/us-west-2/user-subscriptions/aws4_request"));
            assert_eq!(
                request.body_json::<Value>().unwrap()["subscriptionRegion"],
                "us-east-1"
            );
        }
    }
}

#[tokio::test]
async fn upgrade_and_downgrade_use_update_assignment_with_the_verified_direction() {
    for (current, mutation, expected) in [
        (
            "KIRO_ENTERPRISE_PRO",
            Mutation::Upgrade(Plan::ProMax),
            "immediate",
        ),
        (
            "Q_DEVELOPER_STANDALONE_POWER",
            Mutation::Downgrade(Plan::ProPlus),
            "next_month",
        ),
    ] {
        let (server, client) = fixture(vec![direct(current)]).await;
        Mock::given(header("x-amz-target", "KiroControlPlaneQDeveloperService.UpdateAssignment"))
            .and(body_json(json!({"principalId":USER,"principalType":"USER","subscriptionType":mutation.plan().unwrap().aws_type()})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({}))).expect(1).mount(&server).await;
        let result = client
            .mutate(&ScopeSelector::default(), USER, mutation, false)
            .await
            .unwrap();
        assert_eq!(result.expected_effect, expected);
        assert_eq!(result.operation, "UpdateAssignment");
        assert!(!result.application_assignment_requested);
        assert_eq!(
            write_requests(&server.received_requests().await.unwrap()).len(),
            1
        );
    }
}

#[tokio::test]
async fn invalid_directions_and_duplicate_activation_submit_no_write() {
    for mutation in [
        Mutation::Upgrade(Plan::Pro),
        Mutation::Downgrade(Plan::Power),
        Mutation::Create(Plan::ProMax),
    ] {
        let (server, client) = fixture(vec![direct("KIRO_ENTERPRISE_PRO_PLUS")]).await;
        let error = client
            .mutate(&ScopeSelector::default(), USER, mutation, false)
            .await
            .unwrap_err();
        assert_eq!(error.http_status, 409);
        assert_eq!(error.write_outcome, WriteOutcome::NotSubmitted);
        assert!(write_requests(&server.received_requests().await.unwrap()).is_empty());
    }
}

#[tokio::test]
async fn cancellation_preserves_identity_and_group_entitlements_for_both_modes() {
    for mode in [DeletionMode::Immediate, DeletionMode::EndOfPeriod] {
        let (server, client) = fixture(vec![
            direct("KIRO_ENTERPRISE_PRO"),
            claim(json!({"group":"group-1"}), "KIRO_ENTERPRISE_POWER"),
        ])
        .await;
        Mock::given(header(
            "x-amz-target",
            "KiroControlPlaneQDeveloperService.DeleteAssignment",
        ))
        .and(body_json(
            json!({"principalId":USER,"principalType":"USER","deletionMode":mode.aws_type()}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;
        let result = client
            .mutate(
                &ScopeSelector::default(),
                USER,
                Mutation::Cancel(mode),
                false,
            )
            .await
            .unwrap();
        assert_eq!(result.deletion_mode, Some(mode));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(write_requests(&requests).len(), 1);
        assert!(requests.iter().all(|request| !request
            .headers
            .get("x-amz-target")
            .is_some_and(|value| value.to_str().unwrap().contains("ApplicationAssignment"))));
    }
}

#[tokio::test]
async fn previews_and_legacy_cancellation_never_guess_an_unsupported_payload() {
    let (server, mut client) = fixture(vec![direct("KIRO_ENTERPRISE_PRO")]).await;
    let result = client
        .mutate(
            &ScopeSelector::default(),
            USER,
            Mutation::Upgrade(Plan::Power),
            true,
        )
        .await
        .unwrap();
    assert_eq!(result.status, "preview");
    assert!(write_requests(&server.received_requests().await.unwrap()).is_empty());
    client.transport.legacy = true;
    let before = server.received_requests().await.unwrap().len();
    let error = client
        .mutate(
            &ScopeSelector::default(),
            USER,
            Mutation::Cancel(DeletionMode::EndOfPeriod),
            false,
        )
        .await
        .unwrap_err();
    assert_eq!(error.http_status, 400);
    assert_eq!(server.received_requests().await.unwrap().len(), before);
    Mock::given(header(
        "x-amz-target",
        "AmazonQDeveloperService.DeleteAssignment",
    ))
    .and(body_json(
        json!({"principalId":USER,"principalType":"USER"}),
    ))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
    .expect(1)
    .mount(&server)
    .await;
    client
        .mutate(
            &ScopeSelector::default(),
            USER,
            Mutation::Cancel(DeletionMode::Immediate),
            false,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn user_listing_joins_direct_plans_without_exposing_provider_private_fields() {
    let (server, client) = fixture(vec![
        direct("KIRO_ENTERPRISE_PRO"),
        claim(json!({"group":"group-1"}), "KIRO_ENTERPRISE_POWER"),
    ])
    .await;
    let result = client.users(&ScopeSelector::default()).await.unwrap();
    assert_eq!(result.users.len(), 1);
    assert_eq!(
        result.users[0].direct_subscriptions[0].plan,
        Some(Plan::Pro)
    );
    assert_eq!(result.group_subscriptions.len(), 1);
    assert!(!serde_json::to_string(&result).unwrap().contains(SECRET));
    let request = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .find(|request| {
            request
                .headers
                .get("x-amz-target")
                .is_some_and(|value| value == "AWSIdentityStore.ListUsers")
        })
        .unwrap();
    assert!(request.headers["authorization"]
        .to_str()
        .unwrap()
        .contains("/us-west-2/identitystore/aws4_request"));
}

#[tokio::test]
async fn inherited_subscriptions_are_reported_and_cannot_be_updated_as_user_assignments() {
    let (server, client) = fixture(vec![
        claim(json!({"group":"group-1"}), "KIRO_ENTERPRISE_POWER"),
        claim(json!({"group":"group-other"}), "KIRO_ENTERPRISE_PRO"),
    ])
    .await;
    Mock::given(header(
        "x-amz-target",
        "AWSIdentityStore.ListGroupMembershipsForMember",
    ))
    .respond_with(ResponseTemplate::new(200).set_body_json(
        json!({"GroupMemberships":[{"GroupId":"group-1","IdentityStoreId":"d-1111222233"}]}),
    ))
    .with_priority(1)
    .mount(&server)
    .await;
    let result = client
        .user_subscription(&ScopeSelector::default(), USER)
        .await
        .unwrap();
    assert!(result.user.direct_subscriptions.is_empty());
    assert_eq!(result.group_subscriptions.len(), 1);
    assert_eq!(result.group_subscriptions[0].plan, Some(Plan::Power));
    let error = client
        .mutate(
            &ScopeSelector::default(),
            USER,
            Mutation::Upgrade(Plan::Power),
            false,
        )
        .await
        .unwrap_err();
    assert_eq!(error.http_status, 409);
    assert!(write_requests(&server.received_requests().await.unwrap()).is_empty());
}

#[tokio::test]
async fn a_changed_profile_account_or_application_instance_cannot_receive_a_write() {
    for mismatch in ["account", "region", "application"] {
        let (server, client) = fixture(vec![]).await;
        let (operation, response) = if mismatch == "application" {
            (
                "SWBExternalService.DescribeApplication",
                json!({"InstanceArn":"another-instance","ApplicationArn":APPLICATION}),
            )
        } else {
            let mut value = profile();
            value["arn"] = json!(if mismatch == "account" {
                PROFILE.replace("123456789012", "000000000000")
            } else {
                PROFILE.replace("us-east-1", "eu-west-1")
            });
            (
                "KiroControlPlaneService.ListProfiles",
                json!({"profiles":[value]}),
            )
        };
        Mock::given(header("x-amz-target", operation))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .with_priority(1)
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
            .unwrap_err();
        assert_eq!(error.http_status, 502);
        assert!(write_requests(&server.received_requests().await.unwrap()).is_empty());
    }
}

#[tokio::test]
async fn multiple_profiles_require_an_explicit_selection() {
    let (server, client) = fixture(vec![]).await;
    let mut second = profile();
    second["arn"] = json!(format!("{PROFILE}-second"));
    Mock::given(header(
        "x-amz-target",
        "KiroControlPlaneService.ListProfiles",
    ))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({"profiles":[profile(),second]})))
    .with_priority(1)
    .mount(&server)
    .await;
    assert_eq!(
        client
            .scope(&ScopeSelector::default())
            .await
            .unwrap_err()
            .http_status,
        409
    );
    let explicit = ScopeSelector {
        profile_arn: Some(PROFILE.into()),
        ..Default::default()
    };
    assert_eq!(client.scope(&explicit).await.unwrap().profile_arn, PROFILE);
}

#[tokio::test]
async fn pagination_rejects_repeated_tokens_and_never_returns_a_partial_result() {
    let (server, client) = fixture(vec![]).await;
    Mock::given(header(
        "x-amz-target",
        "AWSZornControlPlaneService.ListApplicationClaims",
    ))
    .respond_with(
        ResponseTemplate::new(200).set_body_json(
            json!({"claims":[direct("KIRO_ENTERPRISE_PRO")],"nextToken":"repeated"}),
        ),
    )
    .with_priority(1)
    .mount(&server)
    .await;
    let error = client
        .subscriptions(&ScopeSelector::default())
        .await
        .unwrap_err();
    assert_eq!(error.code, "upstream_protocol_error");
    assert!(error.message.contains("repeated"));
    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request
                .headers
                .get("x-amz-target")
                .is_some_and(|value| value == "AWSZornControlPlaneService.ListApplicationClaims"))
            .count(),
        2
    );
}

#[tokio::test]
async fn pagination_follows_each_token_and_preserves_pending_plan_metadata() {
    let (server, client) = fixture(vec![]).await;
    let mut first = direct("KIRO_ENTERPRISE_POWER");
    first["newType"] = json!({"amazonQ":"KIRO_ENTERPRISE_PRO_PLUS"});
    for (body, response) in [
        (
            json!({"applicationArn":APPLICATION,"subscriptionRegion":"us-east-1","maxResults":100}),
            json!({"claims":[first],"nextToken":"page-2"}),
        ),
        (
            json!({"applicationArn":APPLICATION,"subscriptionRegion":"us-east-1","maxResults":100,"nextToken":"page-2"}),
            json!({"claims":[claim(json!({"group":"group-1"}), "KIRO_ENTERPRISE_PRO")]}),
        ),
    ] {
        Mock::given(header(
            "x-amz-target",
            "AWSZornControlPlaneService.ListApplicationClaims",
        ))
        .and(body_json(body))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    }
    let (_, claims) = client
        .subscriptions(&ScopeSelector::default())
        .await
        .unwrap();
    assert_eq!(claims.len(), 2);
    assert_eq!(claims[0].plan, Some(Plan::Power));
    assert_eq!(claims[0].pending_plan, Some(Plan::ProPlus));
}

#[tokio::test]
async fn changed_user_identity_and_unknown_current_plan_prevent_updates() {
    let (server, client) = fixture(vec![]).await;
    Mock::given(header("x-amz-target", "AWSIdentityStore.GetUserId"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"UserId":"22222222-2222-4222-8222-222222222222","IdentityStoreId":"d-1111222233"})),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    assert_eq!(
        client
            .mutate(
                &ScopeSelector::default(),
                "user@example.invalid",
                Mutation::Create(Plan::Pro),
                false
            )
            .await
            .unwrap_err()
            .code,
        "upstream_protocol_error"
    );
    assert!(write_requests(&server.received_requests().await.unwrap()).is_empty());
    let (server, client) = fixture(vec![direct("KIRO_ENTERPRISE_FUTURE_PLAN")]).await;
    assert_eq!(
        client
            .mutate(
                &ScopeSelector::default(),
                USER,
                Mutation::Upgrade(Plan::Power),
                false
            )
            .await
            .unwrap_err()
            .http_status,
        409
    );
    assert!(write_requests(&server.received_requests().await.unwrap()).is_empty());
}

#[tokio::test]
async fn aws_errors_never_echo_credentials_and_failed_writes_are_not_retried() {
    let (server, client) = fixture(vec![]).await;
    Mock::given(header("x-amz-target", "KiroControlPlaneQDeveloperService.CreateAssignment"))
        .respond_with(ResponseTemplate::new(403).insert_header("x-amzn-requestid", ACCESS)
            .set_body_json(json!({"__type":"com.amazon.kiro.controlplane#AccessDeniedException","message":format!("{ACCESS} {SECRET} {TOKEN}")})))
        .expect(1).mount(&server).await;
    let error = client
        .mutate(
            &ScopeSelector::default(),
            USER,
            Mutation::Create(Plan::Pro),
            false,
        )
        .await
        .unwrap_err();
    assert_eq!(error.write_outcome, WriteOutcome::Rejected);
    assert_eq!(error.code, "aws_access_denied");
    assert_eq!(
        error.upstream_code.as_deref(),
        Some("AccessDeniedException")
    );
    let serialized = serde_json::to_string(&error).unwrap();
    for secret in [ACCESS, SECRET, TOKEN] {
        assert!(!serialized.contains(secret));
    }
    assert_eq!(
        write_requests(&server.received_requests().await.unwrap()).len(),
        1
    );
}

#[tokio::test]
async fn uncertain_write_responses_are_reported_without_automatic_replay_or_redirect() {
    for response in [
        ResponseTemplate::new(200).set_body_string("not JSON"),
        ResponseTemplate::new(503).set_body_json(json!({"__type":"InternalServerException"})),
        ResponseTemplate::new(302)
            .insert_header("location", "https://example.invalid/credential-capture"),
    ] {
        let (server, client) = fixture(vec![]).await;
        Mock::given(header(
            "x-amz-target",
            "KiroControlPlaneQDeveloperService.CreateAssignment",
        ))
        .respond_with(response)
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
            .unwrap_err();
        assert_eq!(error.http_status, 502);
        assert_eq!(error.write_outcome, WriteOutcome::Unknown);
        assert_eq!(
            write_requests(&server.received_requests().await.unwrap()).len(),
            1
        );
    }
}

#[tokio::test]
async fn no_partial_listing_is_returned_when_the_safety_bound_is_exceeded() {
    let (server, client) = fixture(vec![]).await;
    Mock::given(header(
        "x-amz-target",
        "KiroControlPlaneService.ListProfiles",
    ))
    .respond_with(
        ResponseTemplate::new(200).set_body_json(json!({"profiles":vec![json!({});MAX_ITEMS+1]})),
    )
    .with_priority(1)
    .mount(&server)
    .await;
    assert!(client
        .profiles()
        .await
        .unwrap_err()
        .message
        .contains("item limit"));
}

#[test]
fn user_ids_include_the_identity_store_prefix_but_do_not_capture_ordinary_names() {
    assert!(is_user_id(USER));
    assert!(is_user_id(&format!("1234567890-{USER}")));
    assert!(!is_user_id("user@example.invalid"));
    // A username consisting of hexadecimal characters is still a username.
    assert!(!is_user_id("11111111111141118111111111111111"));
}

#[tokio::test]
async fn unrecognized_error_codes_do_not_become_an_output_channel_for_credentials() {
    let (server, client) = fixture(vec![]).await;
    Mock::given(header(
        "x-amz-target",
        "KiroControlPlaneQDeveloperService.CreateAssignment",
    ))
    .respond_with(ResponseTemplate::new(400).set_body_json(json!({"code":SECRET})))
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
        .unwrap_err();
    assert_eq!(
        error.upstream_code.as_deref(),
        Some("UnrecognizedUpstreamError")
    );
    assert!(!serde_json::to_string(&error).unwrap().contains(SECRET));
}

#[tokio::test]
async fn sts_errors_preserve_only_the_known_error_code() {
    let (server, client) = fixture(vec![]).await;
    Mock::given(body_string("Action=GetCallerIdentity&Version=2011-06-15"))
        .respond_with(ResponseTemplate::new(403).set_body_string(format!(
            "<ErrorResponse><Error><Code>ExpiredToken</Code><Message>{SECRET} {TOKEN}</Message></Error></ErrorResponse>"
        )))
        .with_priority(1).expect(1).mount(&server).await;
    let error = client.profiles().await.unwrap_err();
    assert_eq!(error.code, "aws_access_denied");
    assert_eq!(error.upstream_code.as_deref(), Some("ExpiredToken"));
    let output = serde_json::to_string(&error).unwrap();
    assert!(!output.contains(SECRET) && !output.contains(TOKEN));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn malformed_or_ambiguous_sts_xml_cannot_authorize_further_discovery() {
    let valid = "<GetCallerIdentityResponse><GetCallerIdentityResult><Account>123456789012</Account></GetCallerIdentityResult></GetCallerIdentityResponse>";
    for body in [
        format!("{valid}<ExtraRoot/>"),
        format!("<!DOCTYPE external SYSTEM 'https://example.invalid/identity'>{valid}"),
        valid.replace("123456789012", "not-an-account"),
    ] {
        let (server, client) = fixture(vec![]).await;
        Mock::given(body_string("Action=GetCallerIdentity&Version=2011-06-15"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .with_priority(1)
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            client.profiles().await.unwrap_err().code,
            "upstream_protocol_error"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}
