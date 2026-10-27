use super::*;

/// Process-local operator availability and refusal memory, retained across bridge restarts.
#[derive(Debug, Clone, Default)]
pub struct OperatorAway(Arc<Mutex<OperatorAwayState>>);

#[derive(Debug, Default)]
struct OperatorAwayState {
    presence: mj_core::state::OperatorPresence,
    refused: BTreeSet<(String, AwayRequest)>,
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
        }
    }

    /// Availability and repeat identity have one serialized decision point.
    pub(super) fn refuse(&self, agent: String, request: AwayRequest) -> bool {
        let mut state = self.0.lock().expect("operator-away state lock poisoned");
        state.presence.away && state.refused.insert((agent, request))
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
