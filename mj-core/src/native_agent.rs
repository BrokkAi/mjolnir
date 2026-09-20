//! Harness-owned children share their owner's worker, but not its transcript.
use agent_client_protocol::schema::v1::SessionUpdate;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeAgentCapabilities {
    #[serde(default)]
    pub cancel: bool,
    #[serde(default)]
    pub close: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeAgentState {
    Running,
    Completed,
    Failed,
    Cancelled,
    Disconnected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeAgent {
    pub owner_session_id: String,
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub name: String,
    pub task: String,
    pub capabilities: NativeAgentCapabilities,
    pub state: NativeAgentState,
}

impl NativeAgent {
    /// Presentation identity only; never a separately provisioned session.
    pub fn view_id(&self) -> String {
        view_id(&self.owner_session_id, &self.session_id)
    }

    pub fn parent_view_id(&self) -> String {
        self.parent_session_id.as_ref().map_or_else(
            || self.owner_session_id.clone(),
            |parent| view_id(&self.owner_session_id, parent),
        )
    }
}

pub fn view_id(owner: &str, child: &str) -> String {
    // Length-prefixing keeps opaque provider IDs collision-free.
    format!("native:{}:{owner}{child}", owner.len())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NativeAgentEvent {
    Spawned {
        session_id: String,
        parent_session_id: Option<String>,
        name: String,
        task: String,
        capabilities: NativeAgentCapabilities,
    },
    State {
        session_id: String,
        state: NativeAgentState,
    },
    Update {
        session_id: String,
        update: Box<SessionUpdate>,
    },
    ReplayBegin,
    ReplayCommit,
    Disconnected,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeAgentView {
    pub generation_ordinal: u64,
    pub agent: NativeAgent,
    pub projection: crate::state::MaterializedSession,
}

/// Transcript-free identity published by the controller runtime feed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeAgentSummary {
    pub generation_ordinal: u64,
    pub agent: NativeAgent,
    pub projection_ordinal: u64,
    pub projection_digest: String,
}

impl NativeAgentSummary {
    pub fn of(view: &NativeAgentView) -> Self {
        Self {
            generation_ordinal: view.generation_ordinal,
            agent: view.agent.clone(),
            projection_ordinal: view.projection.applied_event_ordinal,
            projection_digest: view.projection.applied_event_digest.clone(),
        }
    }

    pub fn is_satisfied_by(&self, view: &NativeAgentView) -> bool {
        self.generation_ordinal == view.generation_ordinal
            && (view.projection.applied_event_ordinal > self.projection_ordinal
                || (view.projection.applied_event_ordinal == self.projection_ordinal
                    && view.projection.applied_event_digest == self.projection_digest
                    && view.agent == self.agent))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeAgentHistoryPage {
    pub generation_ordinal: u64,
    pub items: Vec<std::sync::Arc<crate::state::TranscriptItem>>,
    pub has_more: bool,
}
