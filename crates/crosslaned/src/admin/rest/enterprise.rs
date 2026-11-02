//! AWS administrator credentials come only from crosslaned's environment,
//! independently of the Admin Bearer token and stored OAuth account pools.
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use crosslane_kiro::enterprise::{
    AwsCredentials, DeletionMode, EnterpriseClient, EnterpriseError, Mutation, MutationCoordinator,
    MutationResult, Plan, ScopeSelector, WriteOutcome,
};
use futures::FutureExt;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Semaphore;
use tracing::instrument::WithSubscriber;

use super::{body, ok, query, ApiError, RequestAudit, RestState};

pub(super) fn router() -> Router<RestState> {
    Router::new()
        .route("/plans", get(plans))
        .route("/profiles", get(profiles))
        .route("/scope", get(scope))
        .route("/users", get(users))
        .route("/subscriptions", get(subscriptions))
        .route(
            "/users/:user/subscription",
            get(user_subscription).post(create).delete(cancel),
        )
        .route("/users/:user/subscription/upgrade", post(upgrade))
        .route("/users/:user/subscription/downgrade", post(downgrade))
}

type Result<T = Response> = std::result::Result<T, RestError>;

pub(super) struct Control {
    mutations: Arc<MutationCoordinator>,
    slots: Arc<Semaphore>,
}

impl Default for Control {
    fn default() -> Self {
        Self {
            mutations: Arc::new(MutationCoordinator::default()),
            slots: Arc::new(Semaphore::new(8)),
        }
    }
}

impl Control {
    async fn execute(
        &self,
        client: EnterpriseClient,
        selector: ScopeSelector,
        user: String,
        mutation: Mutation,
        dry_run: bool,
        audit: RequestAudit,
    ) -> Result<MutationResult> {
        let client = client.with_mutation_coordinator(self.mutations.clone());
        let operation = mutation.operation();
        run_detached(
            self.slots.clone(),
            operation,
            async move { client.mutate(&selector, &user, mutation, dry_run).await },
            move |result| record_completion(operation, &audit, result),
        )
        .await
    }
}

fn record_completion(
    operation: &'static str,
    audit: &RequestAudit,
    result: &std::result::Result<MutationResult, EnterpriseError>,
) {
    let write_outcome = match result {
        Ok(result) if result.status == "api_accepted" => "accepted",
        Ok(_) => "not_submitted",
        Err(error) => error.write_outcome.as_str(),
    };
    tracing::info!(event = "admin.kiro.subscription.completed", operation,
        request_id = %audit.request_id, token_id = %audit.token_id, role = audit.role,
        status = result.as_ref().map(|result| result.status).unwrap_or("failed"),
        error_code = result.as_ref().err().map(|error| error.code), write_outcome,
        upstream_status = result.as_ref().err().and_then(|error| error.upstream_status),
        upstream_request_id = result.as_ref().ok().and_then(|result| result.upstream_request_id.as_deref())
            .or_else(|| result.as_ref().err().and_then(|error| error.upstream_request_id.as_deref())),
        "Kiro subscription operation completed");
}

async fn run_detached<T, F, R>(
    slots: Arc<Semaphore>,
    operation: &'static str,
    future: F,
    report: R,
) -> Result<T>
where
    T: Send + 'static,
    F: Future<Output = std::result::Result<T, EnterpriseError>> + Send + 'static,
    R: FnOnce(&std::result::Result<T, EnterpriseError>) + Send + 'static,
{
    let permit = slots.try_acquire_owned().map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            "Kiro subscription operation limit reached",
        )
    })?;
    // A disconnected HTTP caller must not release the canonical-user guard
    // while AWS is still processing its write. Keep the bounded operation
    // alive until its response or the existing AWS deadline completes.
    tokio::spawn(
        async move {
            let _permit = permit;
            // Capture execution failures inside the worker, before completion
            // reporting. A dropped HTTP waiter must not lose the failure audit.
            let result = AssertUnwindSafe(future)
                .catch_unwind()
                .await
                .unwrap_or_else(|_| Err(unknown_outcome(operation)));
            report(&result);
            result
        }
        .with_current_subscriber(),
    )
    .await
    .map_err(|_| unknown_outcome(operation))?
    .map_err(RestError::from)
}

