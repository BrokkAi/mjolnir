use super::*;

#[derive(Default, Deserialize)]
pub(super) struct NativeHistoryQuery {
    before_position: Option<u64>,
    before_id: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct NativeAgentHistoryQuery {
    before_position: Option<u64>,
    before_id: Option<String>,
    #[serde(default)]
    full: bool,
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
    let response = mj_core::runtime::spawn_blocking(move || {
        let entries = mj_client::transcript::history_entries(&page);
        serde_json::json!({"items": entries, "before": page.before, "frontier": page.frontier})
    })
    .await
    .map_err(|error| anyhow::anyhow!("history rendering task failed: {error}"))?;
    Ok(Json(response))
}

pub(super) async fn native_agent_history(
    State(state): State<ServerState>,
    headers: axum::http::HeaderMap,
    Path((owner, child)): Path<(String, String)>,
    Query(query): Query<NativeAgentHistoryQuery>,
) -> Result<Response, ApiFailure> {
    if query.full {
        require_terminal(&state, &headers)?;
    } else {
        require_session_record(&state.snapshot_rx.borrow(), &owner)?;
    }
    let before = query.before_position.zip(query.before_id);
    let page = backend(&state)?
        .native_agent_history(owner, child, before)
        .await?;
    if query.full {
        return Ok(Json(page).into_response());
    }
    let items = page
        .items
        .iter()
        .map(|item| {
            serde_json::json!({
                "stable_id": item.stable_id,
                "tool_detail_trimmed": item.body.detail_trimmed(),
                "position": item.position,
                "role": mj_core::transcript::transcript_item_role(&item.body),
                "text": mj_transcript::transcript::transcript_item_text(item),
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(
        serde_json::json!({"generation": page.generation_ordinal, "items": items, "has_more": page.has_more}),
    ).into_response())
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(in crate::server) enum ToolCallDetailResponse {
    Full {
        stable_id: String,
        entry: Box<mj_core::transcript::ChatEntry>,
        body: Box<mj_core::transcript::TranscriptBody>,
        call: Option<serde_json::Value>,
        diffs: Vec<mj_core::diff::DiffPatch>,
        terminal_outputs: Vec<mj_core::transcript::TerminalOutputRecord>,
    },
    Unavailable {
        stable_id: String,
        summary: Option<mj_core::transcript::TranscriptBody>,
        unavailable_reason: String,
    },
}

impl ToolCallDetailResponse {
    async fn render(
        stable_id: String,
        detail: Option<mj_core::transcript::ToolCallDetailResult>,
    ) -> Result<axum::response::Response, ApiFailure> {
        use axum::response::IntoResponse;
        use mj_core::transcript::{ToolCallDetailResult, TranscriptBody, TranscriptItem};

        let detail = detail.ok_or_else(|| ApiFailure::not_found("unknown transcript item"))?;
        match detail {
            ToolCallDetailResult::Unavailable { summary, reason } => Ok(Json(Self::Unavailable {
                stable_id,
                summary,
                unavailable_reason: reason,
            })
            .into_response()),
            ToolCallDetailResult::Full { body } => {
                Ok(mj_core::runtime::spawn_blocking(move || {
                    let (call, diffs, terminal_outputs) = match &body {
                        TranscriptBody::Tool {
                            call,
                            terminal_outputs,
                            ..
                        } => (
                            Some(call.value()),
                            mj_client::transcript::tool_detail_patches(call),
                            terminal_outputs.clone(),
                        ),
                        TranscriptBody::TerminalOutput { record, .. } => {
                            (None, Vec::new(), vec![record.clone()])
                        }
                        _ => (None, Vec::new(), Vec::new()),
                    };
                    let item = Arc::new(TranscriptItem {
                        stable_id: stable_id.clone(),
                        position: 0,
                        latest_content_event_ordinal: None,
                        created_at_ms: 0,
                        last_changed_at_ms: 0,
                        body,
                    });
                    let mut entry = mj_client::transcript::materialized_chat_entry(&item, 0);
                    // The response carries the body separately from its rendered entry.
                    entry.source = Default::default();
                    entry.recorded_at_ms = None;
                    entry.tool_ended_at_ms = None;
                    Json(Self::Full {
                        stable_id,
                        entry: Box::new(entry),
                        body: Box::new(
                            Arc::into_inner(item)
                                .expect("detail body has one owner")
                                .body,
                        ),
                        call,
                        diffs,
                        terminal_outputs,
                    })
                    .into_response()
                })
                .await
                .context("render tool call detail")?)
            }
        }
    }
}

pub(in crate::server) async fn tool_call_detail(
    State(state): State<ServerState>,
    Path((session_id, stable_id)): Path<(String, String)>,
) -> Result<axum::response::Response, ApiFailure> {
    require_session_record(&state.snapshot_rx.borrow(), &session_id)?;
    let detail = backend(&state)?
        .tool_call_detail(session_id, stable_id.clone())
        .await?;
    ToolCallDetailResponse::render(stable_id, detail).await
}

pub(super) async fn native_agent_tool_call_detail(
    State(state): State<ServerState>,
    Path((owner, child, stable_id)): Path<(String, String, String)>,
) -> Result<axum::response::Response, ApiFailure> {
    require_session_record(&state.snapshot_rx.borrow(), &owner)?;
    let detail = backend(&state)?
        .native_agent_tool_call_detail(owner, child, stable_id.clone())
        .await?;
    ToolCallDetailResponse::render(stable_id, detail).await
}

pub(super) async fn prompt(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<PromptRequest>,
) -> Result<(StatusCode, Json<PromptResponse>), ApiFailure> {
    let turn_id = submit_prompt(&state, session_id, request.text, request.command_id).await?;
    Ok((StatusCode::ACCEPTED, Json(PromptResponse { turn_id })))
}

/// Submit the user's prompt, answering its relay acceptance ordinal. This is
/// the one path a prompt from the user takes, whether it came to the prompt
/// endpoint or as a message with no sender.
pub(super) async fn submit_prompt(
    state: &ServerState,
    session_id: String,
    text: String,
    command_id: Option<String>,
) -> Result<u64, ApiFailure> {
    let backend = backend(state)?.clone();
    let action = ControllerAction::Prompt {
        command_id: command_id.clone(),
        session_id: session_id.clone(),
        text: text.clone(),
        files: Vec::new(),
        images: Vec::new(),
    };
    // Wait for startup or lifecycle work to release the session instead of
    // making every caller retry while it cannot take a prompt.
    let mut snapshot_rx = state.snapshot_rx.clone();
    let deadline = tokio::time::Instant::now() + PROMPT_READINESS_WAIT;
    let mut waiting = false;
    // While the prompt is held it has no turn, so an interrupt withdraws it
    // instead; see `interrupt_turn`.
    let mut hold: Option<HeldPrompt<'_>> = None;
    loop {
        {
            let snapshot = snapshot_rx.borrow_and_update();
            validate_action(&action, &snapshot)?;
            let session = require_session_record(&snapshot, &session_id)?;
            if session.capabilities.prompt {
                break;
            }
            // The lifecycle owner decides the result: Move and Restart can
            // pass through stopped or failed records before restoring them.
            // A transition can also precede the operation's publication.
            // After it finishes, keep waiting for the worker's first report
            // unless the session stopped or failed.
            waiting = session.operation.is_some()
                || (session.transitioning && !session.has_error)
                || if waiting {
                    still_live(session)
                } else {
                    is_coming_up(session)
                };
            if !waiting {
                return Err(ApiFailure::conflict(prompt_refusal(session)));
            }
        }
        let held = hold.get_or_insert_with(|| state.held_prompts.hold(&session_id));
        tokio::select! {
            changed = tokio::time::timeout_at(deadline, snapshot_rx.changed()) => match changed {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    return Err(ApiFailure::unavailable("the daemon is shutting down"));
                }
                Err(_) => {
                    return Err(ApiFailure::conflict(format!(
                        "this session is not ready to take a prompt after {} seconds; check `mj sessions` for its startup or lifecycle operation before trying again",
                        PROMPT_READINESS_WAIT.as_secs()
                    )));
                }
            },
            () = held.withdrawn() => return Err(withdrawn_prompt()),
        }
    }
    // Recording and submitting hold an upgrade admission, so a handoff waits
    // for the prompt instead of ending the daemon between them.
    let _admission = mj_store::upgrade::activity("API prompt submission")
        .map_err(|_| ApiFailure::unavailable("the daemon is shutting down"))?;
    // A caller that names its command can retry it; one that does not sends
    // a new command each time, so its record gets an identity of its own.
    let history_command = match &command_id {
        Some(command_id) => command_id.clone(),
        None => mj_client::session::new_command_id("api-history")
            .map_err(|error| ApiFailure::unavailable(format!("{error:#}")))?,
    };
    // The prompt is held while it is recorded, even for a session that was
    // ready at once, so an interrupt during the write withdraws it. A
    // withdrawn prompt stays in the history, as an abandoned draft does.
    let held = hold.unwrap_or_else(|| state.held_prompts.hold(&session_id));
    backend
        .record_user_prompt(session_id.clone(), history_command, text.clone())
        .await;
    // Taking the hold back and an interrupt withdrawing it are one decision,
    // so a prompt is either submitted or withdrawn, never both.
    if !held.release() {
        return Err(withdrawn_prompt());
    }
    Ok(backend.prompt_with_id(session_id, text, command_id).await?)
}

fn withdrawn_prompt() -> ApiFailure {
    ApiFailure::conflict(
        "this prompt was withdrawn: its turn was interrupted before the session could take it",
    )
}

/// Prompts the API is holding until startup or lifecycle work finishes.
///
/// A held prompt has no turn yet, so interrupting its session withdraws it
/// rather than letting it become a turn nobody wants once the worker
/// attaches (R2-1).
#[derive(Default)]
pub(crate) struct HeldPrompts {
    next_id: std::sync::atomic::AtomicU64,
    held: std::sync::Mutex<
        std::collections::HashMap<
            String,
            std::collections::BTreeMap<u64, tokio::sync::oneshot::Sender<()>>,
        >,
    >,
}

impl HeldPrompts {
    fn hold(&self, session_id: &str) -> HeldPrompt<'_> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (withdraw, withdrawn) = tokio::sync::oneshot::channel();
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(session_id.to_owned())
            .or_default()
            .insert(id, withdraw);
        HeldPrompt {
            prompts: self,
            session_id: session_id.to_owned(),
            id,
            withdrawn,
            released: false,
        }
    }

    /// Withdraw every prompt held for one session, answering how many there
    /// were.
    fn withdraw(&self, session_id: &str) -> usize {
        let withdrawn = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session_id)
            .unwrap_or_default();
        let count = withdrawn.len();
        for withdraw in withdrawn.into_values() {
            // The holder may have given up on its own in the meantime; then
            // there is nobody left to tell.
            let _ = withdraw.send(());
        }
        count
    }

    /// Take one hold back, answering whether it was still held.
    fn take(&self, session_id: &str, id: u64) -> bool {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(prompts) = held.get_mut(session_id) else {
            return false;
        };
        let taken = prompts.remove(&id).is_some();
        if prompts.is_empty() {
            held.remove(session_id);
        }
        taken
    }
}

/// One prompt held until its session is ready. Dropping it without releasing it,
/// as every refusal does, gives the hold up.
struct HeldPrompt<'a> {
    prompts: &'a HeldPrompts,
    session_id: String,
    id: u64,
    withdrawn: tokio::sync::oneshot::Receiver<()>,
    released: bool,
}

impl HeldPrompt<'_> {
    /// Resolves once an interrupt withdraws this prompt.
    async fn withdrawn(&mut self) {
        if (&mut self.withdrawn).await.is_err() {
            // Only this hold's own release drops the sender without sending,
            // and a released hold is never awaited again.
            std::future::pending::<()>().await;
        }
    }

