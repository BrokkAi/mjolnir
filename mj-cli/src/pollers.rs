//! Terminal-specific adapters for controller background feeds.
use crate::dashboard::io::{DashboardIoUpdate, report};
use mj_controller::controller::Controller;
pub(crate) use mj_controller::pollers::*;
use mj_controller::session_manager::ViewError;
use mj_controller::targets::CancellableProcessExecutor;
use mj_tui::DashboardState;
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Duration;
/// Completion identity belongs to the UI operation that started the work.
/// The daemon's lifecycle projection remains the authority for session state.
pub(crate) struct DashboardLifecycleUpdate {
    pub(crate) session_id: String,
    pub(crate) result: std::result::Result<LifecycleSuccess, String>,
    pub(crate) operation: Arc<AtomicBool>,
}

const WORKER_DIAGNOSIS_TIMEOUT: Duration = Duration::from_secs(15);
/// Applies one worker poll's reachability to the dashboard. The daemon owns
/// durable missing-target findings and publishes their notice. Returns the
/// poll's projection, moved out of the update rather than copied, when it
/// carried one.
pub(crate) fn apply_worker_poll_update(
    dashboard: &mut DashboardState,
    update: WorkerPollUpdate,
) -> Option<crate::dashboard::MaterializedProjection> {
    match update.view.error {
        Some(ViewError::Unreachable(detail)) => {
            dashboard.mark_transcript_unavailable(&update.session_id);
            tracing::warn!(session_id = %update.session_id, "relay unreachable: {detail}");
            dashboard.report_session_unreachable(&update.session_id);
        }
        Some(ViewError::TargetMissing(_)) => {
            dashboard.mark_transcript_unavailable(&update.session_id);
            dashboard.clear_session_unreachable(&update.session_id);
        }
        Some(ViewError::ProjectionIntegrity(detail)) => {
            // Deterministic failure: no worker diagnostics. Like an
            // unreachable relay, it is a live poll fact and is never
            // persisted as a session last_error.
            dashboard.mark_transcript_unavailable(&update.session_id);
            dashboard.clear_session_unreachable(&update.session_id);
            dashboard.set_notice(format!(
                "Session {}: transcript projection failed: {detail}",
                &update.session_id[..update.session_id.len().min(8)]
            ));
        }
        None => dashboard.report_session_reachable(&update.session_id),
    }
    update.view.snapshot.map(Into::into)
}

pub(crate) fn spawn_worker_diagnosis(
    controller: &Controller,
    session_id: String,
    episode_id: u64,
    updates: tokio::sync::mpsc::UnboundedSender<DashboardIoUpdate>,
    tracker: crate::dashboard::CriticalOperationTracker,
) {
    let cancelled = Arc::new(AtomicBool::new(false));
    let guard = tracker.begin_cancellable(
        format!("diagnosing session {}", crate::short_id(&session_id)),
        cancelled.clone(),
    );
    let diagnostic_controller = Controller {
        config: controller.config.clone(),
        state: controller.state.clone(),
    };
    tokio::spawn(async move {
        let task_session_id = session_id.clone();
        let joined = tokio::task::spawn_blocking(move || {
            let executor =
                CancellableProcessExecutor::new(cancelled).with_deadline(WORKER_DIAGNOSIS_TIMEOUT);
            diagnostic_controller.diagnose_worker_controlled(&task_session_id, &executor)
        })
        .await;
        let result = joined.map_err(|error| format!("worker diagnosis task failed: {error}"));
        report(
            "diagnosing a worker",
            &updates,
            DashboardIoUpdate::WorkerDiagnosis {
                session_id,
                episode_id,
                result,
            },
        );
        drop(guard);
    });
}
