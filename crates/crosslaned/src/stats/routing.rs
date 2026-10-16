//! Accounting scoped to one automatic provider candidate, never to a client-supplied trace ID.

use std::future::Future;
use std::sync::{Arc, Mutex};

use super::{lock, RequestLog, StatsStore};

tokio::task_local! {
    static CURRENT_ATTEMPT: Arc<RequestStatsAttempt>;
}

#[derive(Default)]
enum Decision {
    #[default]
    Candidate,
    Selected,
    Rejected,
}

#[derive(Default)]
struct Recording {
    decision: Decision,
    // Each route produces one client log. Keep only that row while preflight
    // decides whether it belongs to the eventual client response.
    buffered: Option<RequestLog>,
}

pub(crate) struct RequestStatsAttempt {
    stats: Arc<StatsStore>,
    trace_id: String,
    path: String,
    recording: Mutex<Recording>,
}

impl RequestStatsAttempt {
    pub(crate) fn new(stats: Arc<StatsStore>, trace_id: &str, path: &str) -> Arc<Self> {
        Arc::new(Self {
            stats,
            trace_id: trace_id.into(),
            path: path.into(),
            recording: Mutex::new(Recording::default()),
        })
    }

    pub(crate) fn current() -> Option<Arc<Self>> {
        CURRENT_ATTEMPT.try_with(Arc::clone).ok()
    }

    pub(crate) fn scope<F: Future>(
        attempt: Option<Arc<Self>>,
        future: F,
    ) -> impl Future<Output = F::Output> {
        // Dispatch futures include the full translation/compaction pipeline.
        // Box before building either branch so adding task-local accounting
        // does not duplicate that state machine in each caller's stack frame.
        let future = Box::pin(future);
        async move {
            if let Some(attempt) = attempt {
                CURRENT_ATTEMPT.scope(attempt, future).await
            } else {
                future.await
            }
        }
    }

    pub(crate) fn matches(&self, stats: &StatsStore, request: &RequestLog) -> bool {
        self.matches_request(stats, &request.trace_id, &request.path)
    }

    pub(crate) fn matches_request(&self, stats: &StatsStore, trace_id: &str, path: &str) -> bool {
        std::ptr::eq(Arc::as_ptr(&self.stats), stats)
            && trace_id == self.trace_id
            && path == self.path
    }

    pub(crate) fn record(&self, request: RequestLog) {
        let mut recording = lock(&self.recording);
        let count_request = match recording.decision {
            Decision::Candidate if recording.buffered.is_none() => {
                recording.buffered = Some(request);
                return;
            }
            Decision::Selected => true,
            // Keep attribution even if an unexpected second producer emits a
            // row while preflight is pending; it must not add a client request.
            Decision::Candidate | Decision::Rejected => false,
        };
        drop(recording);
        self.stats.record_direct(request, count_request);
    }

    pub(crate) fn select(&self) {
        self.resolve(true);
    }

    pub(crate) fn reject(&self) {
        self.resolve(false);
    }

    fn resolve(&self, count_request: bool) {
        let mut recording = lock(&self.recording);
        if !matches!(recording.decision, Decision::Candidate) {
            return;
        }
        recording.decision = if count_request {
            Decision::Selected
        } else {
            Decision::Rejected
        };
        let buffered = recording.buffered.take();
        drop(recording);
        if let Some(request) = buffered {
            self.stats.record_direct(request, count_request);
        }
    }
}

/// Dropping a route future is a cancellation, not a rejected failover attempt.
/// A detached stream finalizer still owns its attempt and can record its 499.
pub(crate) struct CandidateStatsGuard(pub(crate) Arc<RequestStatsAttempt>);

