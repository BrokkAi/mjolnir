use super::*;

#[derive(Default, Deserialize)]
pub(super) struct NativeHistoryQuery {
    before_position: Option<u64>,
    before_id: Option<String>,
}

pub(super) async fn transcript_history(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Query(query): Query<NativeHistoryQuery>,
) -> Result<Json<serde_json::Value>, ApiFailure> {
    require_session_record(&state.snapshot_rx.borrow(), &session_id)?;
    let before = match (query.before_position, query.before_id) {
        (Some(position), Some(stable_id)) => Some(mj_core::storage::TranscriptCursor {
            position,
            stable_id,
        }),
        (None, None) => None,
        _ => {
            return Err(ApiFailure::bad_request(
                "both before_position and before_id are required",
            ));
        }
    };
    let page = backend(&state)?
        .transcript_history(session_id, before)
        .await?;
    let response = tokio::task::spawn_blocking(move || {
        let entries = mj_client::transcript::history_entries(&page);
        serde_json::json!({"items": entries, "before": page.before, "frontier": page.frontier})
    })
    .await
    .map_err(|error| anyhow::anyhow!("history rendering task failed: {error}"))?;
    Ok(Json(response))
}

pub(super) async fn native_agent_history(
    State(state): State<ServerState>,
    Path((owner, child)): Path<(String, String)>,
    Query(query): Query<NativeHistoryQuery>,
) -> Result<Json<serde_json::Value>, ApiFailure> {
    require_session_record(&state.snapshot_rx.borrow(), &owner)?;
    let before = query.before_position.zip(query.before_id);
    let page = backend(&state)?
        .native_agent_history(owner, child, before)
        .await?;
    let items = page
        .items
        .iter()
        .map(|item| {
            serde_json::json!({
                "stable_id": item.stable_id,
                "position": item.position,
                "role": mj_core::transcript::transcript_item_role(&item.body),
                "text": mj_transcript::transcript::transcript_item_text(item),
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(
        serde_json::json!({"generation": page.generation_ordinal, "items": items, "has_more": page.has_more}),
    ))
}

pub(super) async fn prompt(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<PromptRequest>,
) -> Result<(StatusCode, Json<PromptResponse>), ApiFailure> {
    let backend = backend(&state)?.clone();
    let action = ControllerAction::Prompt {
        command_id: None,
        session_id: session_id.clone(),
        text: request.text.clone(),
        images: Vec::new(),
    };
    // A session that is still coming up takes its prompt once its worker
    // attaches, rather than refusing it. A caller that just created the
    // session has nothing better to do than retry, and every caller would
    // otherwise need its own retry loop around a refusal it cannot tell apart
    // from a session that will never take a prompt.
    let mut snapshot_rx = state.snapshot_rx.clone();
    let deadline = tokio::time::Instant::now() + PROMPT_READINESS_WAIT;
    let mut waiting = false;
    loop {
        {
            let snapshot = snapshot_rx.borrow_and_update();
            validate_action(&action, &snapshot)?;
            let session = require_session_record(&snapshot, &session_id)?;
            if session.capabilities.prompt {
                break;
            }
            // Between the worker's handshake and its first report the record
            // already says running while the session still has nothing to
            // take a prompt with, so once a start has been seen only a
            // failure or a stop ends the wait early.
            waiting = if waiting {
                still_live(session)
            } else {
                is_coming_up(session)
            };
            if !waiting {
                return Err(ApiFailure::conflict(
                    "this session cannot take a prompt right now",
                ));
            }
        }
        match tokio::time::timeout_at(deadline, snapshot_rx.changed()).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                return Err(ApiFailure::unavailable("the daemon is shutting down"));
            }
            Err(_) => {
                return Err(ApiFailure::conflict(format!(
                    "this session is still starting after {} seconds; try the prompt again once `mj sessions` shows it running",
                    PROMPT_READINESS_WAIT.as_secs()
                )));
            }
        }
    }
    let turn_id = backend.prompt(session_id, request.text).await?;
    Ok((StatusCode::ACCEPTED, Json(PromptResponse { turn_id })))
}

/// How long a prompt waits for a session that is still starting. Bounded, so
/// the request finishes with its own response, and shorter than the CLI's
/// request timeout so the caller reads this answer rather than a timeout.
const PROMPT_READINESS_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Whether a session is on its way to taking prompts: provisioning, or
/// provisioned and waiting for its worker to attach.
fn is_coming_up(session: &ViewerSession) -> bool {
    use mj_core::state::SessionState;
    still_live(session)
        && matches!(
            SessionState::from_stored(&session.state),
            Some(SessionState::Provisioning | SessionState::Disconnected)
        )
}

/// Whether a session is starting or running without a recorded failure.
fn still_live(session: &ViewerSession) -> bool {
    !session.has_error
        && matches!(
            session.lifecycle,
            ViewerLifecycleCategory::Starting | ViewerLifecycleCategory::Live
        )
}

/// Page through a session's transcript.
///
/// It reads the durable projection rather than the live actor, so it answers
/// the same way while a session runs and long after it stopped.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageQuery {
    pub after_seq: Option<u64>,
    pub limit: Option<usize>,
}

