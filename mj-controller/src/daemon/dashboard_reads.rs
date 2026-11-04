//! Store reads that the terminal dashboard used to make itself. The daemon is
//! the only process that opens the store, so it can live in a volume that no
//! client can reach.

use super::*;

/// Store-only startup data; the runtime history owns the session snapshot.
pub(super) struct DashboardStored {
    pub metadata: StoredSessionMetadata,
    pub layouts: WorkspaceLayouts,
    pub queued_prompts: BTreeMap<String, Vec<mj_core::relay::QueuedPrompt>>,
    pub subagent_parents: BTreeSet<String>,
}

pub(super) fn dashboard_stored(workspace_id: &str) -> Result<DashboardStored> {
    let controller = Controller::load()?;
    let workspace_ids = crate::database::list_workspaces()?
        .into_iter()
        .map(|workspace| workspace.id)
        .collect();
    let layouts = workspace_layouts(workspace_ids)?;
    // A pinned historical conversation needs its family in the first frame.
    let subagent_parents = layouts
        .conversation_layouts
        .get(workspace_id)
        .into_iter()
        .flat_map(|layout| layout.sessions.values())
        .filter_map(|id| controller.state.subagents.get(id))
        .map(|child| child.parent_session_id.clone())
        .collect();
    let queued_prompts = crate::pollers::projected_queued_prompts(&controller)?;
    Ok(DashboardStored {
        metadata: StoredSessionMetadata {
            mount_history: controller.state.mount_history,
            container_sizes: controller.state.container_sizes,
        },
        layouts,
        queued_prompts,
        subagent_parents,
    })
}

/// Only the rows in this client's bootstrap need read frontiers.
pub(super) fn client_read_frontiers(
    mut records: mj_core::snapshot_map::SnapshotMap<String, SessionRecord>,
    client_id: &str,
) -> Result<mj_core::snapshot_map::SnapshotMap<String, SessionRecord>> {
    let active = records
        .values()
        .filter(|session| session.state.is_active())
        .map(|session| session.id.clone())
        .collect::<Vec<_>>();
    for id in active {
        let session = records.get_mut(&id).expect("active session exists");
        session.viewed_through_event_ordinal =
            crate::database::client_read_frontier(client_id, &session.workspace_id, &session.id)?;
    }
    Ok(records)
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