impl Drop for CandidateStatsGuard {
    fn drop(&mut self) {
        self.0.select();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::tests::request;

    fn entry(provider: &str, status: u16) -> RequestLog {
        let mut log = request(provider, 120, status);
        log.trace_id = "same-client-trace".into();
        log.provider_id = provider.into();
        log.account_id = format!("{provider}/account");
        log.endpoint = format!("{provider}-endpoint");
        log.model = format!("{provider}-model");
        log
    }

    fn attempt(stats: &Arc<StatsStore>) -> Arc<RequestStatsAttempt> {
        RequestStatsAttempt::new(Arc::clone(stats), "same-client-trace", "/v1/messages")
    }

    #[tokio::test]
    async fn rejected_attempt_preserves_usage_dimensions_without_counting_a_client_request() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stats.json");
        let stats = Arc::new(StatsStore::empty(&path));
        let first = attempt(&stats);
        let mut failed = entry("kiro", 502);
        failed.input_tokens = 31;
        failed.output_tokens = 4;
        failed.credits = 0.125;
        RequestStatsAttempt::scope(Some(Arc::clone(&first)), async { stats.record(failed) }).await;
        assert_eq!(
            stats.snapshot(None).total.input_tokens,
            0,
            "preflight buffers its one candidate row"
        );
        first.reject();
        let second = attempt(&stats);
        second.select();
        let mut success = entry("copilot", 200);
        success.input_tokens = 8;
        success.output_tokens = 2;
        success.credits = 0.0;
        RequestStatsAttempt::scope(Some(second), async { stats.record(success) }).await;

        for snapshot in [
            stats.snapshot(None),
            stats.window(Some(120), Some(179)).await.unwrap(),
        ] {
            assert_eq!(snapshot.total.requests, 1);
            assert_eq!(snapshot.total.successes, 1);
            assert_eq!(snapshot.total.failures, 0);
            assert_eq!(snapshot.total.input_tokens, 39);
            assert_eq!(snapshot.total.output_tokens, 6);
            assert_eq!(snapshot.total.credits, 0.125);
            assert_eq!(snapshot.total.duration_samples, 1);
            assert_eq!(snapshot.by_provider["kiro"].requests, 0);
            assert_eq!(snapshot.by_provider["kiro"].input_tokens, 31);
            assert_eq!(snapshot.by_account["kiro/account"].credits, 0.125);
            assert_eq!(snapshot.by_endpoint["kiro-endpoint"].output_tokens, 4);
            assert_eq!(snapshot.by_model["kiro-model"].input_tokens, 31);
            assert_eq!(
                snapshot.provider_details["kiro"].by_account["kiro/account"].input_tokens,
                31
            );
            assert_eq!(snapshot.latencies_ms.len(), 1);
            assert_eq!(snapshot.recent_requests.len(), 1);
            assert_eq!(snapshot.recent_requests[0].provider_id, "copilot");
        }
        let session = stats.session_snapshot();
        assert_eq!(session.total.input_tokens, 39);
        assert_eq!(session.by_provider["kiro"].credits, 0.125);
        stats.persist().await.unwrap();
        let reloaded = StatsStore::load(&path).await.unwrap();
        let snapshot = reloaded.window(Some(120), Some(179)).await.unwrap();
        assert_eq!(snapshot.total.requests, 1);
        assert_eq!(snapshot.by_provider["kiro"].input_tokens, 31);
        assert_eq!(snapshot.by_provider["kiro"].credits, 0.125);
        assert_eq!(reloaded.snapshot(None).total.output_tokens, 6);
    }

    #[tokio::test]
    async fn all_rejected_attempts_leave_one_terminal_client_failure() {
        let directory = tempfile::tempdir().unwrap();
        let stats = Arc::new(StatsStore::empty(&directory.path().join("stats.json")));
        for provider in ["kiro", "copilot"] {
            let candidate = attempt(&stats);
            // Native finalization can arrive after preflight has rejected it.
            candidate.reject();
            RequestStatsAttempt::scope(Some(candidate), async {
                stats.record(entry(provider, 502));
            })
            .await;
        }
        let mut terminal = entry("copilot", 503);
        terminal.input_tokens = 0;
        terminal.output_tokens = 0;
        terminal.credits = 0.0;
        stats.record(terminal);
        let snapshot = stats.snapshot(None);
        assert_eq!(snapshot.total.requests, 1);
        assert_eq!(snapshot.total.failures, 1);
        assert_eq!(snapshot.total.successes, 0);
        assert_eq!(snapshot.total.input_tokens, 2);
        assert_eq!(snapshot.total.output_tokens, 2);
        assert_eq!(snapshot.total.credits, 0.2);
        assert_eq!(snapshot.recent_requests.len(), 1);
        assert_eq!(snapshot.recent_requests[0].status, 503);
    }

