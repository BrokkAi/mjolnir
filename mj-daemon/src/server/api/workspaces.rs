//! Workspace routes.
//!
//! A session belongs to a workspace, and until these routes existed the only
//! way to make one was to open the terminal dashboard, so a fresh instance
//! could not be scripted at all (#1080). Both routes go through
//! [`SubagentBackend`], which performs the daemon's own create-or-get operation
//! on a blocking thread and republishes the list to the viewer.

use super::*;

pub(super) async fn list_workspaces(
    State(state): State<ServerState>,
) -> Result<Json<WorkspaceListResponse>, ApiFailure> {
    Ok(Json(WorkspaceListResponse {
        workspaces: backend(&state)?.list_workspaces().await?,
    }))
}

/// Create the named workspace, or return the one that already carries the name.
///
/// Names are the identity here — trimmed, at most 64 characters, unique
/// case-insensitively — so this is idempotent and answers `200 OK` rather than
/// claiming a creation it may not have performed. A script can therefore call
/// it before every run without checking first.
pub(super) async fn create_workspace(
    State(state): State<ServerState>,
    Json(request): Json<CreateWorkspaceRequest>,
) -> Result<Json<CreateWorkspaceResponse>, ApiFailure> {
    let (name, name_key) = mj_core::workspace::normalize_workspace_name(&request.name)
        .map_err(|error| ApiFailure::bad_request(format!("{error:#}")))?;
    let backend = backend(&state)?;
    // The store keeps `default` for sessions made before a workspace was
    // required, and refuses the name while that workspace is not listed. The
    // name is the caller's mistake, so it is refused here like any other
    // unusable name rather than surfacing as the store's failure (launch
    // finding R2-4). A listed `default` holds sessions and is returned as usual.
    if name_key == mj_core::workspace::DEFAULT_WORKSPACE_ID
        && !backend
            .list_workspaces()
            .await?
            .iter()
            .any(|workspace| workspace.name.to_lowercase() == name_key)
    {
        return Err(ApiFailure::bad_request(RESERVED_WORKSPACE_NAME));
    }
    Ok(Json(CreateWorkspaceResponse {
        workspace: backend.create_workspace(name).await?,
    }))
}

/// Why the name `default` cannot be used for a new workspace: the store
/// keeps it for the workspace that holds sessions from before workspaces.
const RESERVED_WORKSPACE_NAME: &str = "the workspace name \"default\" is reserved: it holds sessions made before Mjolnir had workspaces; choose another name";

/// The workspace a new session belongs to.
///
/// An empty answer means "let the controller choose", which it does only when
/// the instance holds exactly one workspace. Every session lives in a
/// workspace the dashboard and the viewer list, so an instance with none is
/// refused with the way to make one, rather than given a workspace nobody
/// sees (launch finding H-3).
pub(super) async fn workspace_for_new_session(
    backend: &Arc<dyn SubagentBackend>,
    requested: Option<String>,
) -> Result<String, ApiFailure> {
    if let Some(requested) = requested {
        return Ok(requested);
    }
    if backend.list_workspaces().await?.is_empty() {
        return Err(ApiFailure::conflict(
            "this instance has no workspace yet; create one with POST /api/v1/workspaces \
             or `mj workspaces create NAME`, then name it in workspace_id",
        ));
    }
    Ok(String::new())
}
