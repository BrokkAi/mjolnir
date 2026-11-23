//! Workspace routes.
//!
use super::*;
use std::future::Future;

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
    let backend = backend.clone();
    run_admitted_mutation(&state, "workspace creation", async move {
        Ok(Json(CreateWorkspaceResponse {
            workspace: backend.create_workspace(name).await?,
        }))
    })
    .await
}

/// Why the name `default` cannot be used for a new workspace: the store
/// keeps it for the workspace that holds sessions from before workspaces.
const RESERVED_WORKSPACE_NAME: &str = "the workspace name \"default\" is reserved: it holds sessions made before Mjolnir had workspaces; choose another name";

/// The workspace a new session belongs to.
///
/// When the caller omits the workspace, select the instance's sole workspace.
/// If there are none, tell the caller how to create one. If there are several,
/// ask the caller to name one.
pub(super) async fn workspace_for_new_session(
    backend: &Arc<dyn SubagentBackend>,
    requested: Option<String>,
) -> Result<String, ApiFailure> {
    if let Some(requested) = requested {
        return Ok(requested);
    }
    match backend.list_workspaces().await?.as_slice() {
        [workspace] => Ok(workspace.id.clone()),
        [] => Err(ApiFailure::conflict(
            "this instance has no workspace yet. Create one with POST /api/v1/workspaces \
             or `mj workspaces create NAME`, then name it in workspace_id",
        )),
        _ => Err(ApiFailure::conflict(
            "this instance holds more than one workspace. Supply workspace_id",
        )),
    }
}

/// Keep the admission with the task so disconnecting after acceptance cannot
/// interrupt a mutation between its durable write and its publication.
async fn run_admitted_mutation<T: Send + 'static>(
    state: &ServerState,
    label: &'static str,
    work: impl Future<Output = Result<T, ApiFailure>> + Send + 'static,
) -> Result<T, ApiFailure> {
    let admission = state
        .upgrade_gate
        .enter_unless_draining(label)
        .map_err(|_| ApiFailure::handoff())?;
    mj_core::runtime::spawn(async move {
        let _admission = admission;
        let result = work.await;
        if let Err(error) = &result {
            tracing::debug!(label, %error, "daemon HTTP mutation failed");
        }
        result
    })
    .await
    .context(label)?
}

pub(super) async fn rename_workspace(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    Json(request): Json<CreateWorkspaceRequest>,
) -> Result<StatusCode, ApiFailure> {
    let (name, _) = mj_core::workspace::normalize_workspace_name(&request.name)
        .map_err(|error| ApiFailure::bad_request(format!("{error:#}")))?;
    let backend = backend(&state)?.clone();
    run_admitted_mutation(&state, "workspace rename", async move {
        mj_core::runtime::spawn_blocking(move || mj_store::database::rename_workspace(&id, &name))
            .await
            .context("rename workspace task")??;
        backend.refresh_workspaces().await?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

pub(super) async fn touch_workspace(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    let backend = backend(&state)?.clone();
    run_admitted_mutation(&state, "workspace touch", async move {
        mj_core::runtime::spawn_blocking(move || mj_store::database::touch_workspace(&id))
            .await
            .context("touch workspace task")??;
        backend.refresh_workspaces().await?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

pub(super) async fn close_workspace(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    let backend = backend(&state)?.clone();
    let admission = state
        .upgrade_gate
        .enter_unless_draining("workspace close")
        .map_err(|_| ApiFailure::handoff())?;
    // Transfer the single admission guard into `close_workspace` so it is
    // dropped right after member suspensions are dispatched instead of holding
    // daemon handoff across slow suspensions.
    mj_core::runtime::spawn(async move {
        let result = backend.close_workspace(id, admission).await;
        if let Err(error) = &result {
            tracing::debug!(label = "workspace close", %error, "daemon HTTP mutation failed");
        }
        result
    })
    .await
    .context("workspace close")??;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn cancel_workspace_close(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    // Cancelling an in-flight close must still work while the upgrade gate is
    // draining, because the close itself holds an admission guard that blocks
    // daemon handoff.
    backend(&state)?.cancel_workspace_close(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn remove_project(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    let backend = backend(&state)?.clone();
    run_admitted_mutation(&state, "project removal", async move {
        backend.remove_project(id).await?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

fn require_terminal(
    state: &ServerState,
    headers: &axum::http::HeaderMap,
) -> Result<(), ApiFailure> {
    if loopback_authenticated(state, headers) {
        Ok(())
    } else {
        Err(ApiFailure::new(
            StatusCode::FORBIDDEN,
            "terminal authentication is required",
        ))
    }
}

pub(super) async fn list_workspace_drafts(
    State(state): State<ServerState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Vec<mj_client::daemon::WorkspaceDrafts>>, ApiFailure> {
    require_terminal(&state, &headers)?;
    Ok(Json(backend(&state)?.list_workspace_drafts().await?))
}

pub(super) async fn recover_draft(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Json<mj_client::http::RecoveredDraft>, ApiFailure> {
    require_terminal(&state, &headers)?;
    let draft =
        mj_core::runtime::spawn_blocking(move || mj_store::database::read_detached_draft(&id))
            .await
            .context("read recoverable draft task")??
            .ok_or_else(|| ApiFailure::not_found("no recoverable draft with that id"))?;
    Ok(Json(mj_client::http::RecoveredDraft {
        session_id: draft.session_id,
        text: draft.text,
        revision: draft.revision,
    }))
}

pub(super) async fn discard_drafts(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    Json(request): Json<mj_client::http::DiscardDraftsRequest>,
) -> Result<Json<serde_json::Value>, ApiFailure> {
    require_terminal(&state, &headers)?;
    run_admitted_mutation(&state, "terminal draft discard", async move {
        let count = mj_core::runtime::spawn_blocking(move || {
            mj_store::database::discard_detached_drafts(&id, request.draft_id.as_deref())
        })
        .await
        .context("discard terminal drafts task")??;
        Ok(Json(serde_json::json!({ "count": count })))
    })
    .await
}

pub(super) async fn persist_terminal_draft(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    Json(request): Json<mj_client::http::TerminalDraftRequest>,
) -> Result<StatusCode, ApiFailure> {
    require_terminal(&state, &headers)?;
    run_admitted_mutation(&state, "terminal draft save", async move {
        mj_core::runtime::spawn_blocking(move || {
            mj_store::database::save_terminal_draft(
                &id,
                &request.client_id,
                request.owner_pid,
                request.revision,
                request.draft,
            )
        })
        .await
        .context("persist terminal draft task")??;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

pub(super) async fn persist_read_receipt(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    Json(request): Json<mj_client::http::ReadReceiptRequest>,
) -> Result<Json<serde_json::Value>, ApiFailure> {
    require_terminal(&state, &headers)?;
    run_admitted_mutation(&state, "terminal read receipt", async move {
        let ordinal = mj_core::runtime::spawn_blocking(move || {
            mj_store::database::persist_read_receipt(
                &request.client_id,
                &request.workspace_id,
                &id,
                request.through,
            )
        })
        .await
        .context("persist terminal read receipt task")??;
        Ok(Json(serde_json::json!({ "ordinal": ordinal })))
    })
    .await
}
