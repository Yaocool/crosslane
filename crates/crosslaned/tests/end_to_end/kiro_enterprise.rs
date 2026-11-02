use super::admin_rest::{AdminFixture, READ_TOKEN, WRITE_TOKEN};
use reqwest::Method;
use serde_json::{json, Value};

#[tokio::test]
async fn kiro_enterprise_routes_keep_admin_auth_roles_and_aws_credentials_separate() {
    let fixture = AdminFixture::start(false).await;
    let prefix = "/kiro/enterprise";
    assert_eq!(
        fixture
            .request(Method::GET, &format!("{prefix}/plans"), None)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let response = fixture
        .request(Method::GET, &format!("{prefix}/plans"), Some(READ_TOKEN))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["data"].as_array().unwrap().len(), 4);
    assert_eq!(body["data"][1]["plan"], "pro-plus");
    let user_arn = "arn:aws:identitystore:::user/11111111-1111-4111-8111-111111111111";
    let user_arn_path: String = url::form_urlencoded::byte_serialize(user_arn.as_bytes()).collect();
    let response = fixture
        .request(
            Method::GET,
            &format!("{prefix}/users/{user_arn_path}/subscription"),
            Some(READ_TOKEN),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        503,
        "encoded user ARNs must reach the management handler without changing route matching"
    );

    for path in [
        "/profiles",
        "/scope",
        "/users",
        "/subscriptions",
        "/users/fixture/subscription",
    ] {
        let response = fixture
            .request(Method::GET, &format!("{prefix}{path}"), Some(READ_TOKEN))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            503,
            "missing daemon credentials must fail before any upstream request"
        );
    }
    for (method, path) in [
        (Method::POST, "/users/fixture/subscription"),
        (Method::POST, "/users/fixture/subscription/upgrade"),
        (Method::POST, "/users/fixture/subscription/downgrade"),
        (Method::DELETE, "/users/fixture/subscription"),
    ] {
        assert_eq!(
            fixture
                .request(method, &format!("{prefix}{path}"), Some(READ_TOKEN))
                .json(&json!({"plan":"pro"}))
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
    }
    let access = "AKIA000000000EXAMPLE";
    let secret = "fixture-enterprise-private-secret";
    let session = "fixture-enterprise-private-session-token";
    for parameter in ["profile_arn", "application_arn", "identity_center_region"] {
        let response = fixture
            .request(
                Method::GET,
                &format!("{prefix}/profiles?{parameter}="),
                Some(WRITE_TOKEN),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            400,
            "discovery must not silently ignore scope selectors"
        );
    }
    for body in [
        json!({"plan":"pro","assign_application":true}),
        json!({"plan":"unknown"}),
    ] {
        let response = fixture
            .request(
                Method::POST,
                &format!("{prefix}/users/fixture/subscription"),
                Some(WRITE_TOKEN),
            )
            .header("x-kiro-aws-access-key-id", access)
            .header("x-kiro-aws-secret-access-key", secret)
            .header("x-kiro-aws-session-token", session)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        let response = response.text().await.unwrap();
        for secret in [access, secret, session] {
            assert!(!response.contains(secret));
        }
    }
    let response = fixture
        .request(
            Method::GET,
            &format!("{prefix}/profiles?region=us-east-1.evil.invalid"),
            Some(WRITE_TOKEN),
        )
        .header("x-kiro-aws-access-key-id", access)
        .header("x-kiro-aws-secret-access-key", secret)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    let response = fixture
        .request(
            Method::GET,
            &format!("{prefix}/profiles?region=eu-west-1&legacy=true"),
            Some(WRITE_TOKEN),
        )
        .header("x-kiro-aws-access-key-id", access)
        .header("x-kiro-aws-secret-access-key", secret)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    let logs = tokio::fs::read_to_string(fixture.daemon.home().join("rest-test.log"))
        .await
        .unwrap();
    for secret in [access, secret, session] {
        assert!(
            !logs.contains(secret),
            "AWS credential must not appear in request logs"
        );
    }
    fixture.daemon.stop().await;
}
