//! Data exchanged with the controller store, independent of its SQLite implementation.
use crate::elicitation::ElicitationRequest;
use crate::state::*;
use crate::usage::ProviderCost;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
/// A deterministic projection integrity violation. Retrying cannot fix it, so
/// callers must report it separately from transport failures.
#[derive(Debug)]
pub struct ProjectionIntegrityError(pub String);

impl std::fmt::Display for ProjectionIntegrityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ProjectionIntegrityError {}

/// A store whose schema is not the one this build supports.
///
/// Carried as a typed cause rather than a message so the daemon can tell a
/// store that moved underneath it from a transport failure. It survives every
/// `anyhow` hop to the caller, which finds it with `error.chain()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreSchemaMismatch {
    pub found: i64,
    pub supported: i64,
}

impl std::fmt::Display for StoreSchemaMismatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { found, supported } = self;
        // Direction decides the advice. A store ahead of this build cannot be
        // migrated by starting a daemon of this build -- that is what the old
        // single message told the user to do, for an hour.
        if found > supported {
            write!(
                formatter,
                "Mjolnir database schema {found} is newer than this Mjolnir build supports ({supported}); upgrade Mjolnir"
            )
        } else {
            write!(
                formatter,
                "Mjolnir database schema {found} is not the supported schema {supported}; start the Mjolnir daemon to migrate it"
            )
        }
    }
}

impl std::error::Error for StoreSchemaMismatch {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryScope {
    Project,
    Session,
    All,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptHistoryEntry {
    pub id: i64,
    pub session_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionApplyOutcome {
    Applied,
    AlreadyApplied,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptMutation {
    Upsert(TranscriptItem),
    Remove { stable_id: String },
}

/// Changes derived from one relay event. `None` leaves a scalar untouched;
/// the nested option on `session_title` permits explicitly clearing it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MaterializedSessionMutation {
    /// Relay receipt time for this event. Persistence and the actor cache both
    /// take a monotonic maximum so removing detail rows cannot move activity
    /// backwards.
    pub last_activity_at_ms: Option<i64>,
    pub execution: Option<MaterializedExecutionState>,
    pub session_title: Option<Option<String>>,
    pub configuration: Option<BTreeMap<String, serde_json::Value>>,
    pub transcript: Vec<TranscriptMutation>,
    pub queued_prompts: Option<Vec<MaterializedQueuedPrompt>>,
    pub pending_elicitations: Option<Vec<crate::elicitation::ElicitationRequest>>,
    /// The nested option distinguishes "unchanged" from "cleared", which is
    /// how a completed turn removes the running turn.
    pub active_turn: Option<Option<MaterializedTurn>>,
    /// A finished turn is only ever replaced, never cleared.
    pub last_turn_outcome: Option<MaterializedTurnOutcome>,
    pub config_results: Vec<(String, Option<String>)>,
    pub provider_cost: Option<crate::usage::ProviderCost>,
    pub api_events: Vec<ApiEventData>,
}

/// Atomically advance both the per-client and legacy session read frontiers.
/// Neither value changes when validation or persistence fails.
/// One viewer's stored state for one session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientSessionState {
    pub draft: String,
    pub through_event_ordinal: u64,
}

/// A terminal's current composer and the shared value it originally inherited.
/// The inherited value is retired on detach, never replaced by client-local text.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DetachedSessionDraft {
    pub text: String,
    pub inherited_input: Option<String>,
}

/// What one turn produced, without loading the transcript around it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSummary {
    /// One-based count of turn starts up to and including this one, which is
    /// what a caller means by "turn 3 of this session".
    pub turn_number: u64,
    pub turn_started_at_ms: i64,
    /// Newest change at or after the turn start, so the caller can measure how
    /// long the turn took.
    pub last_changed_at_ms: i64,
    /// The last nonempty agent message the turn produced, flattened to text.
    pub final_message: Option<String>,
}

/// One page of a session's transcript, ordered by the sequence a reader pages
/// by rather than by creation order.
#[derive(Debug, Clone)]
pub struct TranscriptPage {
    pub items: Vec<Arc<TranscriptItem>>,
    /// The newest sequence in the whole transcript, so a caller can tell
    /// whether this page reached the end without asking for another one.
    pub latest_seq: u64,
    pub next_after_seq: u64,
    pub execution: MaterializedExecutionState,
}

/// What one retention pass reclaimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TranscriptRetention {
    pub items: usize,
    pub bytes: usize,
    /// Rows this pass left for the next one, because of
    /// `RETENTION_BATCH_ITEMS`.
    pub remaining: bool,
}

