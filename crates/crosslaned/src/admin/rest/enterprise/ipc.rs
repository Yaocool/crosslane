//! Trusted Unix-socket facade over the same credentials and operation control
//! used by the REST handlers. No HTTP listener or Bearer token is required.

use std::sync::Arc;

use crosslane_ipc::kiro_admin::EnterpriseScope;
use crosslane_ipc::protocol::{method, RpcError};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use super::{
    context, DeletionMode, Mutation, Plan, RequestAudit, RestError, RestState, ScopeQuery,
};
use crate::state::AppState;

type Result<T = Value> = std::result::Result<T, RpcError>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scoped {
    #[serde(default)]
    scope: EnterpriseScope,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct User {
    user: String,
    #[serde(default)]
    scope: EnterpriseScope,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangePlan {
    user: String,
    plan: Plan,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    scope: EnterpriseScope,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Cancel {
    user: String,
    #[serde(default)]
    mode: DeletionMode,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    scope: EnterpriseScope,
}

enum Operation {
    Plans,
    Profiles(Scoped),
    Scope(Scoped),
    Users(Scoped),
    List(Scoped),
    Show(User),
    Mutation {
        user: String,
        scope: EnterpriseScope,
        action: Mutation,
        dry_run: bool,
    },
}

impl ChangePlan {
    fn operation(self, action: fn(Plan) -> Mutation) -> Operation {
        Operation::Mutation {
            user: self.user,
            scope: self.scope,
            action: action(self.plan),
            dry_run: self.dry_run,
        }
    }
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T> {
    // Serde errors can contain arbitrary input, including unknown field names.
    serde_json::from_value(params)
        .map_err(|_| RpcError::bad_params("invalid Kiro subscription parameters"))
}

fn operation(name: &str, params: Value) -> Result<Operation> {
    match name {
        method::ADMIN_KIRO_SUBSCRIPTION_PLANS => {
            if !params.is_null() && params != json!({}) {
                return Err(RpcError::bad_params("plans accepts no parameters"));
            }
            Ok(Operation::Plans)
        }
        method::ADMIN_KIRO_SUBSCRIPTION_PROFILES => {
            let input: Scoped = parse(params)?;
            if input.scope.profile_arn.is_some()
                || input.scope.application_arn.is_some()
                || input.scope.identity_center_region.is_some()
            {
                return Err(RpcError::bad_params(
                    "profile discovery accepts only region and legacy scope parameters",
                ));
            }
            Ok(Operation::Profiles(input))
        }
        method::ADMIN_KIRO_SUBSCRIPTION_SCOPE => parse(params).map(Operation::Scope),
        method::ADMIN_KIRO_SUBSCRIPTION_USERS => parse(params).map(Operation::Users),
        method::ADMIN_KIRO_SUBSCRIPTION_LIST => parse(params).map(Operation::List),
        method::ADMIN_KIRO_SUBSCRIPTION_SHOW => parse(params).map(Operation::Show),
        method::ADMIN_KIRO_SUBSCRIPTION_ADD => {
            parse::<ChangePlan>(params).map(|input| input.operation(Mutation::Create))
        }
        method::ADMIN_KIRO_SUBSCRIPTION_UPGRADE => {
            parse::<ChangePlan>(params).map(|input| input.operation(Mutation::Upgrade))
        }
        method::ADMIN_KIRO_SUBSCRIPTION_DOWNGRADE => {
            parse::<ChangePlan>(params).map(|input| input.operation(Mutation::Downgrade))
        }
        method::ADMIN_KIRO_SUBSCRIPTION_CANCEL => {
            let input: Cancel = parse(params)?;
            Ok(Operation::Mutation {
                user: input.user,
                scope: input.scope,
                action: Mutation::Cancel(input.mode),
                dry_run: input.dry_run,
            })
        }
        _ => Err(RpcError::unknown_method(name)),
    }
}

impl From<EnterpriseScope> for ScopeQuery {
    fn from(scope: EnterpriseScope) -> Self {
        Self {
            region: scope.region,
            legacy: scope.legacy,
            profile_arn: scope.profile_arn,
            application_arn: scope.application_arn,
            identity_center_region: scope.identity_center_region,
            mode: None,
            dry_run: None,
        }
    }
}

fn value(result: impl Serialize) -> Result {
    serde_json::to_value(result).map_err(|_| RpcError::internal("could not encode Kiro result"))
}

fn rpc_error(error: RestError) -> RpcError {
    let (code, details) = match error {
        RestError::Enterprise(error) => (i32::from(error.http_status), json!(error)),
        RestError::Api(error) => (
            i32::from(error.status.as_u16()),
            json!({
                "code":error.code,
                "http_status":error.status.as_u16(),
                "message":error.message,
                "write_outcome":"not_submitted"
            }),
        ),
    };
    // Preserve the sanitized AWS code, request ID and outcome without changing
    // the existing RPC error schema or exposing raw upstream error bodies.
    RpcError {
        code,
        message: details.to_string(),
    }
}

pub(crate) async fn handle(app: &Arc<AppState>, name: &str, params: Value) -> Result {
    let request_id = format!("cli_{}", Uuid::new_v4().simple());
    let result = execute(app, name, params, &request_id).await;
    tracing::info!(
        event = "admin.kiro.rpc.request",
        %request_id,
        method = name,
        source = "unix_socket",
        role = "admin",
        status = result.as_ref().err().map(|e| e.code).unwrap_or(200),
        "Kiro CLI management request completed"
    );
    result
}

async fn execute(app: &Arc<AppState>, name: &str, params: Value, request_id: &str) -> Result {
    let operation = operation(name, params)?;
    if let Operation::Plans = operation {
        return value(
            Plan::ALL.map(|plan| json!({"plan":plan,"subscription_type":plan.aws_type()})),
        );
    }
    // This constructor retains the REST instance's concurrency slots and
    // canonical-user mutation guard, including when REST is disabled.
    let state = RestState::new(app.clone());
    let scope = match &operation {
        Operation::Profiles(input)
        | Operation::Scope(input)
        | Operation::Users(input)
        | Operation::List(input) => &input.scope,
        Operation::Show(input) => &input.scope,
        Operation::Mutation { scope, .. } => scope,
        Operation::Plans => unreachable!(),
    };
    let (client, selector) = context(&state, &scope.clone().into())
        .await
        .map_err(rpc_error)?;
    match operation {
        Operation::Plans => unreachable!(),
        Operation::Profiles(_) => value(
            client
                .profiles()
                .await
                .map_err(RestError::from)
                .map_err(rpc_error)?,
        ),
        Operation::Scope(_) => value(
            client
                .scope(&selector)
                .await
                .map_err(RestError::from)
                .map_err(rpc_error)?,
        ),
        Operation::Users(_) => value(
            client
                .users(&selector)
                .await
                .map_err(RestError::from)
                .map_err(rpc_error)?,
        ),
        Operation::List(_) => {
            let (scope, subscriptions) = client
                .subscriptions(&selector)
                .await
                .map_err(RestError::from)
                .map_err(rpc_error)?;
            value(json!({"scope":scope,"subscriptions":subscriptions}))
        }
        Operation::Show(input) => value(
            client
                .user_subscription(&selector, &input.user)
                .await
                .map_err(RestError::from)
                .map_err(rpc_error)?,
        ),
        Operation::Mutation {
            user,
            action,
            dry_run,
            ..
        } => {
            let audit = RequestAudit {
                request_id: request_id.into(),
                token_id: "unix_socket".into(),
                role: "admin",
                source: "unix_socket",
            };
            value(
                state
                    .enterprise
                    .execute(client, selector, user, action, dry_run, audit)
                    .await
                    .map_err(rpc_error)?,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_rpc_inputs_cannot_echo_credentials_or_enable_extra_assignment_calls() {
        let secret = "fixture-sensitive-unknown-field";
        for params in [
            json!({"user":"fixture","plan":"unknown"}),
            json!({"user":"fixture","plan":"pro","assign_application":true}),
            json!({"user":"fixture","plan":"pro","scope":{secret:secret}}),
            json!({"user":"fixture","plan":"pro","secret_access_key":secret}),
            json!({"user":"fixture","plan":"pro","dry_run":"true"}),
        ] {
            for method in [
                method::ADMIN_KIRO_SUBSCRIPTION_ADD,
                method::ADMIN_KIRO_SUBSCRIPTION_UPGRADE,
                method::ADMIN_KIRO_SUBSCRIPTION_DOWNGRADE,
            ] {
                let error = operation(method, params.clone()).err().unwrap();
                assert_eq!(error.code, 400);
                assert!(!error.message.contains(secret));
            }
        }
    }

    #[test]
    fn all_mutations_keep_native_intent_preview_and_cancellation_defaults() {
        for (method, kind, params) in [
            (
                method::ADMIN_KIRO_SUBSCRIPTION_ADD,
                "create",
                json!({"user":"fixture","plan":"power","dry_run":true}),
            ),
            (
                method::ADMIN_KIRO_SUBSCRIPTION_UPGRADE,
                "upgrade",
                json!({"user":"fixture","plan":"power","dry_run":true}),
            ),
            (
                method::ADMIN_KIRO_SUBSCRIPTION_DOWNGRADE,
                "downgrade",
                json!({"user":"fixture","plan":"power","dry_run":true}),
            ),
            (
                method::ADMIN_KIRO_SUBSCRIPTION_CANCEL,
                "cancel_period",
                json!({"user":"fixture","dry_run":true}),
            ),
            (
                method::ADMIN_KIRO_SUBSCRIPTION_CANCEL,
                "cancel_immediate",
                json!({"user":"fixture","mode":"immediate","dry_run":true}),
            ),
        ] {
            let Operation::Mutation {
                action, dry_run, ..
            } = operation(method, params).unwrap()
            else {
                panic!("expected mutation")
            };
            assert!(dry_run);
            let actual = match action {
                Mutation::Create(Plan::Power) => "create",
                Mutation::Upgrade(Plan::Power) => "upgrade",
                Mutation::Downgrade(Plan::Power) => "downgrade",
                Mutation::Cancel(DeletionMode::EndOfPeriod) => "cancel_period",
                Mutation::Cancel(DeletionMode::Immediate) => "cancel_immediate",
                _ => panic!("unexpected plan"),
            };
            assert_eq!(actual, kind);
        }
        let Operation::Mutation {
            dry_run, action, ..
        } = operation(
            method::ADMIN_KIRO_SUBSCRIPTION_CANCEL,
            json!({"user":"fixture"}),
        )
        .unwrap()
        else {
            panic!("expected cancellation")
        };
        assert!(!dry_run);
        assert!(matches!(
            action,
            Mutation::Cancel(DeletionMode::EndOfPeriod)
        ));
    }

    #[test]
    fn malformed_cancellation_and_profile_selectors_fail_before_credentials() {
        for params in [
            json!({"user":"fixture","mode":"end-of-period"}), // CLI normalizes to native wire spelling.
            json!({"user":"fixture","mode":"invalid"}),
            json!({"user":"fixture","plan":"pro"}),
            json!({"user":"fixture","dry_run":"true"}),
            json!({"user":"fixture","scope":{"mode":"immediate"}}),
        ] {
            assert_eq!(
                operation(method::ADMIN_KIRO_SUBSCRIPTION_CANCEL, params)
                    .err()
                    .unwrap()
                    .code,
                400
            );
        }
        for key in ["profile_arn", "application_arn", "identity_center_region"] {
            assert_eq!(
                operation(
                    method::ADMIN_KIRO_SUBSCRIPTION_PROFILES,
                    json!({"scope":{key:"fixture-sensitive-value"}})
                )
                .err()
                .unwrap()
                .code,
                400
            );
        }
    }

    #[test]
    fn selectors_and_preview_survive_rpc_decoding() {
        let Operation::Mutation { scope, action, dry_run, .. } = operation(method::ADMIN_KIRO_SUBSCRIPTION_ADD, json!({
            "user":"arn:aws:identitystore:::user/11111111-1111-4111-8111-111111111111",
            "plan":"power","dry_run":true,
            "scope":{"region":"us-east-1","profile_arn":"profile-selector","application_arn":"application-selector","identity_center_region":"eu-west-1","legacy":true}
        })).unwrap() else { panic!("expected add") };
        assert!(dry_run);
        assert!(matches!(action, Mutation::Create(Plan::Power)));
        let scope = ScopeQuery::from(scope);
        assert_eq!(scope.profile_arn.as_deref(), Some("profile-selector"));
        assert_eq!(
            scope.application_arn.as_deref(),
            Some("application-selector")
        );
        assert_eq!(scope.identity_center_region.as_deref(), Some("eu-west-1"));
        assert!(scope.legacy);
        assert!(scope.dry_run.is_none() && scope.mode.is_none());
    }

    #[test]
    fn aws_error_details_are_retained_in_the_legacy_rpc_error_envelope() {
        let mut error = super::super::unknown_outcome("CreateAssignment");
        error.upstream_code = Some("InternalFailure".into());
        error.upstream_request_id = Some("fixture-aws-request-id".into());
        error.upstream_status = Some(500);
        let rpc = rpc_error(RestError::Enterprise(error));
        assert_eq!(rpc.code, 502);
        let details: Value = serde_json::from_str(&rpc.message).unwrap();
        assert_eq!(details["operation"], "CreateAssignment");
        assert_eq!(details["write_outcome"], "unknown");
        assert_eq!(details["upstream_code"], "InternalFailure");
        assert_eq!(details["upstream_request_id"], "fixture-aws-request-id");
        assert_eq!(details["upstream_status"], 500);
    }
}
