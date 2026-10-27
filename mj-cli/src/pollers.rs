//! Terminal-specific adapters for controller background feeds.
use crate::dashboard::io::{DashboardIoUpdate, report};
use mj_controller::controller::Controller;
pub(crate) use mj_controller::pollers::*;
use mj_controller::session_manager::ViewError;
use mj_controller::targets::CancellableProcessExecutor;
use mj_core::state::SessionState;
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
/// Applies one worker poll's reachability to the dashboard. `persistence` is
/// where a missing target is recorded; `None` records nothing. Returns the
/// poll's projection, moved out of the update rather than copied, when it
/// carried one. The session's name is not taken from the poll: the daemon
/// projects it into the record, and the record reaches the dashboard with
/// the daemon's other record changes.
pub(crate) fn apply_worker_poll_update(
    controller: &Controller,
    dashboard: &mut DashboardState,
    update: WorkerPollUpdate,
    persistence: Option<(
        &tokio::sync::mpsc::UnboundedSender<DashboardIoUpdate>,
        &crate::dashboard::CriticalOperationTracker,
    )>,
) -> Option<mj_core::state::MaterializedSession> {
    match update.view.error {
        Some(ViewError::Unreachable(detail)) => {
            dashboard.mark_transcript_unavailable(&update.session_id);
            tracing::warn!(session_id = %update.session_id, "relay unreachable: {detail}");
            dashboard.report_session_unreachable(&update.session_id, true);
        }
        Some(ViewError::TargetMissing(detail)) => {
            dashboard.mark_transcript_unavailable(&update.session_id);
            dashboard.set_notice(format!(
                "Session {}: {detail}; recording the missing target…",
                &update.session_id[..update.session_id.len().min(8)]
            ));
            if controller
                .state
                .sessions
                .get(&update.session_id)
                .is_some_and(|session| {
                    matches!(
                        session.state,
                        SessionState::Provisioning
                            | SessionState::Running
                            | SessionState::Disconnected
                            | SessionState::Error
                    )
                })
                && let Some((dashboard_io_tx, tracker)) = persistence
            {
                spawn_worker_record_persistence(
                    update.session_id.clone(),
                    WorkerRecordPersistence::TargetMissing {
                        session_id: update.session_id.clone(),
                        detail,
                        updated_at: chrono::Utc::now()
                            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    },
                    dashboard_io_tx.clone(),
                    tracker.clone(),
                );
            }
        }
        Some(ViewError::ProjectionIntegrity(detail)) => {
            // Deterministic failure: no worker diagnostics. Like an
            // unreachable relay, it is a live poll fact and is never
            // persisted as a session last_error.
            dashboard.mark_transcript_unavailable(&update.session_id);
            dashboard.set_notice(format!(
                "Session {}: transcript projection failed: {detail}",
                &update.session_id[..update.session_id.len().min(8)]
            ));
        }
        None => dashboard.report_session_reachable(&update.session_id),
    }
    update.view.snapshot.map(|snapshot| snapshot.materialized)
}

#[derive(Debug)]
pub(crate) enum WorkerRecordPersistence {
    TargetMissing {
        session_id: String,
        detail: String,
        updated_at: String,
    },
}

#[derive(Debug)]
pub(crate) enum WorkerRecordPersistenceOutcome {
    TargetMissing(SessionState),
    Unchanged,
}

fn spawn_worker_record_persistence(
    session_id: String,
    operation: WorkerRecordPersistence,
    updates: tokio::sync::mpsc::UnboundedSender<DashboardIoUpdate>,
    tracker: crate::dashboard::CriticalOperationTracker,
) {
    let guard = tracker.begin(format!(
        "saving missing target for {}",
        crate::short_id(&session_id)
    ));
    tokio::spawn(async move {
        let result = async {
            let mut daemon = daemon::connect_or_start().await?;
            match &operation {
                WorkerRecordPersistence::TargetMissing {
                    session_id,
                    detail,
                    updated_at,
                } => daemon
                    .mark_session_target_missing(
                        session_id.clone(),
                        detail.clone(),
                        updated_at.clone(),
                    )
                    .await
                    .map(|state| {
                        state.map_or(
                            WorkerRecordPersistenceOutcome::Unchanged,
                            WorkerRecordPersistenceOutcome::TargetMissing,
                        )
                    }),
            }
        }
        .await
        .map_err(|error| format!("{error:#}"));
        report(
            "persisting a worker record",
            &updates,
            DashboardIoUpdate::WorkerRecordPersistence { operation, result },
        );
        drop(guard);
    });
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

use crate::daemon;
