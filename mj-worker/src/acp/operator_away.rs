use super::*;

/// Process-local operator availability and refusal memory, retained across bridge restarts.
#[derive(Debug, Clone, Default)]
pub struct OperatorAway(Arc<Mutex<OperatorAwayState>>);

#[derive(Debug)]
struct OperatorAwayState {
    presence: mj_core::state::OperatorPresence,
    // The accepted answers are both the fallback identity and Jev's evidence.
    refused: BTreeMap<(String, AwayRequest), Vec<serde_json::Value>>,
    next_bridge: u64,
    bridges: BTreeSet<u64>,
    pending: Vec<PendingElicitation>,
    /// Mjolnir guardian reviews in flight. They are outstanding tool
    /// permission requests like the forms in `pending`, so the same lock
    /// decides what is outstanding and withdraws both at once.
    reviews: Vec<PendingReview>,
    next_review: u64,
    /// Bridges whose current turn's tool permission requests were withdrawn.
    /// A request that arrives later in that turn is answered `cancelled` too,
    /// until the turn ends.
    withdrawn_turns: BTreeSet<u64>,
    changed: tokio::sync::watch::Sender<()>,
}

#[derive(Debug)]
struct PendingReview {
    bridge: u64,
    id: u64,
    withdrawn: tokio_util::sync::CancellationToken,
}

impl Default for OperatorAwayState {
    fn default() -> Self {
        Self {
            presence: Default::default(),
            refused: Default::default(),
            next_bridge: 0,
            bridges: Default::default(),
            pending: Default::default(),
            reviews: Default::default(),
            next_review: 0,
            withdrawn_turns: Default::default(),
            changed: tokio::sync::watch::channel(()).0,
        }
    }
}

impl OperatorAwayState {
    fn answer_away(&mut self, entry: PendingElicitation) {
        // A turn withdrawal marks the turn before sending session/cancel and
        // removes its forms after. AFK must not slip an answer through that
        // gap. Leave the form for the withdrawal to remove.
        if self.withdrawn_turns.contains(&entry.bridge)
            && entry.question.id.starts_with(TOOL_PERMISSION_ID_PREFIX)
        {
            self.pending.push(entry);
            return;
        }
        match entry {
            PendingElicitation {
                question,
                responder: PendingResponder::Acp { identity, answer },
                ..
            } => {
                let evidence =
                    (identity.1 == AwayRequest::Question).then(|| question_evidence(&question));
                if answer.send(PendingAnswer::OperatorAway).is_ok() {
                    let history = self.refused.entry(identity).or_default();
                    if let Some(evidence) = evidence {
                        history.push(evidence);
                        self.changed.send_replace(());
                    }
                }
            }
            // Command-loop recovery choices are never answered by AFK policy.
            entry => self.pending.push(entry),
        }
    }
}

/// Retain answer constraints, excluding transient identity, secret defaults and
/// the required flag. The user ruled (2026-11-13) that a form differing only by
/// required versus optional repeats the earlier question, so the flag never
/// reaches the classifier and cannot make two forms differ.
fn question_evidence(question: &ElicitationRequest) -> serde_json::Value {
    let mut evidence = serde_json::to_value(question).expect("elicitation serializes");
    evidence
        .as_object_mut()
        .expect("elicitation is an object")
        .remove("id");
    for (field, original) in evidence["fields"]
        .as_array_mut()
        .expect("elicitation fields are an array")
        .iter_mut()
        .zip(&question.fields)
    {
        field
            .as_object_mut()
            .expect("elicitation field is an object")
            .remove("required");
        if question.field_is_secret(original) {
            field
                .as_object_mut()
                .expect("elicitation field is an object")
                .remove("default");
            field["secret"] = true.into();
        }
    }
    evidence
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum AwayRequest {
    Question,
    Plan,
    Tool(String),
}

impl OperatorAway {
    pub(crate) fn keep_fixture_question_live(&self, question: ElicitationRequest) -> Box<dyn Send> {
        let bridge = self.pending_for_bridge_with_source(Some(VerdictSource::Disabled), None);
        let answer = bridge.register(question, String::new(), AwayRequest::Question);
        Box::new((bridge, answer))
    }

    pub(crate) fn live_questions(&self) -> BTreeMap<String, ElicitationRequest> {
        // Recovery calls this with the relay held: Relay -> OperatorAway is
        // the lock order. This owner never takes the relay lock while held;
        // answering only wakes a responder, whose FIFO work runs afterwards.
        let state = self.0.lock().expect("operator-away state lock poisoned");
        state
            .pending
            .iter()
            .filter(|entry| state.bridges.contains(&entry.bridge) && entry.responder.is_alive())
            .map(|entry| (entry.question.id.clone(), entry.question.clone()))
            .collect()
    }

    // Only the Unix worker runtime receives SetOperatorAway. Other targets
    // carry the default state so ACP request handling has one type.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn set(&self, presence: mj_core::state::OperatorPresence) {
        let mut state = self.0.lock().expect("operator-away state lock poisoned");
        if presence.revision > state.presence.revision {
            state.presence = presence;
            state.refused.clear();
            if presence.away {
                // Vec order is registration order, including across bridge scopes.
                let pending = std::mem::take(&mut state.pending);
                for entry in pending {
                    if entry
                        .responder
                        .acp_identity()
                        .is_some_and(|identity| !state.refused.contains_key(identity))
                    {
                        state.answer_away(entry);
                    } else {
                        state.pending.push(entry);
                    }
                }
            }
            state.changed.send_replace(());
        }
    }

    #[cfg(test)]
    pub(super) fn pending_for_bridge(&self) -> PendingElicitations {
        self.pending_for_bridge_with_source(Some(VerdictSource::Disabled), None)
    }

    pub(super) fn pending_for_bridge_with_source(
        &self,
        source: Option<VerdictSource>,
        turn_context: Option<mj_transcript::turn_context::TurnContext>,
    ) -> PendingElicitations {
        let mut state = self.0.lock().expect("operator-away state lock poisoned");
        let id = state.next_bridge;
        state.next_bridge = id.checked_add(1).expect("ACP bridge identity exhausted");
        state.bridges.insert(id);
        PendingElicitations(Arc::new(PendingBridge {
            owner: self.clone(),
            id,
            source,
            turn_context,
        }))
    }
}

