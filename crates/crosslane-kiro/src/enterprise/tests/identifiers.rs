use super::*;

const STORE: &str = "d-1111222233";
const STORE_ARN: &str = "arn:aws:identitystore::210987654321:identitystore/d-1111222233";
const GROUP: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";

fn user_arn(id: &str) -> String {
    format!("arn:aws:identitystore:::user/{id}")
}
fn group_arn(id: &str) -> String {
    format!("arn:aws:identitystore:::group/{id}")
}

#[tokio::test]
async fn identifiers_user_arns_are_resolved_as_ids_instead_of_usernames() {
    let (server, client) = fixture(vec![]).await;
    let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let mut response = user();
    response["UserId"] = json!(id);
    Mock::given(header("x-amz-target", "AWSIdentityStore.DescribeUser"))
        .and(body_json(json!({"IdentityStoreId":STORE,"UserId":id})))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .with_priority(1)
        .mount(&server)
        .await;
    let result = client
        .mutate(
            &ScopeSelector::default(),
            &user_arn(id),
            Mutation::Create(Plan::Pro),
            true,
        )
        .await
        .unwrap();
    assert_eq!(
        result.user_id, id,
        "an ARN must not be looked up as an unrelated username"
    );
    let requests = server.received_requests().await.unwrap();
    assert!(!requests.iter().any(|request| request
        .headers
        .get("x-amz-target")
        .is_some_and(|value| value == "AWSIdentityStore.GetUserId")));
    assert!(write_requests(&requests).is_empty());
}

#[tokio::test]
async fn identifiers_user_and_store_arns_join_existing_subscription_claims() {
    let (server, client) = fixture(vec![claim(
        json!({"user":user_arn(USER)}),
        "KIRO_ENTERPRISE_PRO_PLUS",
    )])
    .await;
    let mut response = user();
    response["UserId"] = json!(user_arn(USER));
    response["IdentityStoreId"] = json!(STORE_ARN);
    Mock::given(header("x-amz-target", "AWSIdentityStore.ListUsers"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"Users":[response]})))
        .with_priority(1)
        .mount(&server)
        .await;
    let result = client.users(&ScopeSelector::default()).await.unwrap();
    assert_eq!(result.users[0].user_id, USER);
    assert_eq!(
        result.users[0].direct_subscriptions[0].plan,
        Some(Plan::ProPlus)
    );
}

#[tokio::test]
async fn identifiers_returned_user_arns_are_canonicalized_before_provisioning() {
    let (server, client) = fixture(vec![]).await;
    Mock::given(header("x-amz-target", "AWSIdentityStore.GetUserId"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"UserId":user_arn(USER),"IdentityStoreId":STORE_ARN})),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    let mut response = user();
    response["UserId"] = json!(user_arn(USER));
    response["IdentityStoreId"] = json!(STORE_ARN);
    Mock::given(header("x-amz-target", "AWSIdentityStore.DescribeUser"))
        .and(body_json(json!({"IdentityStoreId":STORE,"UserId":USER})))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(header(
        "x-amz-target",
        "KiroControlPlaneQDeveloperService.CreateAssignment",
    ))
    .and(body_json(
        json!({"principalId":USER,"principalType":"USER","subscriptionType":Plan::Pro.aws_type()}),
    ))
    .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
    .expect(1)
    .mount(&server)
    .await;
    let result = client
        .mutate(
            &ScopeSelector::default(),
            "user@example.invalid",
            Mutation::Create(Plan::Pro),
            false,
        )
        .await
        .unwrap();
    assert_eq!(result.user_id, USER);
    assert!(!result.application_assignment_requested);
    assert!(serde_json::to_value(&result.scope)
        .unwrap()
        .get("identity_store_account")
        .is_none());
}

#[tokio::test]
async fn identifiers_membership_user_and_group_arns_do_not_hide_inherited_plans() {
    for claim_id in [GROUP.to_owned(), group_arn(GROUP)] {
        let (server, client) = fixture(vec![claim(
            json!({"group":claim_id}),
            "KIRO_ENTERPRISE_POWER",
        )])
        .await;
        Mock::given(header("x-amz-target", "AWSIdentityStore.ListGroupMembershipsForMember"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"GroupMemberships":[{
                "IdentityStoreId":STORE_ARN,"GroupId":group_arn(&GROUP.to_ascii_uppercase()),"MemberId":{"UserId":user_arn(USER)}
            }]}))).with_priority(1).mount(&server).await;
        let result = client
            .user_subscription(&ScopeSelector::default(), USER)
            .await
            .unwrap();
        assert_eq!(result.group_subscriptions.len(), 1);
        assert_eq!(result.group_subscriptions[0].principal_id, GROUP);
        assert_eq!(result.group_subscriptions[0].plan, Some(Plan::Power));
    }
}

#[tokio::test]
async fn identifiers_malformed_or_wrong_resource_arns_are_not_treated_as_usernames() {
    for id in [
        group_arn(USER),
        "arn:aws:iam::123456789012:user/fixture".into(),
        format!("arn:aws-us-gov:identitystore:::user/{USER}"),
        format!("arn:aws:identitystore:us-east-1::user/{USER}"),
        user_arn("not-a-user-id"),
    ] {
        let (server, client) = fixture(vec![]).await;
        let error = client
            .mutate(
                &ScopeSelector::default(),
                &id,
                Mutation::Create(Plan::Pro),
                true,
            )
            .await
            .unwrap_err();
        assert_eq!(error.http_status, 400);
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn identifiers_normalization_preserves_foreign_store_and_member_rejection() {
    for wrong in ["store", "store_owner", "member"] {
        let (server, client) =
            fixture(vec![claim(json!({"group":GROUP}), "KIRO_ENTERPRISE_POWER")]).await;
        let store = if wrong == "store" {
            STORE_ARN.replace(STORE, "d-9999888877")
        } else if wrong == "store_owner" {
            STORE_ARN.replace("210987654321", "999999999999")
        } else {
            STORE_ARN.into()
        };
        let member = if wrong == "member" {
            user_arn("22222222-2222-4222-8222-222222222222")
        } else {
            user_arn(USER)
        };
        Mock::given(header(
            "x-amz-target",
            "AWSIdentityStore.ListGroupMembershipsForMember",
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"GroupMemberships":[{
                "IdentityStoreId":store,"GroupId":group_arn(GROUP),"MemberId":{"UserId":member}
            }]})),
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
