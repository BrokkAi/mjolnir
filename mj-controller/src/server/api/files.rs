use super::*;

/// A unified diff of everything the session changed.
pub(super) async fn diff(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Query(options): Query<DiffOptions>,
) -> Result<Response, ApiFailure> {
    let backend = backend(&state)?.clone();
    let json = options.json;
    let diff = backend.diff(session_id, options).await?;
    if json {
        let details: mj_checkpoint::archive::SessionDiff =
            serde_json::from_str(&diff).context("decode session diff metadata")?;
        return Ok(Json(details).into_response());
    }
    Ok(([(CONTENT_TYPE, "text/x-diff; charset=utf-8")], diff).into_response())
}

/// Reject a file path this API can never resolve, before it costs a round trip
/// to the target.
///
/// An absolute path is always a mistake worth naming here. A `..` is not: it is
/// how a multi-repo bundle names a sibling repository, now that a path resolves
/// in the directory the agent runs in rather than at the workspace root
/// (#1079). How far `..` may climb depends on the session's layout, which only
/// the daemon holds, so that is refused there and answers 409.
fn validate_session_file_path(path: &std::path::Path) -> Result<(), ApiFailure> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::Prefix(_) | Component::RootDir))
    {
        return Err(ApiFailure::bad_request(
            "path must be relative to the directory the session's agent runs in",
        ));
    }
    Ok(())
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
    validate_session_file_path(&query.path)?;
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
    let path = PathBuf::from(query.path.trim());
    validate_session_file_path(&path)?;
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
            let diff = backend.diff(session_id, DiffOptions::default()).await?;
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