/// A second-opinion review that was still open when the UI last stopped.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StoredReview {
    pub workflow: crate::second_opinion::ReviewWorkflow,
    /// Reviewer lifetime this review belongs to. It is bumped when native
    /// continuity is lost, so a resumed session starts a new conversation
    /// rather than pretending to reload one that is gone.
    pub generation: u64,
    /// The primary's transcript frontier when the context request went out.
    pub context_baseline: u64,
    /// Whether the reviewer's native session is known to be gone.
    pub native_lost: bool,
    /// What the controller has read of the reviewer's conversation. The
    /// reviewer's own journal is the source, but it dies with the target, so
    /// this copy is what keeps a finished review readable afterwards.
    pub reviewer_transcript: Vec<std::sync::Arc<crate::state::TranscriptItem>>,
}

/// How far this session has been reviewed.
///
/// `baselines` are Git tree ids by repository root: the working tree as of the
/// last completed review. They advance only when a review resolves, which is
/// what makes a cancelled review lossless -- the next one covers both turns.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TurnReviewState {
    pub baselines: std::collections::BTreeMap<std::path::PathBuf, String>,
    pub reviewed_through_ordinal: u64,
    /// The last forwarded verdict, which turns the next review into a
    /// verification pass. Cleared once that pass consumes it.
    pub prior_review: Option<crate::review::lanes::PriorReviewContext>,
    /// A review that was running when the daemon stopped. On recovery it is
    /// cleared without advancing the baseline.
    pub active: Option<String>,
    /// A corrective prompt that was submitted while its relay acceptance was
    /// still ambiguous. It survives a daemon restart so the exact command can
    /// be retried and reconciled without losing the findings.
    #[serde(default)]
    pub pending_forward: Option<crate::review::driver::PendingForward>,
}

/// What a bounded prompt search found, and whether it stopped early.
///
/// The flag is not decoration. Without it a caller cannot tell twenty matches
/// from the first twenty of many, and will present a partial answer as a whole
/// one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedPromptHistory {
    pub entries: Vec<PromptHistoryEntry>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageCoverage {
    pub recorded_turns: u64,
    pub full_turn_reports: u64,
    pub last_request_reports: u64,
    pub unspecified_reports: u64,
    pub missing_reports: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageCounterTotal {
    pub tokens: u64,
    /// Number of full-turn reports supplying this particular counter.
    pub reported_turns: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsagePage {
    pub session_id: String,
    pub turns: Vec<MaterializedTurnOutcome>,
    pub next_after_seq: u64,
    pub latest_seq: u64,
    /// Totals include only reports whose scope is known to be a whole turn.
    pub totals: BTreeMap<String, UsageCounterTotal>,
    pub coverage: UsageCoverage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session_cost: Option<ProviderCost>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiEvent {
    pub seq: u64,
    pub session_id: String,
    pub recorded_at_ms: i64,
    #[serde(flatten)]
    pub event: ApiEventData,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ApiEventData {
    TurnStarted {
        turn: MaterializedTurn,
    },
    TurnEnded {
        turn: MaterializedTurnOutcome,
    },
    Error {
        message: String,
        command_id: Option<String>,
    },
    InputRequired {
        request: ElicitationRequest,
        turn_id: Option<u64>,
    },
    InputResolved {
        elicitation_id: String,
        turn_id: Option<u64>,
        action: String,
    },
    ActivityChanged {
        activity: ApiActivityState,
    },
}

impl ApiEventData {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::TurnStarted { .. } => "turn_started",
            Self::TurnEnded { .. } => "turn_ended",
            Self::Error { .. } => "error",
            Self::InputRequired { .. } => "input_required",
            Self::InputResolved { .. } => "input_resolved",
            Self::ActivityChanged { .. } => "activity_changed",
        }
    }
}

/// These are the same structured facts rendered by the web and terminal UIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiActivityState {
    pub state: String,
    pub details: Option<ApiActivityDetails>,
    pub is_idle: bool,
    pub waiting_for_input: bool,
    pub capacity_retry: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiActivityDetails {
    pub kind: ApiActivityKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_started_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_started_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_started_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_since_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiActivityKind {
    Turn,
    Step,
    Background,
    Idle,
    Lifecycle,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiEventFilter {
    pub session_id: Option<String>,
    pub workspace_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiEventPage {
    pub events: Vec<ApiEvent>,
    pub next_after_seq: u64,
    pub latest_seq: u64,
}