    /// Take the prompt back to submit it: `false` when an interrupt withdrew
    /// it first.
    fn release(mut self) -> bool {
        self.released = true;
        self.prompts.take(&self.session_id, self.id)
    }
}

impl Drop for HeldPrompt<'_> {
    fn drop(&mut self) {
        if !self.released {
            self.prompts.take(&self.session_id, self.id);
        }
    }
}

/// How long a prompt waits for startup or lifecycle work. Bounded, so
/// the request finishes with its own response, and shorter than the CLI's
/// request timeout so the caller reads this answer rather than a timeout.
const PROMPT_READINESS_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Why a session that is not coming up refuses a prompt. A failed session
/// will not take one however long the caller waits, so the refusal says it
/// failed, why, and what can be done with it instead (launch finding R5-8).
/// Resume can repair a retained target or restore an archive.
fn prompt_refusal(session: &ViewerSession) -> String {
    if session.lifecycle != ViewerLifecycleCategory::Failed {
        return "this session cannot take a prompt right now".to_owned();
    }
    let id = &session.id;
    let ways_out = [
        (session.has_resume_source() && session.capabilities.resume)
            .then(|| format!("resume it with `mj resume --session {id}`")),
        session
            .capabilities
            .destroy
            .then(|| format!("remove it with `mj destroy --session {id}`")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    let mut refusal = if session.has_checkpoint || session.failed_launch_retry {
        "this session failed, so it cannot take a prompt"
    } else {
        "this session failed before it saved a checkpoint, so it cannot take a prompt or be resumed"
    }
    .to_owned();
    if !ways_out.is_empty() {
        refusal.push_str("; ");
        refusal.push_str(&ways_out.join(" or "));
    }
    refusal.push('.');
    if let Some(reason) = &session.launch_error {
        refusal.push_str(" It failed with: ");
        refusal.push_str(reason);
    }
    refusal
}

/// Whether a session is on its way to taking prompts: provisioning, or
/// provisioned and waiting for its worker to attach.
fn is_coming_up(session: &ViewerSession) -> bool {
    use mj_core::state::SessionState;
    still_live(session)
        && (session.state == crate::server::LAUNCHING_STATE
            || matches!(
                SessionState::from_stored(&session.state),
                Some(SessionState::Provisioning | SessionState::Disconnected)
            ))
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
) -> Result<Json<mj_store::database::UsagePage>, ApiFailure> {
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

pub(super) async fn usage_tree(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<Json<mj_store::database::UsageTree>, ApiFailure> {
    let tree = backend(&state)?
        .usage_tree(session_id)
        .await?
        .ok_or_else(|| ApiFailure::not_found("no accounting identity exists for that session"))?;
    Ok(Json(tree))
}

pub(super) async fn usage_report(
    State(state): State<ServerState>,
    Query(query): Query<mj_core::usage::SessionUsageQuery>,
) -> Result<Json<mj_core::usage::SessionUsageReport>, ApiFailure> {
    Ok(Json(backend(&state)?.usage_report(query).await?))
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
    let roles = if query.finished_only {
        if query
            .role
            .iter()
            .any(|role| *role != mj_core::transcript::TranscriptRole::Agent)
        {
            return Err(ApiFailure::bad_request(
                "finished_only can only be combined with role=agent",
            ));
        }
        vec![mj_core::transcript::TranscriptRole::Agent]
    } else {
        query.role
    };
    let page = backend
        .transcript(
            session_id.clone(),
            query.after_seq.unwrap_or(0),
            limit,
            roles,
            query.finished_only,
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
                tool_detail_trimmed: item.body.detail_trimmed(),
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

/// Suspend a session. Its active Mjolnir sub-agents are stopped without a
/// checkpoint and the session is suspended alone; the answer warns when some
/// of those sub-agents have not handed back their reports. Accepting the
/// suspend is not finishing it.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SuspendPreflightQuery {
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    without_checkpoint: bool,
    #[serde(default)]
    history_only: bool,
}

pub(super) async fn resolve_suspend_blockers(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<Json<mj_core::suspend_consent::ResolveSuspendBlockersResponse>, ApiFailure> {
    Ok(Json(
        crate::server::handlers::ask_resolve_suspend_blockers(&state, session_id).await?,
    ))
}

pub(super) async fn suspend_preflight(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<SuspendPreflightQuery>,
) -> Result<Json<mj_core::suspend_consent::SuspendPreflight>, ApiFailure> {
    require_session_record(&state.snapshot_rx.borrow(), &session_id)?;
    let preflight = crate::server::handlers::ask_suspend_preflight(
        &state,
        session_id,
        query.dry_run,
        query.without_checkpoint,
        query.history_only,
    )
    .await?;
    Ok(Json(preflight))
}

pub(super) async fn suspend(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    request: Option<Json<SuspendRequest>>,
) -> Result<(StatusCode, Json<SuspendSessionResponse>), ApiFailure> {
    let body = request.map(|Json(body)| body).unwrap_or_default();
    let mut consent = body
        .suspend_consent
        .unwrap_or(mj_core::suspend_consent::SuspendConsent {
            discard_untracked: body.discard_untracked,
            abort_operations: body.abort_operations,
            ..Default::default()
        });
    consent.without_checkpoint |= body.without_checkpoint;
    consent.history_only |= body.history_only;
    consent.confirm_no_checkpoint |= body.confirm_no_checkpoint;
    consent
        .validate()
        .map_err(|error| ApiError::new(StatusCode::CONFLICT, error.to_string()))?;
    let (active_children, owning_children_to_suspend) = {
        let snapshot = state.snapshot_rx.borrow();
        let session = require_session_record(&snapshot, &session_id)?;
        let active_children = session
            .subagent_session_ids
            .iter()
            .filter(|id| {
                snapshot
                    .sessions
                    .iter()
                    .any(|child| child.id == id.as_str() && child.lifecycle.is_dashboard_visible())
            })
            .cloned()
            .collect::<Vec<_>>();
        (active_children, session.owning_children_to_suspend)
    };
    let backend = backend(&state)?.clone();
    // Counted before the suspend is sent, which is what stops the children.
    let mut not_handed_back = 0;
    for child_id in &active_children {
        let handed_back = backend
            .subagent_handed_back(child_id.clone())
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(
                    %session_id,
                    %child_id,
                    error = format!("{error:#}"),
                    "could not tell whether a sub-agent handed back; warning about it"
                );
                false
            });
        if !handed_back {
            not_handed_back += 1;
        }
    }
    backend.cancel_start(session_id.clone()).await?;
    let status = send_action(
        &state,
        ControllerAction::Suspend {
            session_id: session_id.clone(),
            discard_untracked: consent.discard_untracked,
            abort_operations: consent.abort_operations,
            without_checkpoint: consent.without_checkpoint,
            confirm_no_checkpoint: consent.confirm_no_checkpoint,
            history_only: consent.history_only,
        },
    )
    .await?;
    let warnings = [
        mj_core::subagent::suspend_warning(not_handed_back),
        mj_core::suspend_consent::owning_children_suspend_warning(owning_children_to_suspend),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    Ok((
        status,
        Json(SuspendSessionResponse {
            session_id,
            stopped_subagents: active_children.len(),
            subagents_not_handed_back: not_handed_back,
            warning: (!warnings.is_empty()).then(|| warnings.join(" ")),
        }),
    ))
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SuspendRequest {
    /// Older clients send this to suspend a parent with active sub-agents.
    /// A suspend now always stops them, so it is accepted and ignored.
    #[serde(default, rename = "acknowledge_active_subagents")]
    _acknowledge_active_subagents: bool,
    #[serde(default)]
    pub(super) discard_untracked: mj_core::suspend_consent::UntrackedConsent,
    #[serde(default)]
    pub(super) abort_operations: mj_core::suspend_consent::OperationConsent,
    #[serde(default)]
    pub(super) without_checkpoint: bool,
    #[serde(default)]
    pub(super) history_only: bool,
    #[serde(default)]
    pub(super) suspend_consent: Option<mj_core::suspend_consent::SuspendConsent>,
    #[serde(default)]
    pub(super) confirm_no_checkpoint: bool,
}

pub(super) async fn destroy(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    _request: Option<Json<DestroyRequest>>,
) -> Result<StatusCode, ApiFailure> {
    backend(&state)?.cancel_start(session_id.clone()).await?;
    send_action(&state, ControllerAction::Destroy { session_id }).await
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DestroyRequest {}

/// The safe recent notices outlive their session record. Absence here says
/// nothing about completion: an accepted Destroy can still be running.
pub(super) async fn session_notices(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Json<Vec<mj_client::daemon::RuntimeNotice>> {
    Json(
        state
            .snapshot_rx
            .borrow()
            .notices
            .iter()
            .filter(|notice| notice.session_id == session_id)
            .cloned()
            .collect(),
    )
}

/// Submit an exact worker-published control without selecting a newer target.
pub(super) async fn turn_control(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(command): Json<mj_core::relay::RelayCommand>,
) -> Result<StatusCode, ApiFailure> {
    if !crate::server::is_turn_control_command(&command) {
        return Err(ApiFailure::bad_request("unsupported turn-control command"));
    }
    send_action(
        &state,
        ControllerAction::TurnControl {
            session_id,
            command,
        },
    )
    .await
}

pub(super) async fn interrupt_turn(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    // A prompt held until its session is ready has no turn to stop, so
    // interrupting the session withdraws it. Only a session that also has a
    // turn of its own goes on to the daemon.
    if state.held_prompts.withdraw(&session_id) > 0 {
        let has_turn = state
            .snapshot_rx
            .borrow()
            .sessions
            .iter()
            .any(|session| session.id == session_id && session.capabilities.interrupt_turn);
        if !has_turn {
            return Ok(StatusCode::ACCEPTED);
        }
    }
    send_action(&state, ControllerAction::InterruptTurn { session_id }).await
}

/// Review committed work through the push gate. Refusals, including tracked
/// uncommitted changes or an unsupported harness, reach the caller as 409.
pub(super) async fn start_review(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<(StatusCode, Json<StartReviewResponse>), ApiFailure> {
    let status = send_action(
        &state,
        ControllerAction::StartReview {
            session_id: session_id.clone(),
        },
    )
    .await?;
    Ok((
        status,
        Json(StartReviewResponse {
            session_id,
            started: true,
        }),
    ))
}

/// The review the session has open, if any, as the viewer shows it.
pub(super) async fn review_status(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<Json<ReviewStatusResponse>, ApiFailure> {
    let (trigger, review) = {
        let snapshot = state.snapshot_rx.borrow();
        let session = require_session_record(&snapshot, &session_id)?;
        (
            session.review_trigger,
            session.turn_review.as_deref().cloned(),
        )
    };
    Ok(Json(ReviewStatusResponse {
        session_id,
        trigger,
        review,
    }))
}

/// A finished review's per-reviewer output. `review` selects one by id; without
/// it the newest finished review answers.
pub(super) async fn finished_review(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Query(query): Query<FinishedReviewQuery>,
) -> Result<Json<FinishedReviewResponse>, ApiFailure> {
    {
        let snapshot = state.snapshot_rx.borrow();
        require_session_record(&snapshot, &session_id)?;
    }
    let review = backend(&state)?
        .finished_review(session_id.clone(), query.review.clone())
        .await?;
    if review.is_none()
        && let Some(review_id) = query.review
    {
        return Err(ApiFailure::not_found(format!(
            "no finished review {review_id} for session {session_id}"
        )));
    }
    Ok(Json(FinishedReviewResponse { session_id, review }))
}

pub(super) async fn send_action(
    state: &ServerState,
    action: ControllerAction,
) -> Result<StatusCode, ApiFailure> {
    crate::server::validate_action_live(state, &action).await?;
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

/// Cancel queued input without interrupting the current turn or releasing files.
pub(super) async fn clear_queue(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<ClearQueueRequest>,
) -> Result<(StatusCode, Json<PromptResponse>), ApiFailure> {
    require_session_record(&state.snapshot_rx.borrow(), &session_id)?;
    let command_id = match request.command_id {
        Some(id) => id,
        None => format!("api-clear-queue-{}", mj_core::state::new_session_id()?),
    };
    super::super::validate_public_id(&command_id)?;
    let handle = backend(&state)?
        .session_handle(session_id)
        .await?
        .ok_or_else(|| ApiFailure::conflict("session has no live worker"))?;
    let turn_id = handle
        .submit(command_id, mj_core::relay::RelayCommand::ClearQueuedPrompts)
        .await?;
    Ok((StatusCode::ACCEPTED, Json(PromptResponse { turn_id })))
}

/// Engine inspection runs in the controller's supervised background work.
pub(super) async fn container_upgrade_preview(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<Json<mj_core::state::ContainerUpgradePreview>, ApiFailure> {
    crate::server::validate_public_id(&session_id)?;
    require_session_record(&state.snapshot_rx.borrow(), &session_id)?;
    crate::server::handlers::viewer_backend(&state)?
        .container_upgrade_preview(session_id)
        .await?
        .map(Json)
        .map_err(ApiFailure::conflict)
}

pub(super) async fn upgrade_container(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<mj_core::state::ContainerUpgradeRequest>,
) -> Result<StatusCode, ApiFailure> {
    if request.selection.session_id != session_id {
        return Err(ApiFailure::bad_request(
            "upgrade selection belongs to another session",
        ));
    }
    send_action(&state, ControllerAction::UpgradeContainer { request }).await
}

pub(super) async fn cancel_container_upgrade(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    send_action(
        &state,
        ControllerAction::CancelContainerUpgrade { session_id },
    )
    .await
}

pub(super) async fn restart_agent(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
) -> Result<StatusCode, ApiFailure> {
    send_action(
        &state,
        ControllerAction::Restart {
            session_id,
            acknowledge_container_recreation: false,
        },
    )
    .await
}