pub(super) async fn usage(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Query(query): Query<UsageQuery>,
) -> Result<Json<crate::database::UsagePage>, ApiFailure> {
    let page = backend(&state)?
        .usage(
            session_id,
            query.after_seq.unwrap_or(0),
            query.limit.unwrap_or(200).clamp(1, 1000),
        )
        .await?
        .ok_or_else(|| ApiFailure::not_found("no usage history is recorded for that session"))?;
    Ok(Json(page))
}

pub(super) async fn transcript(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Query(query): Query<TranscriptQuery>,
) -> Result<Json<TranscriptResponse>, ApiFailure> {
    let backend = backend(&state)?.clone();
    let limit = query
        .limit
        .unwrap_or(DEFAULT_TRANSCRIPT_LIMIT)
        .clamp(1, MAX_TRANSCRIPT_LIMIT);
    let page = backend
        .transcript(
            session_id.clone(),
            query.after_seq.unwrap_or(0),
            limit,
            query.role,
        )
        .await?
        .ok_or_else(|| ApiFailure::not_found("no transcript is recorded for that session"))?;
    Ok(Json(TranscriptResponse {
        next_after_seq: page.next_after_seq,
        session_id,
        latest_seq: page.latest_seq,
        execution: page.execution,
        items: page
            .items
            .iter()
            .map(|item| TranscriptItemView {
                stable_id: item.stable_id.clone(),
                position: item.position,
                seq: item.seq(),
                role: mj_core::transcript::transcript_item_role(&item.body).to_owned(),
                text: mj_transcript::transcript::transcript_item_text(item),
                created_at_ms: item.created_at_ms,
                last_changed_at_ms: item.last_changed_at_ms,
                body: item.body.clone(),
            })
            .collect(),
    }))
}

pub(super) async fn suspend(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    request: Option<Json<SuspendRequest>>,
) -> Result<StatusCode, ApiFailure> {
    let active_children = {
        let snapshot = state.snapshot_rx.borrow();
        let session = require_session_record(&snapshot, &session_id)?;
        session
            .subagent_session_ids
            .iter()
            .filter(|id| {
                snapshot
                    .sessions
                    .iter()
                    .any(|child| child.id == id.as_str() && child.lifecycle.is_dashboard_visible())
            })
            .count()
    };
    if active_children > 0
        && !request
            .as_ref()
            .is_some_and(|r| r.acknowledge_active_subagents)
    {
        return Err(ApiFailure::conflict(format!(
            "session has {active_children} sub-agent(s); retry with acknowledge_active_subagents=true to suspend children first"
        )));
    }
    backend(&state)?.cancel_start(session_id.clone()).await?;
    send_action(&state, ControllerAction::Suspend { session_id }).await
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SuspendRequest {
    #[serde(default)]
    pub(super) acknowledge_active_subagents: bool,
}

pub(super) async fn destroy(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    request: Option<Json<DestroyRequest>>,
) -> Result<StatusCode, ApiFailure> {
    backend(&state)?.cancel_start(session_id.clone()).await?;
    let delete_branch = request.is_some_and(|r| r.delete_branch);
    send_action(
        &state,
        ControllerAction::Destroy {
            session_id,
            delete_branch,
        },
    )
    .await
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DestroyRequest {
    #[serde(default)]
    pub(super) delete_branch: bool,
}

pub(super) async fn interrupt_turn(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    send_action(&state, ControllerAction::InterruptTurn { session_id }).await
}

pub(super) async fn send_action(
    state: &ServerState,
    action: ControllerAction,
) -> Result<StatusCode, ApiFailure> {
    validate_action(&action, &state.snapshot_rx.borrow())?;
    let (reply, outcome) = tokio::sync::oneshot::channel();
    state
        .action_tx
        .send(ControllerRequest { action, reply })
        .await
        .map_err(|_| ApiFailure::unavailable("the controller is not accepting actions"))?;
    let outcome = outcome
        .await
        .map_err(|_| ApiFailure::unavailable("the controller dropped this action"))?;
    match outcome.rejection() {
        Some(rejection) => Err(rejection.into()),
        None => Ok(StatusCode::ACCEPTED),
    }
}
