use super::*;

/// Why the control loop is stopping because one of its feeds ended.
///
/// During shutdown every feed ends, and that is the plan. At any other time it
/// means the phone server has lost the machinery it exists to drive, so it
/// says which feed and exits non-zero instead of reporting success.
pub(super) fn feed_stopped(shutting_down: bool, reason: &'static str) -> Option<anyhow::Error> {
    (!shutting_down).then(|| anyhow::anyhow!(reason))
}

/// Whether a session's agent said it accepts image content in prompts. An
/// agent that has not answered `initialize` yet has advertised nothing, so the
/// phone is not offered controls the agent may refuse.
pub(super) fn agent_accepts_prompt_images(
    operational: &mj_core::relay::RelayOperationalState,
) -> bool {
    operational
        .agent_capabilities
        .as_ref()
        .is_some_and(|capabilities| capabilities.prompt_capabilities.image)
}

pub(super) fn controller_action_session_id(action: &ControllerAction) -> Option<String> {
    match action {
        ControllerAction::New { .. } => None,
        ControllerAction::Prompt { session_id, .. }
        | ControllerAction::RunShell { session_id, .. }
        | ControllerAction::CancelShell { session_id, .. }
        | ControllerAction::Close { session_id }
        | ControllerAction::ForceClose { session_id, .. }
        | ControllerAction::Resume { session_id, .. }
        | ControllerAction::Open { session_id }
        | ControllerAction::Cancel { session_id }
        | ControllerAction::RemoveQueuedPrompt { session_id, .. }
        | ControllerAction::RespondElicitation { session_id, .. }
        | ControllerAction::Rename { session_id, .. }
        | ControllerAction::CancelTurn { session_id }
        | ControllerAction::SetConfig { session_id, .. }
        | ControllerAction::SetPlanMode { session_id, .. }
        | ControllerAction::StartReview { session_id }
        | ControllerAction::ResolveReview { session_id, .. } => Some(session_id.clone()),
        ControllerAction::Move { request } => {
            Some(request.preparation.selection.session_id.clone())
        }
        // A refresh belongs to a profile or a target rather than a session, so
        // it takes no session slot and cannot be refused as session-busy.
        ControllerAction::RefreshQuota { .. } | ControllerAction::RefreshCapacity { .. } => None,
    }
}

/// Flatten a joined blocking result into the answer a phone channel carries.
pub(super) fn flatten_stored<T>(
    joined: std::result::Result<Result<T>, tokio::task::JoinError>,
) -> std::result::Result<T, String> {
    match joined {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(format!("{error:#}")),
        Err(error) => Err(format!("viewer state task failed: {error}")),
    }
}
