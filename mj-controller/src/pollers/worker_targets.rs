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
            // A session that runs out of the user's own harness home must not
            // receive Mjolnir's managed skills; they would land in that home
            // and stay there.
            let owns_profile_home = match controller.session_owns_profile_home(&session.id) {
                Ok(owns) => owns,
                Err(error) => {
                    tracing::warn!(session_id = %session.id, "could not decide profile home ownership for credential sync: {error:#}");
                    false
                }
            };
            Some(CredentialSyncTarget {
                session_id: session.id.clone(),
                profile_id: session.last_profile.clone(),
                harness: profile.kind,
                profile_home: profile.home.clone(),
                authenticates_with_api_key: profile.auth_scheme().is_api_key(),
                sync_github_token,
                owns_profile_home,
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
