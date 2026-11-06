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

/// A store this build cannot safely read and write.
///
/// Carried as a typed cause rather than a message so the daemon can tell a
/// store that moved underneath it from a transport failure. It survives every
/// `anyhow` hop to the caller, which finds it with `error.chain()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreSchemaMismatch {
    pub found: i64,
    pub supported: i64,
    pub reason: StoreSchemaMismatchReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreSchemaMismatchReason {
    NeedsMigration,
    Incompatible { minimum_compatible: i64 },
    InvalidCompatibilityMetadata,
    Rollback { previous: i64 },
}

impl std::fmt::Display for StoreSchemaMismatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            found,
            supported,
            reason,
        } = self;
        match reason {
            StoreSchemaMismatchReason::Incompatible { minimum_compatible } => write!(
                formatter,
                "Mjolnir database schema {found} requires at least build schema {minimum_compatible} for reads and writes; this build supports {supported}; upgrade Mjolnir, run this build with an isolated data directory (--instance NAME or MJ_DATA_DIR), or restore a backup made by the older build"
            ),
            StoreSchemaMismatchReason::NeedsMigration => write!(
                formatter,
                "Mjolnir database schema {found} is not the supported schema {supported}; start the Mjolnir daemon to migrate it"
            ),
            StoreSchemaMismatchReason::InvalidCompatibilityMetadata => write!(
                formatter,
                "Mjolnir database schema {found} has missing or invalid compatibility metadata; refusing access from build schema {supported}"
            ),
            StoreSchemaMismatchReason::Rollback { previous } => write!(
                formatter,
                "Mjolnir database schema rolled back from {previous} to {found} underneath this writer; refusing writes from build schema {supported}"
            ),
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
    pub native_agent: Option<crate::relay::RelayEvent>,
    /// Relay receipt time for this event. Persistence and the actor cache both
    /// take a monotonic maximum so removing detail rows cannot move activity
    /// backwards.
    pub last_activity_at_ms: Option<i64>,
    pub execution: Option<MaterializedExecutionState>,
    pub session_title: Option<Option<String>>,
    pub configuration: Option<SessionConfiguration>,
    pub transcript: Vec<TranscriptMutation>,
    pub queued_prompts: Option<Vec<MaterializedQueuedPrompt>>,
    pub pending_elicitations: Option<Vec<crate::elicitation::ElicitationRequest>>,
    /// The nested option distinguishes "unchanged" from "cleared", which is
    /// how a completed turn removes the running turn.
    pub active_turn: Option<Option<MaterializedTurn>>,
    /// Explicit context resets retire the last turn without deleting its history.
    pub clear_turn_outcome: bool,
    /// Discard pending mailbox delivery when a new conversation is adopted.
    pub clear_mailbox: bool,
    pub last_turn_outcome: Option<MaterializedTurnOutcome>,
    pub config_results: Vec<(String, Option<String>)>,
    pub provider_cost: Option<crate::usage::ProviderCost>,
    pub api_events: Vec<ApiEventData>,
}

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
    /// How many tool calls the turn made.
    pub tool_calls: u64,
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

/// Exclusive chronological cursor; update sequences are not creation order.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TranscriptCursor {
    pub position: u64,
    pub stable_id: String,
}

impl TranscriptCursor {
    pub fn of(item: &TranscriptItem) -> Self {
        Self {
            position: item.position,
            stable_id: item.stable_id.clone(),
        }
    }
}

/// A bounded historical read, independent of the live projection window.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TranscriptHistoryPage {
    pub items: Vec<Arc<TranscriptItem>>,
    pub before: Option<TranscriptCursor>,
    pub frontier: u64,
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

/// A declined finding and the repositories whose review range it covers. An
/// entry stored before repositories were recorded has none and covers every
/// repository until any review base moves.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DeclinedFinding {
    #[serde(flatten)]
    pub entry: crate::subagent::ReviewOverride,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repositories: Vec<std::path::PathBuf>,
}

impl DeclinedFinding {
    /// Whether a review of `root` must be told not to repeat this finding.
    pub fn covers(&self, root: &std::path::Path) -> bool {
        self.repositories.is_empty() || self.repositories.iter().any(|covered| covered == root)
    }
}

impl std::ops::Deref for DeclinedFinding {
    type Target = crate::subagent::ReviewOverride;
    fn deref(&self) -> &Self::Target {
        &self.entry
    }
}

impl PartialEq<crate::subagent::ReviewOverride> for DeclinedFinding {
    fn eq(&self, other: &crate::subagent::ReviewOverride) -> bool {
        self.entry == *other
    }
}

