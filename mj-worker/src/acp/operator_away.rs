use super::*;

/// Process-local operator availability and refusal memory, retained across bridge restarts.
#[derive(Debug, Clone, Default)]
pub struct OperatorAway(Arc<Mutex<OperatorAwayState>>);

#[derive(Debug, Default)]
struct OperatorAwayState {
    presence: mj_core::state::OperatorPresence,
    refused: BTreeSet<(String, AwayRequest)>,
    next_bridge: u64,
    bridges: BTreeSet<u64>,
    pending: Vec<PendingElicitation>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum AwayRequest {
    Question,
    Plan,
    Tool(String),
}

impl OperatorAway {
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
                    if state.refused.insert(entry.identity.clone()) {
                        // A closed receiver no longer has a live ACP responder.
                        if entry.answer.send(PendingAnswer::OperatorAway).is_err() {
                            state.refused.remove(&entry.identity);
                        }
                    } else {
                        state.pending.push(entry);
                    }
                }
            }
        }
    }

    pub(super) fn pending_for_bridge(&self) -> PendingElicitations {
        let mut state = self.0.lock().expect("operator-away state lock poisoned");
        let id = state.next_bridge;
        state.next_bridge = id.checked_add(1).expect("ACP bridge identity exhausted");
        state.bridges.insert(id);
        PendingElicitations(Arc::new(PendingBridge {
            owner: self.clone(),
            id,
        }))
    }
}

/// A bridge's live responders share the presence owner's serialized decisions.
#[derive(Clone)]
pub(super) struct PendingElicitations(Arc<PendingBridge>);

struct PendingBridge {
    owner: OperatorAway,
    id: u64,
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
    }
}

impl PendingElicitations {
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
        if !state.bridges.contains(&self.0.id) {
            return received;
        }
        let identity = (agent, request);
        if state.presence.away && state.refused.insert(identity.clone()) {
            // The receiver is still local, so this delivery cannot fail.
            answer
                .send(PendingAnswer::OperatorAway)
                .expect("new answer receiver is open");
        } else {
            state.pending.push(PendingElicitation {
                bridge: self.0.id,
                question,
                identity,
                answer,
            });
        }
        received
    }

    pub(super) fn remove(&self, id: &str) -> Option<PendingElicitation> {
        let mut state = self
            .0
            .owner
            .0
            .lock()
            .expect("operator-away state lock poisoned");
        let index = state
            .pending
            .iter()
            .position(|entry| entry.bridge == self.0.id && entry.question.id == id)?;
        Some(state.pending.remove(index))
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

    pub(super) fn is_empty(&self) -> bool {
        let state = self
            .0
            .owner
            .0
            .lock()
            .expect("operator-away state lock poisoned");
        !state.pending.iter().any(|entry| entry.bridge == self.0.id)
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
    }

    pub(super) fn retain(&self, keep: impl Fn(&str) -> bool) {
        self.0
            .owner
            .0
            .lock()
            .expect("operator-away state lock poisoned")
            .pending
            .retain(|entry| entry.bridge != self.0.id || keep(&entry.question.id));
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
            .position(|entry| entry.bridge == self.0.id && entry.question.id == id)
            .ok_or_else(|| format!("elicitation {id:?} is no longer pending"))?;
        let response = state.pending[index].question.harness_response(response)?;
        let entry = state.pending.remove(index);
        entry
            .answer
            .send(PendingAnswer::Human(response))
            .map_err(|_| format!("elicitation {id:?} was cancelled before it was answered"))
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
    identity: (String, AwayRequest),
    answer: oneshot::Sender<PendingAnswer>,
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