/// A bridge's live responders share the presence owner's serialized decisions.
#[derive(Clone)]
pub(super) struct PendingElicitations(Arc<PendingBridge>);

struct PendingBridge {
    owner: OperatorAway,
    id: u64,
    source: Option<VerdictSource>,
    /// Owns the session id and the decision log the AFK check records into.
    turn_context: Option<mj_transcript::turn_context::TurnContext>,
}

impl Drop for PendingBridge {
    fn drop(&mut self) {
        let mut state = self
            .owner
            .0
            .lock()
            .expect("operator-away state lock poisoned");
        state.bridges.remove(&self.id);
        state.pending.retain(|entry| entry.bridge != self.id);
        state.forget_reviews(self.id);
    }
}

impl OperatorAwayState {
    /// Withdraw a bridge's reviews in flight and forget its turn.
    fn forget_reviews(&mut self, bridge: u64) {
        self.reviews.retain(|review| {
            let keep = review.bridge != bridge;
            if !keep {
                review.withdrawn.cancel();
            }
            keep
        });
        self.withdrawn_turns.remove(&bridge);
    }
}

/// A Mjolnir guardian review registered as an outstanding tool permission
/// request. Dropping it, when the review ends, unregisters it.
pub(super) struct PendingReviewTicket {
    owner: OperatorAway,
    bridge: u64,
    id: u64,
    withdrawn: tokio_util::sync::CancellationToken,
}

impl PendingReviewTicket {
    /// Resolves when the turn's tool permission requests are withdrawn.
    pub(super) async fn withdrawn(&self) {
        self.withdrawn.cancelled().await;
    }

    /// Answer the request under the lock that withdraws turns: `answer` learns
    /// whether the turn was withdrawn, so no verdict is sent after a cancel
    /// the agent has already been told about. `answer` must not take that
    /// lock itself; an ACP responder only queues a message.
    pub(super) fn settle<T>(self, answer: impl FnOnce(bool) -> T) -> T {
        let state = self
            .owner
            .0
            .lock()
            .expect("operator-away state lock poisoned");
        let withdrawn = self.withdrawn.is_cancelled()
            || state.withdrawn_turns.contains(&self.bridge)
            || !state.reviews.iter().any(|review| review.id == self.id);
        answer(withdrawn)
    }
}

impl Drop for PendingReviewTicket {
    fn drop(&mut self) {
        self.owner
            .0
            .lock()
            .expect("operator-away state lock poisoned")
            .reviews
            .retain(|review| review.id != self.id);
    }
}

/// The connection, rather than its cloned responder tasks, owns bridge life.
/// Dropping a cancelled driver retires responders before terminal teardown.
pub(super) struct PendingBridgeLifetime(PendingElicitations);

impl Drop for PendingBridgeLifetime {
    fn drop(&mut self) {
        self.0.close();
    }
}

impl PendingElicitations {
    pub(super) fn bridge_lifetime(&self) -> PendingBridgeLifetime {
        PendingBridgeLifetime(self.clone())
    }

    pub(super) fn register_command_loop(&self, request: ElicitationRequest) -> CommandLoopQuestion {
        let lifetime = Arc::new(());
        let mut state = self
            .0
            .owner
            .0
            .lock()
            .expect("operator-away state lock poisoned");
        if state.bridges.contains(&self.0.id) {
            state.pending.push(PendingElicitation {
                bridge: self.0.id,
                question: request.clone(),
                responder: PendingResponder::CommandLoop(Arc::downgrade(&lifetime)),
            });
        }
        CommandLoopQuestion {
            request,
            owner: self.0.owner.clone(),
            bridge: self.0.id,
            lifetime,
        }
    }

    /// Both immediate and later AFK decisions reach the same responder task.
    /// It publishes Requested before receiving this answer, so AFK cannot
    /// resolve a form before the relay has observed the question.
    pub(super) fn register(
        &self,
        question: ElicitationRequest,
        agent: String,
        request: AwayRequest,
    ) -> oneshot::Receiver<PendingAnswer> {
        let (answer, received) = oneshot::channel();
        let mut state = self
            .0
            .owner
            .0
            .lock()
            .expect("operator-away state lock poisoned");
        if !state.bridges.contains(&self.0.id)
            || (state.withdrawn_turns.contains(&self.0.id)
                && question.id.starts_with(TOOL_PERMISSION_ID_PREFIX))
        {
            // Dropping the answer channel answers the request `cancelled`.
            return received;
        }
        let identity = (agent, request);
        if state.presence.away && !state.refused.contains_key(&identity) {
            state.answer_away(PendingElicitation {
                bridge: self.0.id,
                question,
                responder: PendingResponder::Acp { identity, answer },
            });
        } else {
            state.pending.push(PendingElicitation {
                bridge: self.0.id,
                question,
                responder: PendingResponder::Acp { identity, answer },
            });
        }
        received
    }

