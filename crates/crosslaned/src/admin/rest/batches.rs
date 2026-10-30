//! Bounded, process-local bulk creation. Registry entries never retain credentials.

use std::collections::BTreeMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Mutex;

use axum::extract::{FromRequest, Path};
use futures::{stream, FutureExt, StreamExt};
use serde::Serialize;
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::sync::CancellationToken;

use super::accounts::PreparedCreate;
use super::*;

const MAX_ROWS: usize = 1000;
const MAX_RETAINED: usize = 32;
const RETENTION_SECS: i64 = 3600;

pub(super) struct BatchRegistry {
    jobs: Mutex<BTreeMap<String, Arc<BatchJob>>>,
    active: Arc<Semaphore>,
}

impl Default for BatchRegistry {
    fn default() -> Self {
        Self {
            jobs: Mutex::new(BTreeMap::new()),
            active: Arc::new(Semaphore::new(4)),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RowStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Serialize)]
struct RowError {
    code: &'static str,
    message: String,
}

#[derive(Clone, Serialize)]
struct Row {
    index: usize,
    provider: String,
    display_name: Option<String>,
    status: RowStatus,
    account_id: Option<String>,
    reference: Option<String>,
    error: Option<RowError>,
    started_at: Option<i64>,
    finished_at: Option<i64>,
}

struct Progress {
    rows: Vec<Row>,
    started_at: Option<i64>,
    finished_at: Option<i64>,
}

struct BatchJob {
    id: String,
    created_at: i64,
    progress: Mutex<Progress>,
    cancel: CancellationToken,
}

#[derive(Serialize)]
struct Report {
    id: String,
    status: &'static str,
    created_at: i64,
    started_at: Option<i64>,
    finished_at: Option<i64>,
    total: usize,
    pending: usize,
    running: usize,
    succeeded: usize,
    failed: usize,
    cancelled: usize,
    completed: usize,
    complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    rows: Option<Vec<Row>>,
}

impl BatchJob {
    fn new(inputs: &[PreparedCreate]) -> Self {
        Self {
            id: format!("batch_{}", Uuid::new_v4().simple()),
            created_at: crate::meter::now_secs(),
            cancel: CancellationToken::new(),
            progress: Mutex::new(Progress {
                started_at: None,
                finished_at: None,
                rows: inputs
                    .iter()
                    .enumerate()
                    .map(|(index, input)| {
                        let (provider, display_name) = input.identity();
                        Row {
                            index: index + 1,
                            provider,
                            display_name,
                            status: RowStatus::Queued,
                            account_id: None,
                            reference: None,
                            error: None,
                            started_at: None,
                            finished_at: None,
                        }
                    })
                    .collect(),
            }),
        }
    }

    fn report(&self, include_rows: bool) -> Report {
        let progress = self
            .progress
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let count = |status| {
            progress
                .rows
                .iter()
                .filter(|row| row.status == status)
                .count()
        };
        let pending = count(RowStatus::Queued);
        let running = count(RowStatus::Running);
        let succeeded = count(RowStatus::Succeeded);
        let failed = count(RowStatus::Failed);
        let cancelled = count(RowStatus::Cancelled);
        let status = if progress.finished_at.is_some() {
            if cancelled > 0 {
                "cancelled"
            } else if failed > 0 {
                "completed_with_errors"
            } else {
                "completed"
            }
        } else if self.cancel.is_cancelled() {
            "cancelling"
        } else if progress.started_at.is_some() {
            "running"
        } else {
            "queued"
        };
        Report {
            id: self.id.clone(),
            status,
            created_at: self.created_at,
            started_at: progress.started_at,
            finished_at: progress.finished_at,
            total: progress.rows.len(),
            pending,
            running,
            succeeded,
            failed,
            cancelled,
            completed: succeeded + failed + cancelled,
            complete: progress.finished_at.is_some() && failed == 0 && cancelled == 0,
            rows: include_rows.then(|| progress.rows.clone()),
        }
    }

    fn row(&self, index: usize, update: impl FnOnce(&mut Row)) {
        let mut progress = self
            .progress
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        update(&mut progress.rows[index]);
    }
}

impl BatchRegistry {
    fn prune(jobs: &mut BTreeMap<String, Arc<BatchJob>>) {
        let now = crate::meter::now_secs();
        jobs.retain(|_, job| {
            job.report(false)
                .finished_at
                .is_none_or(|finished| now.saturating_sub(finished) < RETENTION_SECS)
        });
    }

