//! Synthetic cloud credentials only: no real account or provider request.

use crosslane_translate::{KiroPayload, RedactionSession};
use serde_json::{json, Value};

const MARKER_PREFIX: &str = "__CROSSLANE_SECRET_";

fn payload(input: Value) -> KiroPayload {
    serde_json::from_value(json!({
        "conversationState": {
            "chatTriggerType":"MANUAL", "conversationId":"cloud-fixture",
            "history":[{"assistantResponseMessage":{
                "content":"", "toolUses":[{
                    "toolUseId":"cloud-call", "name":"cloud_request", "input":input
                }]
            }}],
            "currentMessage":{"userInputMessage":{
                "content":"Check the cloud API response.",
                "modelId":"claude-sonnet-4", "origin":"AI_EDITOR"
            }}
        }
    }))
    .unwrap()
}

fn input(payload: &KiroPayload) -> &Value {
    &payload.conversation_state.history[0]
        .assistant_response_message
        .as_ref()
        .unwrap()
        .tool_uses[0]
        .input
}

#[test]
fn recognizable_cloud_access_ids_are_masked_even_without_field_labels() {
    let credentials = [
        format!("AKID{}", "A1".repeat(16)),
        format!("LTAI{}", "B2".repeat(8)),
        format!("LTAI{}", "C3".repeat(10)),
        format!("STS.{}", "D4".repeat(16)),
        format!("AKLT{}", "E5".repeat(12)),
        format!("AKIA{}", "F6".repeat(8)),
        format!("ASIA{}", "G7".repeat(8)),
        format!("AIza{}", "h8".repeat(17)),
    ];
    for credential in credentials {
        let session = RedactionSession::new();
        let original = format!("Fixture credential {credential}; continue the task.");
        let masked = session.redact_text(&original, &[]);
        assert!(!masked.contains(&credential), "cloud ID was not redacted");
        assert!(masked.contains(MARKER_PREFIX));
        assert!(masked.contains("continue the task"));
        assert_eq!(session.restore_text(&masked), original);
    }
}

#[test]
fn cloud_field_aliases_cover_opaque_secrets_and_restore_tool_values() {
    let mut fields = serde_json::Map::new();
    let aliases = [
        "SecretId",
        "SecretKey",
        "TENCENTCLOUD_SECRET_ID",
        "TENCENTCLOUD_SECRET_KEY",
        "AccessKeyId",
        "AccessKeySecret",
        "ALIBABA_CLOUD_ACCESS_KEY_ID",
        "ALIBABA_CLOUD_ACCESS_KEY_SECRET",
        "SecurityToken",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "HUAWEICLOUD_SDK_AK",
        "HUAWEICLOUD_SDK_SK",
        "BAIDU_CLOUD_AK",
        "BAIDU_CLOUD_SK",
        "VOLCENGINE_ACCESS_KEY_ID",
        "VOLCENGINE_SECRET_ACCESS_KEY",
        "QINIU_ACCESS_KEY",
        "QINIU_SECRET_KEY",
        "AZURE_STORAGE_ACCOUNT_KEY",
        "AccountKey",
        "accountkey",
        "SharedAccessKey",
        "SharedAccessSignature",
        "sig",
        "signature",
        "q-signature",
        "x-amz-signature",
        "TmpSecretId",
        "TmpSecretKey",
        "x-cos-security-token",
        "bce_access_key_id",
        "bce_secret_access_key",
        "bce_security_token",
        "Ak",
        "Sk",
        "Sts",
    ];
    let secrets: Vec<String> = aliases
        .iter()
        .enumerate()
        .map(|(index, alias)| {
            let secret = format!("opaque-cloud-fixture-{index:02}-value");
            fields.insert((*alias).into(), json!(secret));
            secret
        })
        .collect();
    fields.insert("region".into(), json!("ap-shanghai"));
    fields.insert("project_id".into(), json!("project-fixture"));
    fields.insert("account_id".into(), json!("account-fixture"));
    fields.insert("path".into(), json!("/v1/instances"));
    fields.insert("task".into(), json!("list instances"));
    fields.insert("risk".into(), json!("ordinary business field"));
    let original = Value::Object(fields);
    let session = RedactionSession::new();
    let mut request = payload(original.clone());
    session.redact_payload(&mut request, &[]);
    let wire = serde_json::to_string(&request).unwrap();
    for secret in &secrets {
        assert!(!wire.contains(secret), "opaque cloud secret reached wire");
    }
    for alias in aliases {
        assert!(input(&request)[alias]
            .as_str()
            .unwrap()
            .starts_with(MARKER_PREFIX));
    }
    for name in ["region", "project_id", "account_id", "path", "task", "risk"] {
        assert_eq!(input(&request)[name], original[name]);
    }
    let mut returned = input(&request).clone();
    session.restore_tool_input(&mut returned);
    assert_eq!(returned, original);
}