/// Durable review work, findings and declined findings for this session.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TurnReviewState {
    /// Findings the author declined to fix with `override_review`, newest last.
    /// Every before-push review of this session tells its reviewers not to
    /// report them again in those repositories. A baseline move removes that
    /// repository from their coverage. Bounded to `DECLINED_FINDINGS_BYTES`.
    #[serde(default)]
    pub declined_findings: Vec<DeclinedFinding>,
    /// A review that was running when the daemon stopped. On recovery it is
    /// cleared without advancing the baseline.
    pub active: Option<String>,
    /// The exact trees covered by the last before-push findings verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_findings: Option<PushFindingsRecord>,
    /// A reviewer group survives daemon handoff with its exact commands and children.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_group: Option<ActiveReviewGroup>,
}

/// How many bytes of declined findings and reasons a session keeps. The
/// oldest entries go first.
pub const DECLINED_FINDINGS_BYTES: usize = 24 * 1024;

impl TurnReviewState {
    /// Stop covering the repositories whose review base moved, dropping entries
    /// left with none. An entry without recorded repositories is dropped.
    pub fn advance_review_bases(&mut self, moved: &[std::path::PathBuf]) {
        if moved.is_empty() {
            return;
        }
        self.declined_findings.retain_mut(|entry| {
            entry.repositories.retain(|root| !moved.contains(root));
            !entry.repositories.is_empty()
        });
    }

    /// Adds findings the author declined, then drops the oldest entries until
    /// the list fits `DECLINED_FINDINGS_BYTES`.
    pub fn decline_findings(
        &mut self,
        entries: impl IntoIterator<Item = crate::subagent::ReviewOverride>,
        repositories: Vec<std::path::PathBuf>,
    ) {
        for entry in entries {
            let entry = DeclinedFinding {
                entry,
                repositories: repositories.clone(),
            };
            if !self.declined_findings.contains(&entry) {
                self.declined_findings.push(entry);
            }
        }
        let size = |entry: &DeclinedFinding| {
            entry.entry.finding.len()
                + entry.entry.reason.len()
                + entry
                    .repositories
                    .iter()
                    .map(|root| root.as_os_str().len())
                    .sum::<usize>()
        };
        let mut total: usize = self.declined_findings.iter().map(size).sum();
        let mut drop = 0;
        while total > DECLINED_FINDINGS_BYTES && drop < self.declined_findings.len() {
            total -= size(&self.declined_findings[drop]);
            drop += 1;
        }
        self.declined_findings.drain(..drop);
    }
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(from = "ActiveReviewGroupInput")]
pub struct ActiveReviewGroup {
    pub driver: crate::review::driver::ReviewDriverSnapshot,
    pub reviewers: Vec<crate::review::settings::ResolvedReviewSettings>,
    pub registrations: Vec<ReviewerRegistration>,
}

