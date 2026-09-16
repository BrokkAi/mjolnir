use super::*;

/// A session has its own lifecycle operation in flight, so a fresh checkpoint
/// would fight it. Callers that only need archived state (bundle export) fall
/// back to the last durable checkpoint instead of failing (#1010).
#[derive(Debug)]
pub(crate) struct SessionLifecycleBusy {
    pub session_id: String,
}

impl std::fmt::Display for SessionLifecycleBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "session {} has a lifecycle operation in flight; its checkpoint cannot run now",
            self.session_id
        )
    }
}

impl std::error::Error for SessionLifecycleBusy {}

pub(super) fn ensure_no_active_lifecycle(state: &RuntimeState) -> Result<()> {
    ensure!(
        !state
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .any(|active| active.result.borrow().is_none()),
        "cannot rename configuration while a session lifecycle operation is active"
    );
    Ok(())
}

/// The workspace's active session ids, oldest first, so force deletion
/// destroys them in a deterministic order and partial failures name what is
/// left.
pub(super) fn active_sessions_for_force_destruction(
    controller: &Controller,
    workspace_id: &str,
) -> Vec<String> {
    let mut sessions: Vec<&SessionRecord> = controller
        .state
        .sessions
        .values()
        .filter(|session| session.workspace_id == workspace_id && session.state.is_active())
        .collect();
    sessions.sort_by(|a, b| a.compare_by_creation(b));
    sessions
        .into_iter()
        .map(|session| session.id.clone())
        .collect()
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