fn unknown_outcome(operation: &'static str) -> EnterpriseError {
    EnterpriseError {
        code: "aws_outcome_unknown",
        message: "Kiro operation was interrupted; query subscription state before retrying",
        http_status: 502,
        operation: Some(operation.into()),
        upstream_status: None,
        upstream_code: None,
        upstream_request_id: None,
        write_outcome: WriteOutcome::Unknown,
    }
}

#[derive(Debug)]
pub(super) enum RestError {
    Api(ApiError),
    Enterprise(EnterpriseError),
}

impl From<ApiError> for RestError {
    fn from(value: ApiError) -> Self {
        Self::Api(value)
    }
}
impl From<EnterpriseError> for RestError {
    fn from(value: EnterpriseError) -> Self {
        Self::Enterprise(value)
    }
}

impl IntoResponse for RestError {
    fn into_response(self) -> Response {
        match self {
            Self::Api(error) => error.into_response(),
            Self::Enterprise(error) => (
                StatusCode::from_u16(error.http_status).unwrap_or(StatusCode::BAD_GATEWAY),
                Json(json!({"error":error})),
            )
                .into_response(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopeQuery {
    region: Option<String>,
    #[serde(default)]
    legacy: bool,
    profile_arn: Option<String>,
    application_arn: Option<String>,
    identity_center_region: Option<String>,
    // Only DELETE accepts cancellation controls in the query string.
    mode: Option<DeletionMode>,
    dry_run: Option<bool>,
}

impl ScopeQuery {
    fn for_non_delete(request: &Request) -> Result<Self> {
        let value: Self = query(request)?;
        if value.mode.is_some() || value.dry_run.is_some() {
            return Err(ApiError::bad(
                "mode and query dry_run are supported only for subscription cancellation",
            )
            .into());
        }
        Ok(value)
    }
}

fn configuration_error() -> RestError {
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "kiro_admin_not_configured", "set valid CROSSLANE_KIRO_ADMIN_ACCESS_KEY_ID, CROSSLANE_KIRO_ADMIN_SECRET_ACCESS_KEY and, for STS credentials, CROSSLANE_KIRO_ADMIN_SESSION_TOKEN in the crosslaned environment").into()
}

fn context(input: &ScopeQuery) -> Result<(EnterpriseClient, ScopeSelector)> {
    context_with(input, &|name| match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(configuration_error()),
    })
}

// Keep environment loading injectable: tests never mutate the process-global
// credential environment or accidentally fall back to real AWS credentials.
fn context_with(
    input: &ScopeQuery,
    read: &impl Fn(&str) -> Result<Option<String>>,
) -> Result<(EnterpriseClient, ScopeSelector)> {
    let optional = |name: &str| -> Result<Option<String>> {
        Ok(read(name)?.filter(|value| !value.is_empty()))
    };
    let credentials = AwsCredentials::new(
        read("CROSSLANE_KIRO_ADMIN_ACCESS_KEY_ID")?.unwrap_or_default(),
        read("CROSSLANE_KIRO_ADMIN_SECRET_ACCESS_KEY")?.unwrap_or_default(),
        optional("CROSSLANE_KIRO_ADMIN_SESSION_TOKEN")?,
    )
    .map_err(|_| configuration_error())?;
    let fallback = |value: &Option<String>, name: &str| match value {
        Some(value) => Ok(Some(value.clone())),
        None => optional(name),
    };
    let region = fallback(&input.region, "CROSSLANE_KIRO_ADMIN_REGION")?
        .unwrap_or_else(|| "us-east-1".into());
    let selector = ScopeSelector {
        profile_arn: fallback(&input.profile_arn, "CROSSLANE_KIRO_ADMIN_PROFILE_ARN")?,
        application_arn: fallback(
            &input.application_arn,
            "CROSSLANE_KIRO_ADMIN_APPLICATION_ARN",
        )?,
        identity_center_region: fallback(
            &input.identity_center_region,
            "CROSSLANE_KIRO_ADMIN_IDENTITY_CENTER_REGION",
        )?,
    };
    Ok((
        EnterpriseClient::new(credentials, region, input.legacy)?,
        selector,
    ))
}

async fn plans() -> Response {
    ok(Plan::ALL.map(|plan| json!({"plan":plan,"subscription_type":plan.aws_type()})))
}

async fn profiles(request: Request) -> Result {
    let input = ScopeQuery::for_non_delete(&request)?;
    if input.profile_arn.is_some()
        || input.application_arn.is_some()
        || input.identity_center_region.is_some()
    {
        return Err(ApiError::bad(
            "profile discovery accepts only region and legacy query parameters",
        )
        .into());
    }
    Ok(ok(context(&input)?.0.profiles().await?))
}

async fn scope(request: Request) -> Result {
    let input = ScopeQuery::for_non_delete(&request)?;
    let (client, selector) = context(&input)?;
    Ok(ok(client.scope(&selector).await?))
}

async fn users(request: Request) -> Result {
    let input = ScopeQuery::for_non_delete(&request)?;
    let (client, selector) = context(&input)?;
    Ok(ok(client.users(&selector).await?))
}

async fn subscriptions(request: Request) -> Result {
    let input = ScopeQuery::for_non_delete(&request)?;
    let (client, selector) = context(&input)?;
    let (scope, subscriptions) = client.subscriptions(&selector).await?;
    Ok(ok(json!({"scope":scope,"subscriptions":subscriptions})))
}

async fn user_subscription(Path(user): Path<String>, request: Request) -> Result {
    let input = ScopeQuery::for_non_delete(&request)?;
    let (client, selector) = context(&input)?;
    Ok(ok(client.user_subscription(&selector, &user).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangePlan {
    plan: Plan,
    #[serde(default)]
    dry_run: bool,
}

async fn create(
    State(state): State<RestState>,
    Path(user): Path<String>,
    request: Request,
) -> Result {
    change(state.enterprise, user, request, Mutation::Create).await
}
async fn upgrade(
    State(state): State<RestState>,
    Path(user): Path<String>,
    request: Request,
) -> Result {
    change(state.enterprise, user, request, Mutation::Upgrade).await
}
async fn downgrade(
    State(state): State<RestState>,
    Path(user): Path<String>,
    request: Request,
) -> Result {
    change(state.enterprise, user, request, Mutation::Downgrade).await
}

async fn change(
    control: Arc<Control>,
    user: String,
    request: Request,
    mutation: fn(Plan) -> Mutation,
) -> Result {
    let input = ScopeQuery::for_non_delete(&request)?;
    let audit = request
        .extensions()
        .get::<RequestAudit>()
        .cloned()
        .ok_or_else(ApiError::internal)?;
    let body: ChangePlan = body(request).await?;
    let (client, selector) = context(&input)?;
    let result = control
        .execute(
            client,
            selector,
            user,
            mutation(body.plan),
            body.dry_run,
            audit,
        )
        .await?;
    Ok((
        if body.dry_run {
            StatusCode::OK
        } else {
            StatusCode::ACCEPTED
        },
        Json(json!({"data":result})),
    )
        .into_response())
}

async fn cancel(
    State(state): State<RestState>,
    Path(user): Path<String>,
    request: Request,
) -> Result {
    let input: ScopeQuery = query(&request)?;
    let audit = request
        .extensions()
        .get::<RequestAudit>()
        .cloned()
        .ok_or_else(ApiError::internal)?;
    let (client, selector) = context(&input)?;
    let dry_run = input.dry_run.unwrap_or(false);
    let result = state
        .enterprise
        .execute(
            client,
            selector,
            user,
            Mutation::Cancel(input.mode.unwrap_or_default()),
            dry_run,
            audit,
        )
        .await?;
    Ok((
        if dry_run {
            StatusCode::OK
        } else {
            StatusCode::ACCEPTED
        },
        Json(json!({"data":result})),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn audit_a_disconnected_panic_still_reports_its_actor_and_unknown_outcome() {
        #[derive(Clone)]
        struct LogBuffer(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for LogBuffer {
            fn write(&mut self, value: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(value);
                Ok(value.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = bytes.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_ansi(false)
            .with_writer(move || LogBuffer(sink.clone()))
            .finish();
        let slots = Arc::new(Semaphore::new(1));
        let worker_slots = slots.clone();
        let release = Arc::new(tokio::sync::Notify::new());
        let released = release.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (reported, completed) = tokio::sync::oneshot::channel();
        let audit = RequestAudit {
            request_id: "admin_round_two".into(),
            token_id: "adm_round_two".into(),
            role: "admin",
        };
        let request = tracing::subscriber::with_default(subscriber, || {
            tokio::spawn(
                run_detached(
                    worker_slots,
                    "UpdateAssignment",
                    async move {
                        let _ = started.send(());
                        released.notified().await;
                        panic!("fixture-sensitive-panic-payload");
                    },
                    move |result: &std::result::Result<MutationResult, EnterpriseError>| {
                        record_completion("UpdateAssignment", &audit, result);
                        let _ = reported.send(());
                    },
                )
                .with_current_subscriber(),
            )
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), ready)
            .await
            .unwrap()
            .unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(2), completed)
            .await
            .unwrap()
            .unwrap();
        let permit = tokio::time::timeout(std::time::Duration::from_secs(2), slots.acquire_owned())
            .await
            .unwrap()
            .unwrap();
        drop(permit);
        let logs = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        let events: Vec<serde_json::Value> = logs
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let fields = &events
            .iter()
            .find(|event| event["fields"]["event"] == "admin.kiro.subscription.completed")
            .unwrap()["fields"];
        assert_eq!(fields["request_id"], "admin_round_two");
        assert_eq!(fields["token_id"], "adm_round_two");
        assert_eq!(fields["role"], "admin");
        assert_eq!(fields["status"], "failed");
        assert_eq!(fields["write_outcome"], "unknown");
        assert_eq!(fields["error_code"], "aws_outcome_unknown");
        assert!(!logs.contains("fixture-sensitive-panic-payload"));
    }

    #[tokio::test]
    async fn review_a_disconnected_caller_does_not_release_the_active_operation_slot() {
        let slots = Arc::new(Semaphore::new(1));
        let finish = Arc::new(tokio::sync::Notify::new());
        let finished = finish.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        let worker_slots = slots.clone();
        let request = tokio::spawn(async move {
            run_detached(
                worker_slots,
                "CreateAssignment",
                async move {
                    let _ = started.send(());
                    finished.notified().await;
                    Ok(())
                },
                |_| {},
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), ready)
            .await
            .unwrap()
            .unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        assert_eq!(slots.available_permits(), 0);
        let error = run_detached(slots.clone(), "CreateAssignment", async { Ok(()) }, |_| {})
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            RestError::Api(ApiError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                ..
            })
        ));
        finish.notify_one();
        let permit = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            slots.clone().acquire_owned(),
        )
        .await
        .unwrap()
        .unwrap();
        drop(permit);
        assert_eq!(slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn review_an_interrupted_worker_reports_an_unknown_outcome_and_releases_capacity() {
        let slots = Arc::new(Semaphore::new(1));
        let result: Result<()> = run_detached(
            slots.clone(),
            "UpdateAssignment",
            async { panic!("fixture interrupted subscription worker") },
            |_| {},
        )
        .await;
        match result.unwrap_err() {
            RestError::Enterprise(error) => {
                assert_eq!(error.write_outcome, WriteOutcome::Unknown);
                assert_eq!(error.operation.as_deref(), Some("UpdateAssignment"));
            }
            other => panic!("unexpected failure: {other:?}"),
        }
        assert_eq!(slots.available_permits(), 1);
    }

    #[test]
    fn review_empty_optional_environment_values_are_unset_but_sts_still_requires_a_token() {
        let input: ScopeQuery = serde_json::from_value(json!({})).unwrap();
        let mut values = HashMap::from([
            ("CROSSLANE_KIRO_ADMIN_ACCESS_KEY_ID", "AKIA000000000EXAMPLE"),
            (
                "CROSSLANE_KIRO_ADMIN_SECRET_ACCESS_KEY",
                "fixture-environment-secret",
            ),
            ("CROSSLANE_KIRO_ADMIN_SESSION_TOKEN", ""),
            ("CROSSLANE_KIRO_ADMIN_REGION", ""),
            ("CROSSLANE_KIRO_ADMIN_PROFILE_ARN", ""),
            ("CROSSLANE_KIRO_ADMIN_APPLICATION_ARN", ""),
            ("CROSSLANE_KIRO_ADMIN_IDENTITY_CENTER_REGION", ""),
        ]);
        let result = context_with(&input, &|name| {
            Ok(values.get(name).map(|value| (*value).into()))
        });
        assert!(
            result.is_ok(),
            "blank optional deployment variables must not invalidate a long-lived AWS key"
        );
        let (_, selector) = result.ok().unwrap();
        assert!(
            selector.profile_arn.is_none()
                && selector.application_arn.is_none()
                && selector.identity_center_region.is_none()
        );
        values.insert("CROSSLANE_KIRO_ADMIN_ACCESS_KEY_ID", "ASIA000000000EXAMPLE");
        assert!(context_with(&input, &|name| Ok(values
            .get(name)
            .map(|value| (*value).into())))
        .is_err());
        let explicit: ScopeQuery = serde_json::from_value(json!({"profile_arn":""})).unwrap();
        values.insert("CROSSLANE_KIRO_ADMIN_ACCESS_KEY_ID", "AKIA000000000EXAMPLE");
        let (_, selector) = context_with(&explicit, &|name| {
            Ok(values.get(name).map(|value| (*value).into()))
        })
        .ok()
        .unwrap();
        assert_eq!(selector.profile_arn.as_deref(), Some(""));
    }

    #[test]
    fn only_the_dedicated_daemon_environment_provides_aws_credentials() {
        let input: ScopeQuery = serde_json::from_value(json!({})).unwrap();
        let mut values = HashMap::from([
            ("AWS_ACCESS_KEY_ID", "AKIA000000000EXAMPLE"),
            ("AWS_SECRET_ACCESS_KEY", "unrelated-default-chain-secret"),
        ]);
        let error = context_with(&input, &|name| {
            Ok(values.get(name).map(|value| (*value).into()))
        })
        .err()
        .unwrap();
        assert!(matches!(
            error,
            RestError::Api(ApiError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                ..
            })
        ));
        values.insert("CROSSLANE_KIRO_ADMIN_ACCESS_KEY_ID", "ASIA000000000EXAMPLE");
        values.insert(
            "CROSSLANE_KIRO_ADMIN_SECRET_ACCESS_KEY",
            "fixture-environment-secret",
        );
        assert!(context_with(&input, &|name| Ok(values
            .get(name)
            .map(|value| (*value).into())))
        .is_err());
        values.insert(
            "CROSSLANE_KIRO_ADMIN_SESSION_TOKEN",
            "fixture-environment-session-token",
        );
        values.insert("CROSSLANE_KIRO_ADMIN_PROFILE_ARN", "fixture-profile");
        values.insert(
            "CROSSLANE_KIRO_ADMIN_APPLICATION_ARN",
            "fixture-application",
        );
        let (_, selector) = context_with(&input, &|name| {
            Ok(values.get(name).map(|value| (*value).into()))
        })
        .ok()
        .unwrap();
        assert_eq!(selector.profile_arn.as_deref(), Some("fixture-profile"));
        assert_eq!(
            selector.application_arn.as_deref(),
            Some("fixture-application")
        );
        values.insert("CROSSLANE_KIRO_ADMIN_REGION", "us-east-1.attacker.invalid");
        assert!(context_with(&input, &|name| Ok(values
            .get(name)
            .map(|value| (*value).into())))
        .is_err());
        let explicit: ScopeQuery =
            serde_json::from_value(json!({"region":"us-east-1","profile_arn":"explicit-profile"}))
                .unwrap();
        let (_, selector) = context_with(&explicit, &|name| {
            Ok(values.get(name).map(|value| (*value).into()))
        })
        .ok()
        .unwrap();
        assert_eq!(selector.profile_arn.as_deref(), Some("explicit-profile"));
    }
}
