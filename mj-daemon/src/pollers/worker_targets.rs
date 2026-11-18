use super::*;

pub fn dashboard_worker_targets(controller: &Controller) -> Vec<WorkerPollTarget> {
    controller
        .state
        .sessions
        .values()
        .filter(|session| session_target_is_pollable(session))
        .filter_map(|session| {
            worker_poll_target(
                controller,
                session,
                mj_controller::database::load_move_operation(&session.id),
            )
        })
        .collect()
}

pub(crate) fn worker_poll_target(
    controller: &Controller,
    session: &SessionRecord,
    operation: Result<Option<mj_core::state::MoveOperation>>,
) -> Option<WorkerPollTarget> {
    let spec = match controller.reconnect_command(&session.id) {
        Ok(spec) => spec,
        Err(error) => {
            tracing::warn!(session_id = %session.id, "could not build worker poll target: {error:#}");
            return None;
        }
    };
    Some(WorkerPollTarget {
        session_id: session.id.clone(),
        spec,
        worker_recovery: match operation
            .and_then(|operation| controller.worker_recovery_plan(&session.id, operation.as_ref()))
        {
            Ok(plan) => Some(plan),
            Err(error) => {
                tracing::debug!(session_id = %session.id, "worker recovery target unavailable: {error:#}");
                None
            }
        },
        project_memory: match controller.project_memory_sync_target(&session.id) {
            Ok(target) => target,
            Err(error) => {
                tracing::debug!(session_id = %session.id, "project memory target unavailable: {error:#}");
                None
            }
        },
    })
}

pub fn dashboard_worker_targets_excluding(
    controller: &Controller,
    excluded_sessions: &std::collections::BTreeSet<String>,
) -> Vec<WorkerPollTarget> {
    let mut targets = dashboard_worker_targets(controller);
    targets.retain(|target| !excluded_sessions.contains(&target.session_id));
    targets
}

#[cfg(test)]
pub(crate) use mj_controller::worker_client::target_syncs_github_token;
pub use mj_controller::worker_client::{
    credential_sync_target_is_current, credential_sync_targets,
};
pub(crate) use mj_controller::worker_client::{
    credential_sync_targets_from_sources, staged_credential_sessions,
};
