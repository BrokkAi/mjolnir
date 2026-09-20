use super::*;

#[derive(Default, Deserialize)]
pub(super) struct NativeHistoryQuery {
    before_position: Option<u64>,
    before_id: Option<String>,
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
    {
        let snapshot = state.snapshot_rx.borrow();
        let action = ControllerAction::Prompt {
            command_id: None,
            session_id: session_id.clone(),
            text: request.text.clone(),
            images: Vec::new(),
        };
        validate_action(&action, &snapshot)?;
        let session = require_session_record(&snapshot, &session_id)?;
        if !session.capabilities.prompt {
            return Err(ApiFailure::conflict(
                "this session cannot take a prompt right now",
            ));
        }
    }
    let turn_id = backend.prompt(session_id, request.text).await?;
    Ok((StatusCode::ACCEPTED, Json(PromptResponse { turn_id })))
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
