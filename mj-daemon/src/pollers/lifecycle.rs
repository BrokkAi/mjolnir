use super::*;

pub use mj_controller::controller::interrupted_lifecycle_cause;

pub enum LifecycleSuccess {
    Created,
    Resumed {
        profile_id: String,
        target_id: String,
    },
    Moved(mj_core::state::MoveOutcome),
    Closed,
    ForceStopped,
    DestroyedStopped,
    ForceDestroyed,
}

pub struct LifecycleUpdate {
    pub session_id: String,
    pub result: std::result::Result<LifecycleSuccess, String>,
    pub deferred_cleanup: bool,
}

/// Whether a close stopped partway and left the record mid-close with its
/// target still present. Such a record cannot be closed again from the start:
/// its worker is gone, so only recovery can finish it.
pub fn is_interrupted_close(session: &SessionRecord) -> bool {
    matches!(
        session.state,
        SessionState::Closing | SessionState::Destroying
    ) && session.target.is_some()
}

pub fn interrupted_suspend_session_ids(controller: &Controller) -> Vec<String> {
    controller
        .state
        .sessions
        .values()
        .filter(|session| is_interrupted_close(session))
        .map(|session| session.id.clone())
        .collect()
}

/// Sessions whose destroy failed and stopped partway: `Error`, still holding
/// their target, with the destruction failure recorded by
/// `record_lifecycle_failure`. The person asked for them to be destroyed, so
/// startup finishes the removal (the next `mj destroy` does the same).
pub fn interrupted_destroy_session_ids(controller: &Controller) -> Vec<String> {
    controller
        .state
        .sessions
        .values()
        .filter(|session| {
            session.state == SessionState::Error
                && session.target.is_some()
                && session.last_error.as_deref().is_some_and(|error| {
                    error.starts_with(mj_core::state::DESTRUCTION_FAILURE_PREFIX)
                })
        })
        .map(|session| session.id.clone())
        .collect()
}

/// Every session whose in-flight lifecycle state has no owner, with the cause
/// to record against it. `owned` names the sessions a durable move intent or
/// another startup recovery has already claimed.
pub fn unowned_interrupted_lifecycles(
    controller: &Controller,
    owned: &std::collections::BTreeSet<String>,
) -> Vec<(String, String)> {
    controller
        .state
        .sessions
        .values()
        .filter(|session| !owned.contains(&session.id) && !is_interrupted_close(session))
        .filter_map(|session| {
            interrupted_lifecycle_cause(session).map(|cause| (session.id.clone(), cause))
        })
        .collect()
}

pub fn project_worker_title(
    controller: &mut Controller,
    update: &WorkerPollUpdate,
) -> Option<Option<String>> {
    let snapshot = update.view.snapshot.as_ref()?;
    let session = controller.state.sessions.get_mut(&update.session_id)?;
    let title = snapshot.resolved_title();
    if session.acp_session_title == title {
        return None;
    }
    session.acp_session_title = title.clone();
    Some(title)
}

pub fn apply_worker_record_update(controller: &mut Controller, update: &WorkerPollUpdate) {
    let Some(title) = project_worker_title(controller, update) else {
        return;
    };
    let session_id = update.session_id.clone();
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            mj_controller::database::set_session_acp_title(&session_id, title.as_deref())
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "could not persist relay title"),
            Err(error) => tracing::warn!(%error, "relay title persistence task failed"),
        }
    });
}