    #[tokio::test]
    async fn cancelling_a_pending_candidate_keeps_its_late_finalizer_accounting() {
        let directory = tempfile::tempdir().unwrap();
        let stats = Arc::new(StatsStore::empty(&directory.path().join("stats.json")));
        let candidate = attempt(&stats);
        let guard = CandidateStatsGuard(Arc::clone(&candidate));
        drop(guard);
        RequestStatsAttempt::scope(Some(candidate), async {
            stats.record(entry("copilot", 499));
        })
        .await;
        let snapshot = stats.snapshot(None);
        assert_eq!(snapshot.total.requests, 1);
        assert_eq!(snapshot.total.failures, 1);
        assert_eq!(snapshot.recent_requests[0].status, 499);
    }

    #[tokio::test]
    async fn concurrent_requests_with_identical_trace_ids_keep_separate_decisions() {
        let directory = tempfile::tempdir().unwrap();
        let stats = Arc::new(StatsStore::empty(&directory.path().join("stats.json")));
        let first = attempt(&stats);
        let second = attempt(&stats);
        first.reject();
        second.select();
        let left = RequestStatsAttempt::scope(Some(first), async {
            tokio::task::yield_now().await;
            stats.record(entry("kiro", 502));
        });
        let right = RequestStatsAttempt::scope(Some(second), async {
            stats.record(entry("copilot", 200));
            tokio::task::yield_now().await;
        });
        tokio::join!(left, right);
        assert!(RequestStatsAttempt::current().is_none());
        let snapshot = stats.snapshot(None);
        assert_eq!(snapshot.total.requests, 1);
        assert_eq!(snapshot.total.input_tokens, 2);
        assert_eq!(snapshot.recent_requests[0].provider_id, "copilot");
        // The first request's external terminal failure remains independent of
        // the concurrently successful request, even with the same trace ID.
        let mut failure = entry("kiro", 502);
        failure.input_tokens = 0;
        failure.output_tokens = 0;
        failure.credits = 0.0;
        stats.record(failure);
        assert_eq!(stats.snapshot(None).total.requests, 2);
    }

    #[tokio::test]
    async fn internal_compaction_and_other_store_records_bypass_the_client_scope() {
        let directory = tempfile::tempdir().unwrap();
        let stats = Arc::new(StatsStore::empty(&directory.path().join("stats.json")));
        let other = StatsStore::empty(&directory.path().join("other-stats.json"));
        let candidate = attempt(&stats);
        candidate.reject();
        RequestStatsAttempt::scope(Some(candidate), async {
            let mut compaction = entry("kiro", 200);
            compaction.path = "/internal/compaction".into();
            stats.record(compaction);
            let mut unrelated = entry("kiro", 200);
            unrelated.trace_id = "another-trace".into();
            stats.record(unrelated);
            other.record(entry("kiro", 200));
        })
        .await;
        assert_eq!(stats.snapshot(None).total.requests, 2);
        assert_eq!(other.snapshot(None).total.requests, 1);
    }

    #[tokio::test]
    async fn accepting_a_buffered_full_response_records_it_exactly_once() {
        let directory = tempfile::tempdir().unwrap();
        let stats = Arc::new(StatsStore::empty(&directory.path().join("stats.json")));
        let candidate = attempt(&stats);
        RequestStatsAttempt::scope(Some(Arc::clone(&candidate)), async {
            let mut log = entry("kiro", 200);
            log.path = "/v1/messages/count_tokens".into();
            // Count-token paths use their own exact route scope too.
            let token_scope =
                RequestStatsAttempt::new(Arc::clone(&stats), &log.trace_id, &log.path);
            RequestStatsAttempt::scope(Some(Arc::clone(&token_scope)), async {
                stats.record(log);
            })
            .await;
            token_scope.select();
            token_scope.reject();
            token_scope.select();
        })
        .await;
        candidate.reject();
        assert_eq!(stats.snapshot(None).total.requests, 1);
        assert_eq!(stats.snapshot(None).total.input_tokens, 1);
        assert_eq!(
            stats.snapshot(None).recent_requests[0].path,
            "/v1/messages/count_tokens"
        );
    }
}