    /// The responder owns this future. Human answers and cancellation drop its
    /// HTTP work; only the presence owner may accept the resulting answer.
    pub(super) async fn refine_question(&self, id: &str) {
        let mut changed = self
            .0
            .owner
            .0
            .lock()
            .expect("operator-away state lock poisoned")
            .changed
            .subscribe();
        let mut client = None;
        loop {
            changed.borrow_and_update();
            let snapshot = {
                let state = self
                    .0
                    .owner
                    .0
                    .lock()
                    .expect("operator-away state lock poisoned");
                let Some(entry) = state
                    .pending
                    .iter()
                    .find(|entry| entry.bridge == self.0.id && entry.question.id == id)
                else {
                    return;
                };
                let Some(identity) = entry.responder.acp_identity() else {
                    return;
                };
                if identity.1 != AwayRequest::Question {
                    return;
                }
                state.presence.away.then(|| {
                    (
                        state.presence.revision,
                        identity.0.clone(),
                        question_evidence(&entry.question),
                        state.refused.get(identity).cloned().unwrap_or_default(),
                    )
                })
            };
            if let Some((revision, agent, question, history)) = snapshot {
                let classify = async {
                    if client.is_none() {
                        client =
                            verdict_client::VerdictClient::resolve(self.0.source.as_ref()).await;
                    }
                    // Read at classification time: the log opens after the bridge starts.
                    let (session, log) = self.0.turn_context.as_ref().map_or_else(
                        || (String::new(), None),
                        |context| (context.session_id(), context.decision_log()),
                    );
                    match &client {
                        Some(client) => {
                            client
                                .clone()
                                .with_log(log)
                                .same_afk_question(&session, &question, &history)
                                .await
                        }
                        None => Err(anyhow!("the classifier is disabled or unavailable")),
                    }
                };
                let verdict = tokio::select! {
                    result = classify => result,
                    result = changed.changed() => { if result.is_err() { return; } continue; }
                };
                match verdict {
                    Ok(false) => {
                        let mut state = self
                            .0
                            .owner
                            .0
                            .lock()
                            .expect("operator-away state lock poisoned");
                        // Compare and consume under the same owner lock. Concurrent
                        // new questions must reclassify against the accepted history.
                        if state.presence.away
                            && state.presence.revision == revision
                            && state
                                .refused
                                .get(&(agent, AwayRequest::Question))
                                .is_some_and(|current| *current == history)
                            && let Some(index) = state.pending.iter().position(|entry| {
                                entry.bridge == self.0.id
                                    && entry.question.id == id
                                    && entry.responder.acp_identity().is_some()
                            })
                        {
                            let entry = state.pending.remove(index);
                            state.answer_away(entry);
                            tracing::info!(target: "mj_jev", %id, revision, "Jev identified a new AFK question");
                            return;
                        }
                        continue;
                    }
                    Ok(true) => {
                        tracing::info!(target: "mj_jev", %id, revision, "Jev identified a repeated AFK question")
                    }
                    Err(error) => {
                        tracing::warn!(target: "mj_jev", %id, %error, "AFK question classification unavailable; retaining existing behavior")
                    }
                }
            }
            if changed.changed().await.is_err() {
                return;
            }
        }
    }

    pub(super) fn remove(&self, id: &str) -> Option<PendingElicitation> {
        let mut state = self
            .0
            .owner
            .0
            .lock()
            .expect("operator-away state lock poisoned");
        let index = state.pending.iter().position(|entry| {
            entry.bridge == self.0.id
                && entry.question.id == id
                && entry.responder.acp_identity().is_some()
        })?;
        let entry = state.pending.remove(index);
        Some(entry)
    }

    /// Claim Cancel before stopping the turn, so a resolved form cannot stop it.
    /// The caller keeps the sender until after sending session/cancel.
    pub(super) fn take_turn_cancel(
        &self,
        id: &str,
        response: &ElicitationResponse,
    ) -> Option<PendingElicitation> {
        if *response != ElicitationResponse::Cancel || !id.starts_with(TOOL_PERMISSION_ID_PREFIX) {
            return None;
        }
        self.remove(id)
    }

    /// Register a Mjolnir guardian review of a tool permission request, so the
    /// turn's withdrawal reaches it as it reaches an open form. `None` when the
    /// bridge is gone or its turn was already withdrawn: answer `cancelled`.
    pub(super) fn begin_review(&self) -> Option<PendingReviewTicket> {
        let mut state = self
            .0
            .owner
            .0
            .lock()
            .expect("operator-away state lock poisoned");
        if !state.bridges.contains(&self.0.id) || state.withdrawn_turns.contains(&self.0.id) {
            return None;
        }
        let id = state.next_review;
        state.next_review = id
            .checked_add(1)
            .expect("guardian review identity exhausted");
        let withdrawn = tokio_util::sync::CancellationToken::new();
        state.reviews.push(PendingReview {
            bridge: self.0.id,
            id,
            withdrawn: withdrawn.clone(),
        });
        Some(PendingReviewTicket {
            owner: self.0.owner.clone(),
            bridge: self.0.id,
            id,
            withdrawn,
        })
    }

    /// Mark the turn withdrawn before the agent is told of the cancel, so a
    /// guardian review that settles in between answers `cancelled`, and no
    /// request arriving later in the turn is reviewed or shown.
    pub(super) fn begin_withdrawal(&self) {
        self.0
            .owner
            .0
            .lock()
            .expect("operator-away state lock poisoned")
            .withdrawn_turns
            .insert(self.0.id);
    }

