//! Terminal-specific adapters for controller background feeds.
use crate::dashboard::io::{DashboardIoUpdate, report};
use anyhow::Result;
use mj_controller::controller::Controller;
pub(crate) use mj_controller::pollers::*;
use mj_controller::session_manager::ViewError;
use mj_controller::targets::CancellableProcessExecutor;
use mj_core::state::SessionState;
use mj_tui::DashboardState;
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Duration;
const WORKER_DIAGNOSIS_TIMEOUT: Duration = Duration::from_secs(15);
/// Applies one worker poll to the controller's records, the dashboard, and
/// the open conversations. `persistence` is where the records it changes are
/// saved; `None` saves nothing.
pub(crate) fn apply_worker_poll_update(
    controller: &mut Controller,
    dashboard: &mut DashboardState,
    chats: &mut std::collections::BTreeMap<String, mj_chat::chat::ActiveChat>,
    update: WorkerPollUpdate,
    persistence: Option<(
        &tokio::sync::mpsc::UnboundedSender<DashboardIoUpdate>,
        &crate::dashboard::CriticalOperationTracker,
    )>,
) -> Result<bool> {
    if apply_worker_record_update(controller, &update, persistence)? {
        dashboard.set_state(controller.state.clone());
        // The Sessions row now reads the new title from the record. The open
        // conversation's header must read it too, now: the daemon's later
        // copy of the record is identical, so it refreshes nothing, and the
        // header used to wait for the turn's end to change the record again
        // (launch finding R5-7).
        crate::dashboard::refresh_open_chats(chats, controller, dashboard);
    }
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
    Ok(update.view.snapshot.is_some())
}

fn apply_worker_record_update(
    controller: &mut Controller,
    update: &WorkerPollUpdate,
    dashboard_io: Option<(
        &tokio::sync::mpsc::UnboundedSender<DashboardIoUpdate>,
        &crate::dashboard::CriticalOperationTracker,
    )>,
) -> Result<bool> {
    let Some(title) = project_worker_title(controller, update) else {
        return Ok(false);
    };
    if let Some((updates, tracker)) = dashboard_io {
        spawn_worker_record_persistence(
            update.session_id.clone(),
            WorkerRecordPersistence::AcpTitle { title },
            updates.clone(),
            tracker.clone(),
        );
    }
    Ok(true)
}

#[derive(Debug)]
pub(crate) enum WorkerRecordPersistence {
    AcpTitle {
        title: Option<String>,
    },
    TargetMissing {
        session_id: String,
        detail: String,
        updated_at: String,
    },
}

#[derive(Debug)]
pub(crate) enum WorkerRecordPersistenceOutcome {
    Saved,
    TargetMissing(SessionState),
    Unchanged,
}

fn spawn_worker_record_persistence(
    session_id: String,
    operation: WorkerRecordPersistence,
    updates: tokio::sync::mpsc::UnboundedSender<DashboardIoUpdate>,
    tracker: crate::dashboard::CriticalOperationTracker,
) {
    let label = match &operation {
        WorkerRecordPersistence::AcpTitle { .. } => "saving agent title for",
        WorkerRecordPersistence::TargetMissing { .. } => "saving missing target for",
    };
    let guard = tracker.begin(format!("{label} {}", crate::short_id(&session_id)));
    tokio::spawn(async move {
        let result = async {
            let mut daemon = daemon::connect_or_start().await?;
            match &operation {
                WorkerRecordPersistence::AcpTitle { title } => daemon
                    .set_session_acp_title(session_id, title.clone())
                    .await
                    .map(|()| WorkerRecordPersistenceOutcome::Saved),
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
