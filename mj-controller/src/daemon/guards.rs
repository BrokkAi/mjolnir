use super::*;

/// A session has its own lifecycle operation in flight, so a fresh checkpoint
/// would fight it. Callers that only need archived state (bundle export) fall
/// back to the last durable checkpoint instead of failing (#1010).
///
/// It names the operation and how long it has been running, because "something
/// is busy" left people retrying an export with nothing to act on (#1010).
#[derive(Debug, Clone)]
pub(crate) struct SessionLifecycleBusy {
    pub session_id: String,
    pub operation: &'static str,
    pub age_seconds: u64,
}

impl std::fmt::Display for SessionLifecycleBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "session {} is busy with a {} that started {}s ago",
            self.session_id, self.operation, self.age_seconds
        )
    }
}

impl std::error::Error for SessionLifecycleBusy {}

/// Name a running operation and how long it has been running. A clock that has
/// gone backwards must not print a nonsensical age, so it saturates at zero.
pub(super) fn describe_lifecycle_busy(
    session_id: &str,
    active: &ActiveLifecycle,
) -> SessionLifecycleBusy {
    SessionLifecycleBusy {
        session_id: session_id.to_owned(),
        operation: active.kind.label(),
        age_seconds: epoch_seconds().saturating_sub(active.started_at_epoch_seconds),
    }
}

pub(super) fn ensure_no_active_lifecycle(state: &RuntimeState) -> Result<()> {
    match state.any_lifecycle_busy() {
        Some(busy) => bail!("cannot rename configuration while {busy}"),
        None => Ok(()),
    }
}

pub(super) fn install_renamed_controller(state: &RuntimeState, controller: Controller) {
    *state
        .controller
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = controller;
    state.publish_revision();
}

pub(super) fn workspace_snapshot(workspace_id: &str) -> Result<WorkspaceSnapshot> {
    let workspace = crate::database::list_workspaces()?
        .into_iter()
        .find(|workspace| workspace.id == workspace_id)
        .with_context(|| format!("unknown workspace {workspace_id:?}"))?;
    let ids = crate::database::session_ids_for_workspace(workspace_id)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let controller = Controller::load()?;
    let sessions = controller
        .state
        .sessions
        .values()
        .filter(|session| session.state.is_active() && ids.contains(&session.id))
        .map(|session| SessionPreview {
            id: session.id.clone(),
            title: session.display_title().to_owned(),
            project: session.project_name(&controller.config),
            harness: session.harness_kind.display_name().to_owned(),
            state: session.state.as_str().to_owned(),
            active: session.state.is_active(),
            updated_at: session.updated_at.clone(),
        })
        .collect();
    let drafts = crate::database::list_detached_drafts(workspace_id)?
        .into_iter()
        .map(|draft| DraftPreview {
            id: draft.id,
            session_id: draft.session_id,
            source: draft.source,
            owner_pid: draft.owner_pid,
            saved_at: draft.saved_at,
        })
        .collect();
    Ok(WorkspaceSnapshot {
        workspace,
        sessions,
        drafts,
    })
}