    /// Withdraw the turn's tool permission requests in one decision: drop the
    /// answer channel of every open form (its task answers the agent
    /// `cancelled` and closes the form), cancel every guardian review in
    /// flight, and answer any that arrives later in this turn `cancelled`.
    pub(super) fn withdraw_tool_permissions(&self) {
        let mut state = self
            .0
            .owner
            .0
            .lock()
            .expect("operator-away state lock poisoned");
        state.pending.retain(|entry| {
            entry.bridge != self.0.id
                || entry.responder.acp_identity().is_none()
                || !entry.question.id.starts_with(TOOL_PERMISSION_ID_PREFIX)
        });
        state.reviews.retain(|review| {
            let keep = review.bridge != self.0.id;
            if !keep {
                review.withdrawn.cancel();
            }
            keep
        });
        state.withdrawn_turns.insert(self.0.id);
    }

    /// The withdrawn turn is over: its prompt answered, or a new one began.
    pub(super) fn end_withdrawal(&self) {
        self.0
            .owner
            .0
            .lock()
            .expect("operator-away state lock poisoned")
            .withdrawn_turns
            .remove(&self.0.id);
    }

    /// Whether the only outstanding requests are guardian reviews, which end
    /// on their own within the review deadline.
    pub(super) fn only_reviews(&self) -> bool {
        let state = self
            .0
            .owner
            .0
            .lock()
            .expect("operator-away state lock poisoned");
        state
            .reviews
            .iter()
            .any(|review| review.bridge == self.0.id)
            && !state
                .pending
                .iter()
                .any(|entry| entry.bridge == self.0.id && entry.responder.acp_identity().is_some())
    }

    pub(super) fn is_empty(&self) -> bool {
        let state = self
            .0
            .owner
            .0
            .lock()
            .expect("operator-away state lock poisoned");
        !state
            .pending
            .iter()
            .any(|entry| entry.bridge == self.0.id && entry.responder.acp_identity().is_some())
            && !state
                .reviews
                .iter()
                .any(|review| review.bridge == self.0.id)
    }

    pub(super) fn close(&self) {
        let mut state = self
            .0
            .owner
            .0
            .lock()
            .expect("operator-away state lock poisoned");
        state.bridges.remove(&self.0.id);
        state.pending.retain(|entry| entry.bridge != self.0.id);
        state.forget_reviews(self.0.id);
    }

    pub(super) fn resolve(
        &self,
        id: &str,
        response: ElicitationResponse,
    ) -> std::result::Result<(), String> {
        let mut state = self
            .0
            .owner
            .0
            .lock()
            .expect("operator-away state lock poisoned");
        let index = state
            .pending
            .iter()
            .position(|entry| {
                entry.bridge == self.0.id
                    && entry.question.id == id
                    && entry.responder.acp_identity().is_some()
            })
            .ok_or_else(|| format!("elicitation {id:?} is no longer pending"))?;
        let response = state.pending[index].question.harness_response(response)?;
        let entry = state.pending.remove(index);
        match entry.responder {
            PendingResponder::Acp { answer, .. } => answer
                .send(PendingAnswer::Human(response))
                .map_err(|_| format!("elicitation {id:?} was cancelled before it was answered")),
            PendingResponder::CommandLoop(_) => {
                Err(format!("elicitation {id:?} belongs to the command loop"))
            }
        }
    }
}

#[cfg(test)]
impl Default for PendingElicitations {
    fn default() -> Self {
        OperatorAway::default().pending_for_bridge()
    }
}

#[derive(Debug)]
pub(super) struct PendingElicitation {
    bridge: u64,
    question: ElicitationRequest,
    responder: PendingResponder,
}

#[derive(Debug)]
enum PendingResponder {
    Acp {
        identity: (String, AwayRequest),
        answer: oneshot::Sender<PendingAnswer>,
    },
    CommandLoop(std::sync::Weak<()>),
}

impl PendingResponder {
    fn acp_identity(&self) -> Option<&(String, AwayRequest)> {
        match self {
            Self::Acp { identity, .. } => Some(identity),
            Self::CommandLoop(_) => None,
        }
    }

    fn is_alive(&self) -> bool {
        match self {
            Self::Acp { answer, .. } => !answer.is_closed(),
            Self::CommandLoop(lifetime) => lifetime.upgrade().is_some(),
        }
    }
}

/// A recovery choice is answered by the command loop, not ACP or AFK policy.
/// Its actual local owner keeps this wrapper until resolution or cancellation.
pub(super) struct CommandLoopQuestion {
    request: ElicitationRequest,
    owner: OperatorAway,
    bridge: u64,
    lifetime: Arc<()>,
}

impl std::ops::Deref for CommandLoopQuestion {
    type Target = ElicitationRequest;
    fn deref(&self) -> &Self::Target {
        &self.request
    }
}

impl Drop for CommandLoopQuestion {
    fn drop(&mut self) {
        let lifetime = Arc::downgrade(&self.lifetime);
        let mut state = self
            .owner
            .0
            .lock()
            .expect("operator-away state lock poisoned");
        state.pending.retain(|entry| {
            entry.bridge != self.bridge
                || !matches!(&entry.responder,
                PendingResponder::CommandLoop(token) if token.ptr_eq(&lifetime))
        });
    }
}

impl PendingElicitation {
    /// Preserve explicit user intent instead of conflating it with a dropped bridge.
    pub(super) fn cancel(self) {
        if let PendingResponder::Acp { answer, .. } = self.responder
            && answer
                .send(PendingAnswer::Human(ElicitationResponse::Cancel))
                .is_err()
        {
            tracing::debug!(elicitation_id = %self.question.id, "permission responder closed before cancellation was answered");
        }
    }
}

#[derive(Debug)]
pub(super) enum PendingAnswer {
    Human(ElicitationResponse),
    OperatorAway,
}