    fn insert(
        &self,
        inputs: &[PreparedCreate],
    ) -> ApiResult<(Arc<BatchJob>, OwnedSemaphorePermit)> {
        let permit = self.active.clone().try_acquire_owned().map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "busy",
                "at most four account batches may run at once",
            )
        })?;
        let mut jobs = self.jobs.lock().unwrap_or_else(|error| error.into_inner());
        Self::prune(&mut jobs);
        if jobs.len() >= MAX_RETAINED {
            let oldest = jobs
                .iter()
                .filter(|(_, job)| job.report(false).finished_at.is_some())
                .min_by_key(|(_, job)| job.created_at)
                .map(|(id, _)| id.clone());
            if let Some(id) = oldest {
                jobs.remove(&id);
            }
        }
        let job = Arc::new(BatchJob::new(inputs));
        jobs.insert(job.id.clone(), Arc::clone(&job));
        Ok((job, permit))
    }

    fn get(&self, id: &str) -> ApiResult<Arc<BatchJob>> {
        let mut jobs = self.jobs.lock().unwrap_or_else(|error| error.into_inner());
        Self::prune(&mut jobs);
        jobs.get(id)
            .cloned()
            .ok_or_else(|| ApiError::not_found("account batch not found or expired"))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBatch {
    provider: Option<String>,
    auth: Option<String>,
    start_url: Option<String>,
    region: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    tag: Option<String>,
    csv: Option<String>,
    accounts: Option<Vec<Value>>,
    #[serde(default = "default_concurrency")]
    concurrency: usize,
}
fn default_concurrency() -> usize {
    1
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CsvQuery {
    provider: String,
    auth: Option<String>,
    start_url: Option<String>,
    region: Option<String>,
    tag: Option<String>,
    #[serde(default = "default_concurrency")]
    concurrency: usize,
}

fn prepare(app: &Arc<AppState>, input: CreateBatch) -> ApiResult<(Vec<PreparedCreate>, usize)> {
    // Defaults are cloned into every row; bound them before that expansion.
    accounts::validate_login_options(input.region.as_deref(), input.start_url.as_deref())?;
    if !(1..=8).contains(&input.concurrency) {
        return Err(ApiError::bad("concurrency must be between 1 and 8"));
    }
    if input.csv.is_some() == input.accounts.is_some() {
        return Err(ApiError::bad("provide exactly one of csv or accounts"));
    }
    let tags = accounts::normalize_tags(input.tags, input.tag)?;
    let mut defaults = serde_json::Map::new();
    for (field, value) in [
        ("provider", input.provider),
        ("auth", input.auth),
        ("start_url", input.start_url),
        ("region", input.region),
    ] {
        if let Some(value) = value {
            defaults.insert(field.into(), json!(value));
        }
    }
    let rows = if let Some(csv) = input.csv {
        if !defaults.contains_key("provider")
            || defaults.get("auth").is_some_and(|auth| auth != "sso")
        {
            return Err(ApiError::bad(
                "CSV requires a Kiro provider instance and auth=sso",
            ));
        }
        defaults.insert("auth".into(), json!("sso"));
        crosslane_core::account_csv::parse_kiro_sso_csv(&csv)
            .map_err(|error| ApiError::bad(error.to_string()))?
            .into_iter()
            .map(|(email, password)| json!({"email":email,"password":password}))
            .collect()
    } else {
        input.accounts.expect("exclusive input source checked")
    };
    if rows.is_empty() || rows.len() > MAX_ROWS {
        return Err(ApiError::bad("a batch must contain 1..1000 accounts"));
    }
    let mut prepared = Vec::with_capacity(rows.len());
    for (index, row) in rows.into_iter().enumerate() {
        let mut row = row
            .as_object()
            .cloned()
            .ok_or_else(|| ApiError::bad(format!("account row {} must be an object", index + 1)))?;
        for (field, value) in &defaults {
            row.entry(field.clone()).or_insert_with(|| value.clone());
        }
        prepared.push(
            accounts::prepare_json(app, Value::Object(row), &tags).map_err(|mut error| {
                error.message = format!("account row {}: {}", index + 1, error.message);
                error
            })?,
        );
    }
    Ok((prepared, input.concurrency))
}

pub(super) async fn create(
    State(state): State<RestState>,
    request: Request,
) -> ApiResult<Response> {
    let csv = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/csv"));
    let input = if csv {
        let options: CsvQuery = query(&request)?;
        let raw = String::from_request(request, &()).await.map_err(|error| {
            ApiError::new(
                if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    StatusCode::PAYLOAD_TOO_LARGE
                } else {
                    StatusCode::BAD_REQUEST
                },
                "invalid_request",
                "CSV must be UTF-8 and no larger than 2 MiB",
            )
        })?;
        CreateBatch {
            provider: Some(options.provider),
            auth: options.auth,
            start_url: options.start_url,
            region: options.region,
            tags: vec![],
            tag: options.tag,
            csv: Some(raw),
            accounts: None,
            concurrency: options.concurrency,
        }
    } else {
        body(request).await?
    };
    let (inputs, concurrency) = prepare(&state.app, input)?;
    if state.app.shutdown.is_cancelled() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "daemon is shutting down",
        ));
    }
    let (job, active) = state.batches.insert(&inputs)?;
    let report = job.report(true);
    let location = format!("/admin/v1/account-batches/{}", job.id);
    let app = Arc::clone(&state.app);
    tokio::spawn(async move {
        let _active = active;
        let shutdown = app.shutdown.clone();
        run_job(
            job,
            inputs,
            concurrency,
            state.account_creates,
            shutdown,
            move |input| {
                let app = Arc::clone(&app);
                async move { accounts::execute(&app, input).await }
            },
        )
        .await;
    });
    Ok((
        StatusCode::ACCEPTED,
        [(header::LOCATION, location)],
        Json(json!({"data":report})),
    )
        .into_response())
}