#[derive(serde::Deserialize)]
struct ActiveReviewGroupInput {
    driver: crate::review::driver::ReviewDriverSnapshot,
    #[serde(default)]
    reviewers: Vec<crate::review::settings::ResolvedReviewSettings>,
    #[serde(default)]
    reviewer: Option<crate::review::settings::ResolvedReviewSettings>,
    registrations: Vec<ReviewerRegistration>,
}
impl From<ActiveReviewGroupInput> for ActiveReviewGroup {
    fn from(input: ActiveReviewGroupInput) -> Self {
        let reviewers = if input.reviewers.is_empty() {
            input
                .reviewer
                .map(|reviewer| vec![reviewer; input.driver.roles.len()])
                .unwrap_or_default()
        } else {
            input.reviewers
        };
        Self {
            driver: input.driver,
            reviewers,
            registrations: input.registrations,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReviewerRegistration {
    pub role: String,
    pub request_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PushFindingsRecord {
    pub review_id: String,
    pub trees: std::collections::BTreeMap<std::path::PathBuf, (String, String)>,
    pub synthesis: String,
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
    /// Started turns for which no completion report has been recorded.
    #[serde(default)]
    pub unfinished_turns: u64,
    #[serde(default)]
    pub provider_cost_turns: u64,
    #[serde(default)]
    pub estimated_cost_turns: u64,
    #[serde(default)]
    pub missing_cost_turns: u64,
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
    /// Sum of priced whole turns, in 10^-10 USD. Partial when cost coverage is missing.
    #[serde(default)]
    pub turn_cost_usd_ticks: u64,
    #[serde(default)]
    pub turn_selections: BTreeMap<String, UsageSelection>,
    #[serde(default)]
    pub by_model: Vec<UsageModelTotal>,
}

/// Configuration observed when a turn started. None means unknown, not zero.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct UsageSelection {
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageModelTotal {
    #[serde(flatten)]
    pub selection: UsageSelection,
    pub totals: BTreeMap<String, UsageCounterTotal>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageSummary {
    pub totals: BTreeMap<String, UsageCounterTotal>,
    pub coverage: UsageCoverage,
    pub by_model: Vec<UsageModelTotal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session_cost: Option<ProviderCost>,
    /// Sum of priced whole turns, in 10^-10 USD. Partial when cost coverage is missing.
    #[serde(default)]
    pub turn_cost_usd_ticks: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageTreeSession {
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub task_name: Option<String>,
    pub operational_session_present: bool,
    #[serde(flatten)]
    pub summary: UsageSummary,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageTree {
    pub parent_session_id: String,
    pub sessions: Vec<UsageTreeSession>,
    #[serde(flatten)]
    pub summary: UsageSummary,
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
    RuntimeResolved {
        receipt: crate::harness_runtime::RuntimeReceipt,
    },
    TurnStarted {
        turn: MaterializedTurn,
    },
    TurnEnded {
        turn: crate::event_outcome::ApiTurnOutcome,
    },
    CommandEnded {
        #[serde(flatten)]
        result: crate::event_outcome::CommandResult,
    },
    SessionFault {
        reason: crate::event_outcome::OutcomeReason,
        message: String,
        command_id: Option<String>,
    },
    LegacyNotice {
        original_type: String,
        message: String,
        command_id: Option<String>,
    },
    InputRequired {
        /// Absent when a completed turn asks for unstructured user input.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request: Option<ElicitationRequest>,
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
            Self::RuntimeResolved { .. } => "runtime_resolved",
            Self::TurnStarted { .. } => "turn_started",
            Self::TurnEnded { .. } => "turn_ended",
            Self::CommandEnded { .. } => "command_ended",
            Self::SessionFault { .. } => "session_fault",
            Self::LegacyNotice { .. } => "legacy_notice",
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
    /// When anything last arrived from the harness, while a turn or a tool is
    /// in flight. A surface subtracts it from the current time to show how
    /// long a running session has been quiet. Mjolnir never ends a turn for
    /// this on its own; see `mj_core::activity::silence_note`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_at_ms: Option<i64>,
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

// Scope is a persisted compatibility field, never an orchestration decision.
impl serde::Serialize for ActiveReviewGroup {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut value = serializer.serialize_struct("ActiveReviewGroup", 4)?;
        value.serialize_field("driver", &self.driver)?;
        value.serialize_field("reviewers", &self.reviewers)?;
        value.serialize_field("registrations", &self.registrations)?;
        value.serialize_field("scope", "pushable_commits")?;
        value.end()
    }
}

#[cfg(test)]
mod declined_findings_tests {
    use super::*;
    use crate::subagent::ReviewOverride;

    fn entry(finding: &str, reason: &str) -> ReviewOverride {
        ReviewOverride {
            finding: finding.into(),
            reason: reason.into(),
        }
    }

    // Hard-won: 9ebd246b2: clearing one repository erased other ranges.
    #[test]
    fn a_base_move_stops_covering_only_its_own_repository() {
        let mut state: TurnReviewState = serde_json::from_value(serde_json::json!({
            "active": null, "declined_findings": [{"finding": "legacy", "reason": "all repositories"}]
        })).unwrap();
        assert!(state.declined_findings[0].covers(std::path::Path::new("/b")));
        state.decline_findings([entry("scoped", "both")], vec!["/a".into(), "/b".into()]);
        state.decline_findings([entry("scoped", "both")], vec!["/a".into(), "/b".into()]);
        assert_eq!(
            state.declined_findings.len(),
            2,
            "a retried override is not duplicated"
        );
        state.advance_review_bases(&["/a".into()]);
        assert_eq!(
            state.declined_findings.len(),
            1,
            "an unscoped entry ends at any move"
        );
        assert_eq!(
            state.declined_findings[0].repositories,
            vec![std::path::PathBuf::from("/b")]
        );
        let restored: TurnReviewState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(restored, state);
        state.advance_review_bases(&["/b".into()]);
        assert!(state.declined_findings.is_empty());
    }

    #[test]
    fn the_declined_list_keeps_the_newest_entries_within_its_byte_budget() {
        let mut state = TurnReviewState::default();
        state.decline_findings([entry("first", "kept order")], vec![]);
        state.decline_findings([entry("second", "kept order")], vec![]);
        assert_eq!(
            state.declined_findings.len(),
            2,
            "within budget nothing goes"
        );

        let half = DECLINED_FINDINGS_BYTES / 2 - 2;
        state.decline_findings(
            [entry(&"a".repeat(half), "x"), entry(&"b".repeat(half), "y")],
            vec![],
        );
        let kept: Vec<_> = state
            .declined_findings
            .iter()
            .map(|entry| entry.finding.chars().next().unwrap())
            .collect();
        assert_eq!(kept, vec!['a', 'b'], "the oldest entries are dropped first");
        let size: usize = state
            .declined_findings
            .iter()
            .map(|entry| entry.finding.len() + entry.reason.len())
            .sum();
        assert!(size <= DECLINED_FINDINGS_BYTES);
    }
}
