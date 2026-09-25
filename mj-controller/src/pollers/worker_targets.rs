use super::*;

pub fn dashboard_worker_targets(controller: &Controller) -> Vec<WorkerPollTarget> {
    controller
        .state
        .sessions
        .values()
        .filter(|session| session_target_is_pollable(session))
        .filter_map(|session| {
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
                worker_recovery: match controller.worker_recovery_plan(&session.id) {
                    Ok(plan) => Some(plan),
                    Err(error) => {
                        tracing::debug!(session_id = %session.id, "worker recovery target unavailable: {error:#}");
                        None
                    }
                },
                project_memory: match controller.project_memory_sync_target(&session.id) {
                    Ok(target) => Some(target),
                    Err(error) => {
                        tracing::debug!(session_id = %session.id, "project memory target unavailable: {error:#}");
                        None
                    }
                },
            })
        })
        .collect()
}

pub fn dashboard_worker_targets_excluding(
    controller: &Controller,
    excluded_sessions: &std::collections::BTreeSet<String>,
) -> Vec<WorkerPollTarget> {
    let mut targets = dashboard_worker_targets(controller);
    targets.retain(|target| !excluded_sessions.contains(&target.session_id));
    targets
}

/// Sessions whose worker can answer credential requests right now. Sessions
/// still provisioning or already disconnected would only produce connection
/// errors, so they stay out.
///
/// Every session runs from a staged home of its own, on this machine as on any
/// other target, so each one is reconciled the same way. The one exception is a
/// local session an earlier release started from the profile home itself. Its
/// worker still reads that home, so a push would replace the person's own
/// skills tree; it is left out until it is next staged (see
/// [`crate::controller::local_profile_homes`]).
pub fn credential_sync_targets(controller: &Controller) -> Vec<CredentialSyncTarget> {
    controller
        .state
        .sessions
        .values()
        .filter(|session| {
            matches!(
                session.state,
                SessionState::Running | SessionState::Checkpointing
            ) && session.target.is_some()
        })
        .filter(|session| {
            crate::controller::local_profile_homes::session_has_a_staged_home_of_its_own(session)
        })
        .filter_map(|session| {
            let profile = controller.config.profiles.get(&session.last_profile)?;
            let spec = match controller.reconnect_command(&session.id) {
                Ok(spec) => spec,
                Err(error) => {
                    tracing::warn!(session_id = %session.id, "could not build credential sync target: {error:#}");
                    return None;
                }
            };
            let sync_github_token = target_syncs_github_token(session.target.as_ref());
            Some(CredentialSyncTarget {
                session_id: session.id.clone(),
                profile_id: session.last_profile.clone(),
                harness: profile.kind,
                profile_home: profile.home.clone(),
                authenticates_with_api_key: profile.auth_scheme().is_api_key(),
                sync_github_token,
                spec,
            })
        })
        .collect()
}

pub(super) fn target_syncs_github_token(target: Option<&mj_core::state::TargetLocator>) -> bool {
    target.is_some()
        && !matches!(
            target,
            Some(mj_core::state::TargetLocator::LocalBare { .. })
        )
}
