//! Workspace routes.
//!
//! A session belongs to a workspace, and until these routes existed the only
//! way to make one was to open the terminal dashboard, so a fresh instance
//! could not be scripted at all (#1080). Both routes go through
//! [`SubagentBackend`], which performs the daemon's own create-or-get operation
//! on a blocking thread and republishes the list to the viewer.

use super::*;

/// The name a session falls back to when the instance has no workspace yet.
///
/// The store is created with this workspace already in it; it is hidden from
/// listings until it owns a session, which is why a fresh instance looks empty.
pub(super) const DEFAULT_WORKSPACE_NAME: &str = "default";

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
    let (name, _) = mj_core::workspace::normalize_workspace_name(&request.name)
        .map_err(|error| ApiFailure::bad_request(format!("{error:#}")))?;
    Ok(Json(CreateWorkspaceResponse {
        workspace: backend(&state)?.create_workspace(name).await?,
    }))
}

/// The workspace a new session belongs to when the caller named none.
///
/// An empty answer means "let the controller choose", which is what it already
/// does when the instance holds exactly one workspace and what it refuses when
/// it holds several. Only the empty case is decided here: a fresh instance
/// lists no workspace, so a script's first `mj new` was refused with nothing it
/// could do about it from the CLI or the API (#1080). The store always holds
/// the `default` workspace, so adopt that instead of refusing.
pub(super) async fn workspace_for_new_session(
    backend: &Arc<dyn SubagentBackend>,
    requested: Option<String>,
) -> Result<String, ApiFailure> {
    if let Some(requested) = requested {
        return Ok(requested);
    }
    if !backend.list_workspaces().await?.is_empty() {
        return Ok(String::new());
    }
    Ok(backend
        .create_workspace(DEFAULT_WORKSPACE_NAME.to_owned())
        .await?
        .id)
}