#[test]
fn cloud_config_assignments_and_command_flags_share_the_same_rules() {
    let source = "TENCENTCLOUD_SECRET_ID=opaque-tencent-id\n\
                  TENCENTCLOUD_SECRET_KEY='opaque tencent secret'\n\
                  ALIBABA_CLOUD_ACCESS_KEY_ID=opaque-alibaba-id\n\
                  ALIBABA_CLOUD_ACCESS_KEY_SECRET=opaque-alibaba-secret\n\
                  AWS_ACCESS_KEY_ID=opaque-aws-id\n\
                  HUAWEICLOUD_SDK_AK=opaque-huawei-ak\n\
                  HUAWEICLOUD_SDK_SK=opaque-huawei-sk\n\
                  --secret-id opaque-flag-id --access-key-id opaque-access-id --secret-access-key opaque-access-secret\n\
                  https://cloud.example.invalid/object?q-signature=opaque-tencent-signature&X-Amz-Signature=opaque-aws-signature&sig=opaque-azure-signature";
    let session = RedactionSession::new();
    let masked = session.redact_text(source, &[]);
    for secret in [
        "opaque-tencent-id",
        "opaque tencent secret",
        "opaque-alibaba-id",
        "opaque-alibaba-secret",
        "opaque-aws-id",
        "opaque-huawei-ak",
        "opaque-huawei-sk",
        "opaque-flag-id",
        "opaque-access-id",
        "opaque-access-secret",
        "opaque-tencent-signature",
        "opaque-aws-signature",
        "opaque-azure-signature",
    ] {
        assert!(
            !masked.contains(secret),
            "cloud assignment was not redacted"
        );
    }
    assert_eq!(session.restore_text(&masked), source);
}

#[test]
fn nested_cloud_credentials_scrub_unlabeled_repeats_before_tool_restoration() {
    let original = json!({
        "cloud":{
            "provider":"tencent",
            "credentials":{
                "SecretId":"opaque-nested-id",
                "SecretKey":"opaque-nested-secret",
                "SecurityToken":"opaque-temporary-session"
            }
        },
        "retry_note":"reuse opaque-nested-secret for the same request",
        "account_id":"normal-account"
    });
    let session = RedactionSession::new();
    let mut request = payload(original.clone());
    session.redact_payload(&mut request, &[]);
    let wire = serde_json::to_string(&request).unwrap();
    for secret in [
        "opaque-nested-id",
        "opaque-nested-secret",
        "opaque-temporary-session",
    ] {
        assert!(!wire.contains(secret));
    }
    assert_eq!(input(&request)["account_id"], "normal-account");
    let mut returned = input(&request).clone();
    session.restore_tool_input(&mut returned);
    assert_eq!(returned, original);
}

#[test]
fn cloud_environment_references_remain_expressions() {
    let original = json!({
        "SecretId":"$TENCENTCLOUD_SECRET_ID",
        "SecretKey":"${TENCENTCLOUD_SECRET_KEY}",
        "AccessKeyId":"%ALIBABA_CLOUD_ACCESS_KEY_ID%",
        "AccessKeySecret":"$env:ALIBABA_CLOUD_ACCESS_KEY_SECRET",
        "SecurityToken":"process.env.CLOUD_SECURITY_TOKEN",
        "HUAWEICLOUD_SDK_AK":"os.environ[\"HUAWEICLOUD_SDK_AK\"]",
        "HUAWEICLOUD_SDK_SK":"os.getenv('HUAWEICLOUD_SDK_SK')",
        "command":"cloud request --secret-id $TENCENTCLOUD_SECRET_ID --access-key-secret ${ALIBABA_CLOUD_ACCESS_KEY_SECRET}"
    });
    let mut request = payload(original.clone());
    RedactionSession::new().redact_payload(&mut request, &[]);
    assert_eq!(input(&request), &original);
}

#[test]
fn ordinary_ids_and_opaque_unlabeled_values_are_not_blindly_masked() {
    let source = "region=ap-shanghai\nproject_id=project-fixture\naccount_id=account-fixture\ninstance_id=instance-fixture\nsts.authentication.example.com\ntask=list instances\nrisk=low\nfunction_signature=fn(value)\ntype_signature=string -> bool\n0123456789abcdef0123456789abcdef01234567";
    assert_eq!(RedactionSession::new().redact_text(source, &[]), source);
}