pub(super) async fn list(State(state): State<RestState>) -> Response {
    let mut jobs = state
        .batches
        .jobs
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    BatchRegistry::prune(&mut jobs);
    let mut reports = jobs
        .values()
        .map(|job| job.report(false))
        .collect::<Vec<_>>();
    reports.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    ok(json!({"batches":reports}))
}
pub(super) async fn show(
    State(state): State<RestState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    Ok(ok(state.batches.get(&id)?.report(true)))
}
pub(super) async fn cancel(
    State(state): State<RestState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let job = state.batches.get(&id)?;
    if job.report(false).finished_at.is_none() {
        job.cancel.cancel();
    }
    Ok(ok(job.report(true)))
}

async fn run_job<F, Fut>(
    job: Arc<BatchJob>,
    inputs: Vec<PreparedCreate>,
    concurrency: usize,
    slots: Arc<Semaphore>,
    shutdown: CancellationToken,
    execute: F,
) where
    F: Fn(PreparedCreate) -> Fut + Send + Sync,
    Fut: Future<Output = ApiResult<Value>> + Send,
{
    job.progress
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .started_at = Some(crate::meter::now_secs());
    tracing::info!(event="admin.account_batch.started",batch_id=%job.id,total=inputs.len(),concurrency,"account batch started");
    let work = stream::iter(inputs.into_iter().enumerate().map(|(index, input)| {
        let job = Arc::clone(&job);
        let slots = Arc::clone(&slots);
        let shutdown = shutdown.clone();
        let execute = &execute;
        async move {
            let permit = tokio::select! {
                biased;
                _ = job.cancel.cancelled() => None,
                _ = shutdown.cancelled() => None,
                permit = slots.acquire_owned() => permit.ok(),
            };
            let Some(_permit) = permit else {
                job.row(index, |row| {
                    row.status = RowStatus::Cancelled;
                    row.finished_at = Some(crate::meter::now_secs());
                });
                return;
            };
            if job.cancel.is_cancelled() || shutdown.is_cancelled() {
                job.row(index, |row| {
                    row.status = RowStatus::Cancelled;
                    row.finished_at = Some(crate::meter::now_secs());
                });
                return;
            }
            job.row(index, |row| {
                row.status = RowStatus::Running;
                row.started_at = Some(crate::meter::now_secs());
            });
            let secrets = input.secrets();
            // Cancellation drains existing logins instead of abandoning browser cleanup or a durable write.
            let outcome = AssertUnwindSafe(async { execute(input).await })
                .catch_unwind()
                .await
                .unwrap_or_else(|_| Err(ApiError::internal()));
            job.row(index, |row| {
                row.finished_at = Some(crate::meter::now_secs());
                match outcome {
                    Ok(account) => {
                        row.status = RowStatus::Succeeded;
                        row.account_id = account["id"].as_str().map(str::to_owned);
                        row.reference = account["reference"].as_str().map(str::to_owned);
                        if let Some(name) = account["display_name"].as_str() {
                            row.display_name = Some(name.to_owned());
                        }
                    }
                    Err(error) => {
                        row.status = RowStatus::Failed;
                        let error = accounts::protect_error(error, &secrets);
                        row.error = Some(RowError {
                            code: error.code,
                            message: error.message,
                        });
                    }
                }
            });
        }
    }))
    .buffer_unordered(concurrency);
    work.collect::<Vec<_>>().await;
    job.progress
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .finished_at = Some(crate::meter::now_secs());
    let report = job.report(false);
    tracing::info!(event="admin.account_batch.completed",batch_id=%job.id,status=report.status,succeeded=report.succeeded,failed=report.failed,cancelled=report.cancelled,"account batch completed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crosslane_ipc::protocol::AccountAddSsoParams;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::Notify;

    fn sso_inputs() -> Vec<PreparedCreate> {
        crosslane_core::account_csv::parse_kiro_sso_csv(
            "\u{feff}email,password\r\none@example.com,\" fixture,\"\"password \"\r\nbad@example.com,second-private-password\r\nthree@example.com,third-private-password\r\n"
        ).unwrap().into_iter().map(|(email,password)| PreparedCreate::Sso(AccountAddSsoParams {
            email,password,start_url:"https://example.awsapps.com/start".into(),region:"us-west-2".into(),headful:false,tags:vec!["team-a".into()]
        })).collect()
    }

    #[tokio::test]
    async fn sso_batch_preserves_cli_credentials_limits_concurrency_and_continues_after_failure() {
        let inputs = sso_inputs();
        let job = Arc::new(BatchJob::new(&inputs));
        let current = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let received = Arc::new(Mutex::new(Vec::new()));
        run_job(Arc::clone(&job),inputs,2,Arc::new(Semaphore::new(8)),CancellationToken::new(),|input| {
            let current=Arc::clone(&current);
            let peak=Arc::clone(&peak);
            let received=Arc::clone(&received);
            async move {
                let PreparedCreate::Sso(params)=input else {panic!("expected SSO login")};
                assert_eq!(params.region,"us-west-2");
                assert_eq!(params.start_url,"https://example.awsapps.com/start");
                assert_eq!(params.tags,vec!["team-a"]);
                assert!(!params.headful);
                received.lock().unwrap().push(serde_json::to_value(&params).unwrap());
                peak.fetch_max(current.fetch_add(1,Ordering::SeqCst)+1,Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(if params.email.starts_with("one") {30} else {5})).await;
                current.fetch_sub(1,Ordering::SeqCst);
                if params.email.starts_with("bad") {
                    Err(ApiError::new(StatusCode::BAD_GATEWAY,"upstream_failed",format!("login rejected {}",params.password)))
                } else {
                    Ok(json!({"id":"acc_12345678","reference":"kiro/acc_12345678","display_name":params.email,
                        "password":params.password,"access_token":"must-not-retain-private-token"}))
                }
            }
        }).await;
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        let report = job.report(true);
        assert_eq!(report.status, "completed_with_errors");
        assert_eq!(
            (
                report.total,
                report.completed,
                report.succeeded,
                report.failed
            ),
            (3, 3, 2, 1)
        );
        assert!(!report.complete);
        let rows = report.rows.as_ref().unwrap();
        assert_eq!(rows[0].display_name.as_deref(), Some("one@example.com"));
        assert!(matches!(rows[1].status, RowStatus::Failed));
        assert!(matches!(rows[2].status, RowStatus::Succeeded));
        assert!(received
            .lock()
            .unwrap()
            .iter()
            .any(|params| params["password"] == " fixture,\"password "));
        let output = serde_json::to_string(&report).unwrap();
        for secret in [
            "fixture,",
            "second-private-password",
            "third-private-password",
            "must-not-retain-private-token",
        ] {
            assert!(!output.contains(secret));
        }
        assert!(output.contains("login rejected"));
    }

    #[tokio::test]
    async fn cancellation_stops_queued_logins_and_drains_an_in_flight_login() {
        let inputs = sso_inputs();
        let job = Arc::new(BatchJob::new(&inputs));
        let started = Arc::new(Notify::new());
        let finish = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let task = {
            let job = Arc::clone(&job);
            let started = Arc::clone(&started);
            let finish = Arc::clone(&finish);
            let calls = Arc::clone(&calls);
            tokio::spawn(async move {
                run_job(
                    job,
                    inputs,
                    1,
                    Arc::new(Semaphore::new(8)),
                    CancellationToken::new(),
                    move |_| {
                        let started = Arc::clone(&started);
                        let finish = Arc::clone(&finish);
                        let calls = Arc::clone(&calls);
                        async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            started.notify_one();
                            finish.notified().await;
                            Ok(json!({"id":"acc_12345678","reference":"kiro/acc_12345678"}))
                        }
                    },
                )
                .await;
            })
        };
        started.notified().await;
        assert_eq!(job.report(false).running, 1);
        job.cancel.cancel();
        assert_eq!(job.report(false).status, "cancelling");
        finish.notify_one();
        task.await.unwrap();
        let report = job.report(true);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            (report.succeeded, report.cancelled, report.completed),
            (1, 2, 3)
        );
        assert_eq!(report.status, "cancelled");
    }

    #[tokio::test]
    async fn different_batches_share_the_global_account_creation_limit() {
        let first = sso_inputs();
        let second = sso_inputs();
        let first_job = Arc::new(BatchJob::new(&first));
        let second_job = Arc::new(BatchJob::new(&second));
        let current = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let execute = |_| {
            let current = Arc::clone(&current);
            let peak = Arc::clone(&peak);
            async move {
                peak.fetch_max(current.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
                current.fetch_sub(1, Ordering::SeqCst);
                Ok(json!({"id":"acc_12345678","reference":"kiro/acc_12345678"}))
            }
        };
        let slots = Arc::new(Semaphore::new(2));
        tokio::join!(
            run_job(
                Arc::clone(&first_job),
                first,
                8,
                Arc::clone(&slots),
                CancellationToken::new(),
                &execute
            ),
            run_job(
                Arc::clone(&second_job),
                second,
                8,
                slots,
                CancellationToken::new(),
                &execute
            )
        );
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        assert!(first_job.report(false).complete && second_job.report(false).complete);
    }

    #[test]
    fn registry_limits_active_jobs_and_expires_finished_reports() {
        let registry = BatchRegistry::default();
        let inputs = sso_inputs();
        let mut permits = Vec::new();
        for _ in 0..4 {
            permits.push(registry.insert(&inputs).unwrap().1);
        }
        assert_eq!(
            registry.insert(&inputs).err().unwrap().status,
            StatusCode::SERVICE_UNAVAILABLE
        );
        permits.pop();
        let (job, _permit) = registry.insert(&inputs).unwrap();
        assert!(registry.get(&job.id).is_ok());
        job.progress.lock().unwrap().finished_at =
            Some(crate::meter::now_secs() - RETENTION_SECS - 1);
        assert_eq!(
            registry.get(&job.id).err().unwrap().status,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn oversized_login_defaults_are_rejected_before_expanding_a_full_batch() {
        let directory = tempfile::tempdir().unwrap();
        let paths = crosslane_core::paths::Paths::from_env_values(
            Some(directory.path().to_str().unwrap()),
            None,
            None,
            None,
        );
        crosslane_store::bootstrap::ensure_layout(&paths)
            .await
            .unwrap();
        let store = crosslane_store::accounts::AccountStore::load(&paths.accounts_file)
            .await
            .unwrap();
        let app = Arc::new(AppState::new(
            paths,
            crosslane_store::config_loader::ConfigHandle::new(Default::default()),
            store,
        ));
        for (field, value) in [
            ("region", "x".repeat(65)),
            (
                "start_url",
                format!("https://example.invalid/{}", "x".repeat(8192)),
            ),
        ] {
            let mut input = json!({
                "provider":"kiro","auth":"sso","start_url":"https://example.invalid/start",
                "accounts":vec![json!({"email":"batch@example.invalid","password":"fixture-password"});1000]
            });
            input[field] = json!(value);
            let error = prepare(&app, serde_json::from_value(input).unwrap())
                .err()
                .expect("unbounded shared fields must be rejected");
            assert_eq!(error.status, StatusCode::BAD_REQUEST);
            assert!(error.message.contains(field));
            assert!(app.with_accounts(|store| store.is_empty()));
        }
        app.shutdown.cancel();
    }
}
