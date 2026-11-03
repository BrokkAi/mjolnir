//! Store reads that the terminal dashboard used to make itself. The daemon is
//! the only process that opens the store, so it can live in a volume that no
//! client can reach.

use super::*;

/// Assembles what the dashboard shows before its runtime feed connects:
/// live sessions (and durably moving ones) with this client's read frontiers,
/// the workspaces' layouts, queued prompts and launch recency.
pub(super) fn dashboard_startup(client_id: &str) -> Result<DashboardStartup> {
    let mut controller = Controller::load()?;
    let launch_recency = mj_client::runtime_feed::launch_recency(&controller.state.sessions);
    // Start from what the runtime feed will send, which is live sessions
    // only; the resume dialog asks for the stopped ones. No lifecycle
    // operation is known yet, so only a durable move keeps a stopped session
    // live here.
    let moving = crate::database::load_move_operations()?
        .into_iter()
        .filter(|operation| operation.is_active())
        .map(|operation| operation.selection.session_id)
        .collect();
    let live = mj_core::state::live_session_ids(
        &controller.state.sessions,
        &controller.state.subagents,
        &moving,
    );
    controller.state.sessions.retain(|id, _| live.contains(id));
    controller.state.subagents.retain(|id, _| live.contains(id));
    apply_client_read_frontiers(&mut controller.state, client_id)?;
    let workspace_ids = crate::database::list_workspaces()?
        .into_iter()
        .map(|workspace| workspace.id)
        .collect();
    let layouts = workspace_layouts(workspace_ids)?;
    let queued_prompts = crate::pollers::projected_queued_prompts(&controller)?;
    Ok(DashboardStartup {
        state: controller.state,
        pane_sizes: layouts.pane_sizes,
        conversation_layouts: layouts.conversation_layouts,
        queued_prompts,
        launch_recency,
    })
}

/// Each active session's stored read frontier for `client_id`.
fn apply_client_read_frontiers(state: &mut mj_core::state::State, client_id: &str) -> Result<()> {
    let active: Vec<_> = state
        .sessions
        .values()
        .filter(|session| session.state.is_active())
        .map(|session| session.id.clone())
        .collect();
    for id in active {
        let session = state.sessions.get_mut(&id).expect("active session exists");
        session.viewed_through_event_ordinal =
            crate::database::client_read_frontier(client_id, &session.workspace_id, &session.id)?;
    }
    Ok(())
}

pub(super) fn workspace_layouts(workspace_ids: Vec<String>) -> Result<WorkspaceLayouts> {
    let mut layouts = WorkspaceLayouts::default();
    for workspace_id in workspace_ids {
        let sizes = crate::database::load_workspace_pane_sizes(&workspace_id)?;
        let layout = crate::database::load_workspace_layout(&workspace_id)?;
        layouts.pane_sizes.insert(workspace_id.clone(), sizes);
        layouts.conversation_layouts.insert(workspace_id, layout);
    }
    Ok(layouts)
}

pub(super) fn stored_session_metadata() -> Result<StoredSessionMetadata> {
    let state = crate::database::load_state()?;
    Ok(StoredSessionMetadata {
        mount_history: state.mount_history,
        container_sizes: state.container_sizes,
    })
}
