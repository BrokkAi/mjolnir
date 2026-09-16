use super::*;

/// A unified diff of everything the session changed.
pub(super) async fn diff(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<Response, ApiFailure> {
    let backend = backend(&state)?.clone();
    let diff = backend.diff(session_id).await?;
    Ok(([(CONTENT_TYPE, "text/x-diff; charset=utf-8")], diff).into_response())
}

/// One file from the session's workspace, as bytes.
///
/// The path is checked here as well as on the target: a caller that spells an
/// absolute or escaping path has made a mistake worth naming, and there is no
/// reason to spend a round trip to the target discovering it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteFileQuery {
    pub path: PathBuf,
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteFileResponse {
    pub path: PathBuf,
    pub bytes: usize,
}

pub(super) async fn write_file(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Query(query): Query<WriteFileQuery>,
    bytes: axum::body::Bytes,
) -> Result<Json<WriteFileResponse>, ApiFailure> {
    mj_core::config::validate_relative_destination(&query.path)
        .map_err(|error| ApiFailure::bad_request(format!("{error:#}")))?;
    {
        let snapshot = state.snapshot_rx.borrow();
        let session = require_session_record(&snapshot, &session_id)?;
        if !session.is_idle || session.lifecycle != ViewerLifecycleCategory::Live {
            return Err(ApiFailure::conflict(
                "session must be live and idle for file injection",
            ));
        }
    }
    let count = bytes.len();
    backend(&state)?
        .write_file(
            session_id,
            query.path.clone(),
            bytes.to_vec(),
            query.overwrite,
        )
        .await?;
    Ok(Json(WriteFileResponse {
        path: query.path,
        bytes: count,
    }))
}

pub(super) async fn elicitations(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<Json<Vec<mj_core::elicitation::ElicitationRequest>>, ApiFailure> {
    let snapshot = state.snapshot_rx.borrow();
    Ok(Json(
        require_session_record(&snapshot, &session_id)?
            .pending_elicitations
            .clone(),
    ))
}

pub(super) async fn respond_elicitation(
    State(state): State<ServerState>,
    Path((session_id, elicitation_id)): Path<(String, String)>,
    Json(response): Json<mj_core::elicitation::ElicitationResponse>,
) -> Result<StatusCode, ApiFailure> {
    send_action(
        &state,
        ControllerAction::RespondElicitation {
            session_id,
            elicitation_id,
            response,
        },
    )
    .await
}

pub(super) async fn read_file(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Query(query): Query<FileQuery>,
) -> Result<Response, ApiFailure> {
    let backend = backend(&state)?.clone();
    let path = PathBuf::from(&query.path);
    if query.path.trim().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(ApiFailure::bad_request(
            "path must be relative to the session workspace and must not contain '..'",
        ));
    }
    let bytes = backend.read_file(session_id, path).await?;
    Ok(([(CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

/// Get the session's work out, in whichever form the caller asked for.
pub(super) async fn export(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<ExportRequest>,
) -> Result<Response, ApiFailure> {
    let backend = backend(&state)?.clone();
    match request.kind {
        ExportKind::Patch => {
            let diff = backend.diff(session_id).await?;
            Ok(([(CONTENT_TYPE, "text/x-diff; charset=utf-8")], diff).into_response())
        }
        ExportKind::Branch => {
            let branch = request
                .branch
                .as_deref()
                .map(str::trim)
                .filter(|branch| !branch.is_empty())
                .ok_or_else(|| ApiFailure::bad_request("a branch export needs a branch name"))?
                .to_owned();
            let pushed = backend.push_branch(session_id, branch).await?;
            Ok(Json(pushed).into_response())
        }
        ExportKind::Bundle => {
            let bundle = backend.bundle(session_id.clone()).await?;
            // The filename reaches a header, so keep it to characters that
            // cannot end the quoted string or split the response.
            let filename: String = format!("{session_id}-{}.bundle", bundle.repository)
                .chars()
                .map(|character| match character {
                    'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '-' | '_' => character,
                    _ => '-',
                })
                .collect();
            Ok((
                [
                    (CONTENT_TYPE, "application/octet-stream".to_owned()),
                    (
                        CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{filename}\""),
                    ),
                ],
                bundle.bytes,
            )
                .into_response())
        }
    }
}