impl PendingAnswer {
    pub(super) fn action_name(&self) -> &str {
        match self {
            Self::Human(response) => response.action_name(),
            Self::OperatorAway => mj_core::elicitation::OPERATOR_AWAY_ACTION,
        }
    }

    pub(super) fn human(&self) -> Option<&ElicitationResponse> {
        match self {
            Self::Human(response) => Some(response),
            Self::OperatorAway => None,
        }
    }

    pub(super) fn harness_response(self) -> ElicitationResponse {
        match self {
            Self::Human(response) => response,
            Self::OperatorAway => ElicitationResponse::Decline,
        }
    }
}

pub(super) fn away_permission_identity(
    request: &RequestPermissionRequest,
    input: Option<serde_json::Value>,
) -> AwayRequest {
    let mut identity = serde_json::json!({
        "kind": request.tool_call.fields.kind,
        "title": request.tool_call.fields.title,
        "input": input,
    });
    // ACP metadata may enable serde_json's preserve_order feature elsewhere.
    identity.sort_all_objects();
    AwayRequest::Tool(identity.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};

    // Hard-won: c1da39f: the last terminal closed before a dead bridge's form-resolution event.
    #[cfg(unix)]
    #[test]
    fn recovery_cut_asks_responder_owner_before_terminal_close_releases_it() {
        use mj_core::relay::{ActiveAgentTerminal, RelayObservation};
        let root = tempfile::tempdir().unwrap();
        let relay = Arc::new(crate::relay::RelayCell::new(
            crate::relay::DurableRelay::open(root.path(), "question-cut", "test").unwrap(),
        ));
        let owner = relay.lock().unwrap().operator_away.clone();
        let old = owner.pending_for_bridge();
        let old_lifetime = old.bridge_lifetime();
        let replacement = owner.pending_for_bridge();
        let _replacement_lifetime = replacement.bridge_lifetime();
        let old_question = question("old", "Old bridge question");
        let live_question = question("live", "Replacement bridge question");
        let abandoned = question("abandoned", "Responder disappeared");
        let old_reused_question = question("reused", "Previous bridge's form");
        let current_reused_question = question("reused", "Replacement bridge's different form");
        let _reused_answer = replacement.register(
            current_reused_question.clone(),
            String::new(),
            AwayRequest::Question,
        );

        let _old_answer = old.register(old_question.clone(), String::new(), AwayRequest::Question);
        let _live_answer =
            replacement.register(live_question.clone(), String::new(), AwayRequest::Question);
        drop(replacement.register(abandoned.clone(), String::new(), AwayRequest::Question));
        {
            let mut state = relay.lock().unwrap();
            for request in [
                old_question.clone(),
                live_question.clone(),
                abandoned,
                old_reused_question,
            ] {
                state
                    .record_observation(RelayObservation::ElicitationRequested { request })
                    .unwrap();
            }
            assert_eq!(
                state.live_session_baseline().unwrap().questions,
                vec![live_question.clone(), old_question]
            );
            state
                .agent_terminal_started(ActiveAgentTerminal {
                    terminal_id: "old-terminal".into(),
                    command: "sleep 30".into(),
                    started_at_ms: 1,
                })
                .unwrap();
        }
        let mut cut = relay.lock().unwrap().reserve_live_baseline();
        assert!(cut.try_recv().is_err());
        // Responder clones and their resolution tasks still exist. The actual
        // bridge lifetime owner is decisive even without HarnessRestarting.
        drop(old_lifetime);
        crate::worker_runtime::record_runtime_event(
            &relay,
            &mut BTreeMap::new(),
            RuntimeEvent::TerminalClosed {
                terminal_id: "old-terminal".into(),
                output: String::new(),
                truncated: false,
                exit_code: Some(0),
                signal: None,
            },
        )
        .unwrap();
        assert_eq!(
            cut.try_recv().unwrap().unwrap().questions,
            vec![live_question.clone()]
        );
        // A restarted bridge can reuse IDs. Its live responder cannot bless
        // the old body before the new Requested event reaches the frontier.
        relay
            .lock()
            .unwrap()
            .record_observation(RelayObservation::ElicitationRequested {
                request: current_reused_question.clone(),
            })
            .unwrap();
        assert_eq!(
            relay
                .lock()
                .unwrap()
                .live_session_baseline()
                .unwrap()
                .questions,
            vec![live_question, current_reused_question]
        );
        let history = relay
            .lock()
            .unwrap()
            .events_after(0, mj_core::relay::RELAY_EVENT_GENESIS_DIGEST)
            .unwrap();
        assert!(!history.iter().any(|event| matches!(
            event.observation,
            RelayObservation::ElicitationResolved { .. }
        )));
    }

    struct Classifier {
        listener: TcpListener,
        source: VerdictSource,
    }

    impl Classifier {
        async fn new() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/", listener.local_addr().unwrap());
            Self {
                listener,
                source: VerdictSource::Hosted { endpoint },
            }
        }

        async fn request(&self) -> (TcpStream, Value) {
            let (stream, _) = self.listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut headers = String::new();
            let mut length = None;
            loop {
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                assert!(
                    !line.is_empty(),
                    "classifier client disconnected before headers"
                );
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                    length = Some(value.trim().parse::<usize>().unwrap());
                }
                headers.push_str(&line);
            }
            let mut body = vec![0; length.unwrap()];
            stream.read_exact(&mut body).await.unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert!(headers.starts_with("POST /v1/afk-question-glide "));
            // The hosted proxy owns the question: the body is the state alone.
            assert_eq!(body.as_object().unwrap().len(), 2, "{body}");
            (stream.into_inner(), body)
        }

        async fn respond(&self, mut stream: TcpStream, choice: &str) {
            let (same, different) = if choice == "same" {
                (0.99, 0.01)
            } else {
                (0.01, 0.99)
            };
            let body = json!({"probabilities":{"same":same,"different":different}}).to_string();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    }

    fn question(id: &str, text: &str) -> ElicitationRequest {
        ElicitationRequest {
            id: id.into(),
            message: text.into(),
            title: None,
            description: None,
            fields: vec![],
            tool_permission: None,
        }
    }

    fn ask(
        pending: &PendingElicitations,
        id: &str,
        text: &str,
    ) -> tokio::task::JoinHandle<Option<PendingAnswer>> {
        ask_form(pending, question(id, text))
    }

    fn ask_form(
        pending: &PendingElicitations,
        question: ElicitationRequest,
    ) -> tokio::task::JoinHandle<Option<PendingAnswer>> {
        let id = question.id.clone();
        let answer = pending.register(question, String::new(), AwayRequest::Question);
        let pending = pending.clone();
        mj_core::runtime::spawn(async move {
            await_elicitation_response(&pending, &id, answer, std::future::pending()).await
        })
    }

    // Hard-won: #1489: new questions shared the generic Question refusal identity.
    #[tokio::test]
    async fn afk_question_protocol_distinguishes_mixed_forms_and_preserves_unavailable_fallback() {
        let classifier = Classifier::new().await;
        let owner = OperatorAway::default();
        let pending = owner.pending_for_bridge_with_source(Some(classifier.source.clone()), None);
        owner.set(mj_core::state::OperatorPresence {
            away: true,
            revision: 1,
        });
        assert!(matches!(
            ask(&pending, "first", "Who owns #10941?").await.unwrap(),
            Some(PendingAnswer::OperatorAway)
        ));
        let new = ask(&pending, "mixed", "Who owns #11482, and who owns #10941?");
        let (stream, evidence) = classifier.request().await;
        assert_eq!(
            evidence,
            json!({"current_question":{"message":"Who owns #11482, and who owns #10941?", "fields":[]}, "previous_questions":[{"message":"Who owns #10941?", "fields":[]}]})
        );
        classifier.respond(stream, "different").await;
        assert!(matches!(
            new.await.unwrap(),
            Some(PendingAnswer::OperatorAway)
        ));

        let repeated = ask(&pending, "repeat", "Please assign #10941.");
        let (stream, evidence) = classifier.request().await;
        assert_eq!(
            evidence["previous_questions"],
            json!([{"message":"Who owns #10941?", "fields":[]}, {"message":"Who owns #11482, and who owns #10941?", "fields":[]}])
        );
        classifier.respond(stream, "same").await;
        pending
            .resolve("repeat", ElicitationResponse::Decline)
            .unwrap();
        assert!(matches!(
            repeated.await.unwrap(),
            Some(PendingAnswer::Human(_))
        ));

        for body in [
            "HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
        ] {
            let fallback = ask(&pending, "fallback", "Who owns #12000?");
            let (mut stream, _) = classifier.request().await;
            stream.write_all(body.as_bytes()).await.unwrap();
            // Wait until the network attempt has consumed the response before
            // answering, so this proves fallback rather than winning a race.
            let mut eof = Vec::new();
            stream.read_to_end(&mut eof).await.unwrap();
            pending
                .resolve("fallback", ElicitationResponse::Decline)
                .unwrap();
            assert!(matches!(
                fallback.await.unwrap(),
                Some(PendingAnswer::Human(_))
            ));
        }
    }

    // Hard-won: #1489 review: evidence lost constraints and leaked secret defaults/choices.
    #[tokio::test]
    async fn afk_question_evidence_preserves_changed_selection_without_the_required_flag() {
        let classifier = Classifier::new().await;
        let owner = OperatorAway::default();
        let pending = owner.pending_for_bridge_with_source(Some(classifier.source.clone()), None);
        owner.set(mj_core::state::OperatorPresence {
            away: true,
            revision: 1,
        });
        let first = ElicitationRequest::from_acp_params(
            "first",
            json!({
                "mode":"form", "sessionId":"test", "message":"Choose a destination",
            "requestedSchema":{"type":"object", "properties":{
                "destination":{"type":"string", "title":"Destination", "enum":["one","two"]},
                "password":{"type":"string", "title":"Password", "enum":["P!n12","n3w?Pin"], "default":"P!n12", "_meta":{"codex":{"isSecret":true}}},
                "password__other":{"type":"string", "title":"Other password", "default":"77!Z", "_meta":{"codex":{"questionId":"password", "role":"user_note"}}}
            }}
            }),
        )
        .unwrap();
        let second = ElicitationRequest::from_acp_params("second", json!({
            "mode":"form", "sessionId":"test", "message":"Choose a destination",
            "requestedSchema":{"type":"object", "required":["destination"], "properties":{
                "destination":{"type":"array", "title":"Destination", "items":{"type":"string", "enum":["one","two"]}, "minItems":1},
                "password":{"type":"string", "title":"Password", "enum":["P!n12","n3w?Pin"], "default":"n3w?Pin", "_meta":{"codex":{"isSecret":true}}},
                "password__other":{"type":"string", "title":"Other password", "default":"55?X", "_meta":{"codex":{"questionId":"password", "role":"user_note"}}}
            }}
        })).unwrap();
        assert_eq!(
            first.question_text(),
            second.question_text(),
            "the old evidence lost the change"
        );
        assert!(matches!(
            ask_form(&pending, first).await.unwrap(),
            Some(PendingAnswer::OperatorAway)
        ));
        let second = ask_form(&pending, second);
        let (stream, evidence) = classifier.request().await;
        assert!(!evidence.to_string().contains("P!n12"));
        assert!(!evidence.to_string().contains("n3w?Pin"));
        assert!(!evidence.to_string().contains("77!Z"));
        assert!(!evidence.to_string().contains("55?X"));
        assert!(
            evidence["current_question"]["fields"][2]
                .get("default")
                .is_none()
        );
        assert!(
            evidence["previous_questions"][0]["fields"][2]
                .get("default")
                .is_none()
        );
        assert_eq!(evidence["current_question"]["fields"][2]["secret"], true);
        assert_eq!(
            evidence["current_question"]["fields"][1]["options"],
            evidence["previous_questions"][0]["fields"][1]["options"]
        );
        assert_ne!(
            evidence["current_question"]["fields"][1]["options"][0]["value"],
            evidence["current_question"]["fields"][1]["options"][1]["value"]
        );
        assert!(
            evidence["current_question"]["fields"][1]
                .get("default")
                .is_none()
        );
        assert!(
            evidence["previous_questions"][0]["fields"][1]
                .get("default")
                .is_none()
        );
        assert_eq!(evidence["current_question"]["fields"][1]["secret"], true);
        assert_eq!(
            evidence["current_question"]["fields"][0]["kind"],
            "multi_select"
        );
        // Required versus optional alone is a repeat (user rule, 2026-11-13).
        assert!(
            evidence["current_question"]["fields"][0]
                .get("required")
                .is_none()
        );
        assert_eq!(evidence["current_question"]["fields"][0]["min_items"], 1);
        assert_eq!(
            evidence["previous_questions"][0]["fields"][0]["kind"],
            "single_select"
        );
        assert!(
            evidence["previous_questions"][0]["fields"][0]
                .get("required")
                .is_none()
        );
        classifier.respond(stream, "different").await;
        assert!(matches!(
            second.await.unwrap(),
            Some(PendingAnswer::OperatorAway)
        ));
    }

    // Hard-won: #1489 review: redaction changed ID identity across escapes and punctuation.
    #[tokio::test]
    async fn afk_question_protocol_preserves_redacted_identifier_equality() {
        let classifier = Classifier::new().await;
        let owner = OperatorAway::default();
        let pending = owner.pending_for_bridge_with_source(Some(classifier.source.clone()), None);
        owner.set(mj_core::state::OperatorPresence {
            away: true,
            revision: 1,
        });
        let first_id = "a".repeat(40);
        let second_id = "b".repeat(40);
        assert!(matches!(
            ask(&pending, "first", &format!("Merge commit {first_id}?"))
                .await
                .unwrap(),
            Some(PendingAnswer::OperatorAway)
        ));
        let second = ask(&pending, "second", &format!("Merge commit\n{second_id}?"));
        let (stream, evidence) = classifier.request().await;
        let serialized = evidence.to_string();
        assert!(!serialized.contains(&first_id) && !serialized.contains(&second_id));
        assert_eq!(
            evidence["current_question"]["message"],
            "Merge commit\n[redacted token 0]?"
        );
        assert_eq!(
            evidence["previous_questions"][0]["message"],
            "Merge commit [redacted token 1]?"
        );
        classifier.respond(stream, "different").await;
        assert!(matches!(
            second.await.unwrap(),
            Some(PendingAnswer::OperatorAway)
        ));
        let repeated = ask(&pending, "repeated", &format!("Merge commit\t{first_id}."));
        let (stream, evidence) = classifier.request().await;
        assert_eq!(
            evidence["current_question"]["message"],
            "Merge commit\t[redacted token 0]."
        );
        assert_eq!(
            evidence["previous_questions"][0]["message"],
            "Merge commit [redacted token 0]?"
        );
        assert_ne!(
            evidence["current_question"],
            evidence["previous_questions"][1]
        );
        classifier.respond(stream, "same").await;
        pending
            .resolve("repeated", ElicitationResponse::Decline)
            .unwrap();
        assert!(matches!(
            repeated.await.unwrap(),
            Some(PendingAnswer::Human(_))
        ));
        // A serialized newline plus a 23-character run used to cross the
        // 24-character redaction boundary and produce an invalid JSON escape.
        let short_message = format!("Merge commit\n{}?", "c".repeat(23));
        let short = ask(&pending, "short", &short_message);
        let (stream, evidence) = classifier.request().await;
        assert_eq!(evidence["current_question"]["message"], short_message);
        classifier.respond(stream, "different").await;
        assert!(matches!(
            short.await.unwrap(),
            Some(PendingAnswer::OperatorAway)
        ));
    }

    #[tokio::test]
    async fn afk_question_decisions_are_recorded_with_the_redacted_request_and_outcome() {
        let classifier = Classifier::new().await;
        let directory = tempfile::tempdir().unwrap();
        let context = mj_transcript::turn_context::TurnContext::default();
        context.set_session_id("afk-session");
        context.set_decision_log(mj_core::jev::DecisionLog::open(directory.path().into()).unwrap());
        let owner = OperatorAway::default();
        let pending =
            owner.pending_for_bridge_with_source(Some(classifier.source.clone()), Some(context));
        owner.set(mj_core::state::OperatorPresence {
            away: true,
            revision: 1,
        });
        const TOKEN: &str = "ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789";
        assert!(matches!(
            ask(&pending, "first", &format!("Use {TOKEN}?"))
                .await
                .unwrap(),
            Some(PendingAnswer::OperatorAway)
        ));
        let new = ask(&pending, "new", "Who owns #11482?");
        let (stream, _) = classifier.request().await;
        classifier.respond(stream, "different").await;
        assert!(matches!(
            new.await.unwrap(),
            Some(PendingAnswer::OperatorAway)
        ));
        let repeat = ask(&pending, "repeat", "Please assign #11482.");
        let (stream, _) = classifier.request().await;
        classifier.respond(stream, "same").await;
        let failed = ask(&pending, "failed", "Another decision?");
        let (mut stream, _) = classifier.request().await;
        stream
            .write_all(
                b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        let mut eof = Vec::new();
        stream.read_to_end(&mut eof).await.unwrap();
        // "repeat" was held and still waits; resolve both so the tasks end.
        for id in ["repeat", "failed"] {
            pending.resolve(id, ElicitationResponse::Decline).unwrap();
        }
        repeat.await.unwrap();
        failed.await.unwrap();
        let mut page = mj_core::jev::read(directory.path(), "afk-session", None).unwrap();
        page.decisions
            .sort_by_key(|d| (d.started_at_ms, d.id.clone()));
        let outcomes: Vec<_> = page.decisions.iter().map(|d| d.status.as_str()).collect();
        assert_eq!(outcomes, ["declined_new", "held_repeat", "failed"]);
        // The list omits technical details; read each record by id.
        let mut records = Vec::new();
        for summary in &page.decisions {
            let detail =
                mj_core::jev::read(directory.path(), "afk-session", Some(&summary.id)).unwrap();
            records.push(detail.decisions.into_iter().next().unwrap());
        }
        for record in &records {
            assert_eq!(record.kind, "activity");
            assert_eq!(
                record.checked,
                "Does this question repeat one already declined while away?"
            );
            let technical = record.technical.as_ref().unwrap();
            assert_eq!(technical["contract"], "afk-question-glide-v1");
            assert!(!technical.to_string().contains(TOKEN), "{technical}");
            assert!(
                technical["request"]["previous_questions"][0]["message"]
                    .as_str()
                    .unwrap()
                    .contains("[redacted token"),
                "{technical}"
            );
        }
        assert_eq!(
            records[0].technical.as_ref().unwrap()["probabilities"],
            json!({"same":0.01,"different":0.99})
        );
        assert!(
            records[2].technical.as_ref().unwrap()["error"]
                .as_str()
                .unwrap()
                .contains("413")
        );
    }

    #[tokio::test]
    async fn afk_question_classification_cannot_outlive_human_cancel_return_or_bridge_close() {
        for action in ["human", "cancel", "return", "close"] {
            let classifier = Classifier::new().await;
            let owner = OperatorAway::default();
            let pending =
                owner.pending_for_bridge_with_source(Some(classifier.source.clone()), None);
            owner.set(mj_core::state::OperatorPresence {
                away: true,
                revision: 1,
            });
            assert!(matches!(
                ask(&pending, "first", "First decision?").await.unwrap(),
                Some(PendingAnswer::OperatorAway)
            ));
            let answer = pending.register(
                question("racing", "Another decision?"),
                String::new(),
                AwayRequest::Question,
            );
            let (cancel, cancelled) = oneshot::channel();
            let response = {
                let pending = pending.clone();
                mj_core::runtime::spawn(async move {
                    await_elicitation_response(&pending, "racing", answer, async {
                        let _ = cancelled.await;
                    })
                    .await
                })
            };
            let (mut stream, _) = classifier.request().await;
            match action {
                "human" => pending
                    .resolve("racing", ElicitationResponse::Decline)
                    .unwrap(),
                "cancel" => cancel.send(()).unwrap(),
                "return" => owner.set(mj_core::state::OperatorPresence {
                    away: false,
                    revision: 2,
                }),
                "close" => pending.close(),
                _ => unreachable!(),
            }
            // Cancellation/return drops the in-flight HTTP request, rather than
            // waiting for Jev to release its response.
            let mut eof = Vec::new();
            stream.read_to_end(&mut eof).await.unwrap();
            if action == "return" {
                pending
                    .resolve("racing", ElicitationResponse::Decline)
                    .unwrap();
            }
            let outcome = response.await.unwrap();
            assert!(
                if matches!(action, "human" | "return") {
                    matches!(outcome, Some(PendingAnswer::Human(_)))
                } else {
                    outcome.is_none()
                },
                "{action}: {outcome:?}"
            );
        }
    }

    #[tokio::test]
    async fn concurrent_afk_questions_reclassify_after_another_question_is_answered() {
        let classifier = Classifier::new().await;
        let owner = OperatorAway::default();
        let pending = owner.pending_for_bridge_with_source(Some(classifier.source.clone()), None);
        owner.set(mj_core::state::OperatorPresence {
            away: true,
            revision: 1,
        });
        assert!(matches!(
            ask(&pending, "first", "First decision?").await.unwrap(),
            Some(PendingAnswer::OperatorAway)
        ));
        let one = ask(&pending, "one", "Who owns #11482?");
        let (stream_one, _) = classifier.request().await;
        let two = ask(&pending, "two", "Assign #11482 to someone?");
        let (mut stream_two, _) = classifier.request().await;
        classifier.respond(stream_one, "different").await;
        assert!(matches!(
            one.await.unwrap(),
            Some(PendingAnswer::OperatorAway)
        ));
        let mut eof = Vec::new();
        stream_two.read_to_end(&mut eof).await.unwrap();
        let (stream_two, evidence) = classifier.request().await;
        assert_eq!(
            evidence["previous_questions"],
            json!([{"message":"First decision?", "fields":[]}, {"message":"Who owns #11482?", "fields":[]}])
        );
        classifier.respond(stream_two, "same").await;
        pending
            .resolve("two", ElicitationResponse::Decline)
            .unwrap();
        assert!(matches!(two.await.unwrap(), Some(PendingAnswer::Human(_))));
    }
}
