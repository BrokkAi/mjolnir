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

/// Availability for another turn is independent of the last turn's outcome.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeAgentAvailability {
    Available,
    #[default]
    Unknown,
    Unavailable,
}

impl NativeAgentAvailability {
    pub fn label(self) -> &'static str {
        match self {
            Self::Available => "reusable",
            Self::Unknown => "availability unknown",
            Self::Unavailable => "unavailable",
        }
    }
}

/// A negotiated, read-only inventory entry. IDs are opaque provider identities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeAgentAvailabilityReport {
    pub session_id: String,
    #[serde(default)]
    pub stable_id: Option<String>,
    pub availability: NativeAgentAvailability,
    #[serde(default)]
    pub state: Option<NativeAgentState>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeAgent {
    #[serde(default)]
    pub availability: NativeAgentAvailability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub availability_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable_id: Option<String>,
    pub owner_session_id: String,
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub name: String,
    pub task: String,
    pub capabilities: NativeAgentCapabilities,
    pub state: NativeAgentState,
}

impl NativeAgent {
    pub fn invalidate_availability(&mut self) {
        self.availability = NativeAgentAvailability::Unknown;
        self.availability_reason =
            Some("The current harness has not confirmed that this agent can resume".into());
        self.capabilities = NativeAgentCapabilities::default();
    }

    /// Replayed work is historical until the current harness confirms activity.
    pub fn finish_replay(&mut self) {
        self.invalidate_availability();
        if self.state == NativeAgentState::Running {
            self.state = NativeAgentState::Disconnected;
        }
    }

    /// What the agent is doing or how its work ended: "working",
    /// "completed", "failed", "interrupted" or "disconnected".
    pub fn activity_label(&self) -> &'static str {
        match self.state {
            NativeAgentState::Running => "working",
            NativeAgentState::Completed => "completed",
            NativeAgentState::Failed => "failed",
            NativeAgentState::Cancelled => "interrupted",
            NativeAgentState::Disconnected => "disconnected",
        }
    }

    pub fn lifecycle_label(&self) -> String {
        format!("{} · {}", self.activity_label(), self.availability.label())
    }

    pub fn apply_availability(
        &mut self,
        reports: &[NativeAgentAvailabilityReport],
        complete: bool,
    ) {
        if let Some(report) = reports.iter().find(|r| r.session_id == self.session_id) {
            self.availability = report.availability;
            if let Some(state) = report.state {
                self.state = state;
            }
            self.availability_reason = report.reason.clone();
            self.stable_id = report.stable_id.clone();
        } else if complete {
            self.availability = NativeAgentAvailability::Unavailable;
            self.availability_reason = Some("Absent from the current harness inventory".into());
        }
    }

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

/// Marks a native child's presentation id. Mjolnir session ids are hex, so no
/// real session id starts with it.
const VIEW_ID_PREFIX: &str = "native:";

pub fn view_id(owner: &str, child: &str) -> String {
    // Length-prefixing keeps opaque provider IDs collision-free.
    format!("{VIEW_ID_PREFIX}{}:{owner}{child}", owner.len())
}

/// Whether `id` is a native child's presentation id from [`view_id`].
pub fn is_view_id(id: &str) -> bool {
    id.starts_with(VIEW_ID_PREFIX)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NativeAgentEvent {
    Availability {
        reports: Vec<NativeAgentAvailabilityReport>,
        complete: bool,
    },
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
