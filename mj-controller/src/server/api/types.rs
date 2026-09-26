use super::*;

/// Comparison and representation requested for a session's working-tree diff.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiffOptions {
    pub base: Option<String>,
    #[serde(default)]
    pub json: bool,
}

/// Observed provider-owned background work; absent when no live snapshot is available.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiBackgroundWork {
    pub known: Option<bool>,
    pub tasks: Vec<mj_core::relay::BackgroundCommand>,
}

impl From<&mj_core::relay::RelayOperationalState> for ApiBackgroundWork {
    fn from(state: &mj_core::relay::RelayOperationalState) -> Self {
        Self {
            known: state.background_work_known,
            tasks: state.background_commands.clone(),
        }
    }
}

/// One session as the API presents it. This is a narrower, more stable shape
/// than the viewer's own session projection, which changes whenever the browser
/// needs something new.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiSession {
    /// Immutable starting selection; session readiness verifies preparation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout: Option<mj_core::remote_git::ExactCheckout>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_work: Option<ApiBackgroundWork>,
    pub id: String,
    pub workspace_id: String,
    pub title: String,
    pub harness_kind: String,
    pub profile_id: String,
    pub target_id: String,
    pub bundle_id: String,
    pub state: String,
    pub lifecycle: ViewerLifecycleCategory,
    pub chat_phase: crate::server::ViewerChatPhase,
    pub is_idle: bool,
    /// What this session is doing, in more detail than `chat_phase`'s four
    /// values allow: in particular it can say that the daemon cannot see the
    /// worker and report what it last knew, rather than claiming idleness it
    /// cannot prove.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_state: Option<mj_core::activity::ActivityState>,
    pub has_error: bool,
    /// Why a launch failed, for a session in the error state. Absent
    /// otherwise: raw runtime error text is deliberately not published for a
    /// running session. Why a *turn* failed travels in `last_turn_diagnostic`,
    /// which a single-session query fills.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// How the last finished prompt ended. Absent unless the caller asked for
    /// one session by id or waited on it, because the dashboard projection the
    /// list is built from does not carry turn identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn_outcome: Option<MaterializedTurnOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn_diagnostic: Option<mj_core::diagnostic::TurnDiagnostic>,
    #[serde(default)]
    pub config_options: Vec<crate::server::ViewerConfigOption>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_elicitations: Vec<mj_core::elicitation::ElicitationRequest>,
}

impl From<&ViewerSession> for ApiSession {
    fn from(session: &ViewerSession) -> Self {
        Self {
            background_work: None,
            checkout: session.checkout.clone(),
            id: session.id.clone(),
            workspace_id: session.workspace_id.clone(),
            title: session.title.clone(),
            harness_kind: session.harness_kind.clone(),
            profile_id: session.profile_id.clone(),
            target_id: session.target_id.clone(),
            bundle_id: session.bundle_id.clone(),
            state: session.state.clone(),
            lifecycle: session.lifecycle,
            chat_phase: session.chat_phase,
            is_idle: session.is_idle,
            activity_state: session.activity_state.clone(),
            has_error: session.has_error,
            error: session.launch_error.clone(),
            created_at: session.created_at.clone(),
            updated_at: session.updated_at.clone(),
            last_turn_outcome: None,
            last_turn_diagnostic: None,
            config_options: session.config_options.clone(),
            pending_elicitations: session.pending_elicitations.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionListResponse {
    pub sessions: Vec<ApiSession>,
}

/// The workspaces the daemon holds, newest opening first, exactly as the
/// terminal's workspace tabs and the viewer's list see them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceListResponse {
    pub workspaces: Vec<mj_core::workspace::WorkspaceRecord>,
}

/// Name the workspace to work in. The name is the identity: it is trimmed, at
/// most 64 characters, and unique case-insensitively, so naming one that
/// already exists returns it rather than making a second.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateWorkspaceRequest {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateWorkspaceResponse {
    pub workspace: mj_core::workspace::WorkspaceRecord,
}

/// Create a session and, optionally, send its first prompt. Served in M2.
///
/// `profile_id` and `target_id` may be omitted, and each resolves
/// independently: a caller may name a profile and take the saved default
/// target. An omitted identifier comes from the pair the user last saved with
/// the `mj go` workflow, which the first setup also becomes, so a caller that
/// has never read `config.toml` can create a session by naming neither. The
/// controller still receives two explicit identifiers, because a session whose
/// profile was implicit would be a session nobody can explain later.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartSessionRequest {
    #[serde(default)]
    pub create_managed_worktree: Option<bool>,
    /// Diff baseline; also the starting revision for a raw managed worktree.
    #[serde(default)]
    pub launch_base: Option<String>,
    /// Branch to check out in a new isolated workspace.
    #[serde(default)]
    pub launch_branch: Option<String>,
    /// Exact starting selection for one bundle repository, verified before readiness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout: Option<mj_core::remote_git::ExactCheckout>,
    /// None means native sub-agents, the same as `Some(false)`.
    #[serde(default)]
    pub mjolnir_subagents: Option<bool>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// Omitted follows the saved default. See the type's own documentation.
    #[serde(default)]
    pub profile_id: Option<String>,
    /// Omitted follows the saved default. See the type's own documentation.
    #[serde(default)]
    pub target_id: Option<String>,
    #[serde(default)]
    pub bundle_id: Option<String>,
    #[serde(default)]
    pub project_directory: Option<PathBuf>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
}

/// Resume a stopped, lost, or failed session. Every field is optional: the
/// session's own record supplies what the caller does not name, which is what
/// makes `POST .../resume` with no body the scriptable "continue this session"
/// call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeSessionRequest {
    /// Profile to resume on. Defaults to the one the session last ran.
    #[serde(default)]
    pub profile_id: Option<String>,
    /// Target template to provision. Defaults to the session's own.
    #[serde(default)]
    pub target_id: Option<String>,
    /// Workspace the resumed session belongs to. Defaults to its own.
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// Whether prompts queued when the session stopped are started or
    /// discarded. Defaults to `start`, which is what the terminal's own resume
    /// wizard defaults to.
    #[serde(default)]
    pub queue: Option<mj_core::state::ResumeQueueDisposition>,
}

/// What a resume was accepted as: the settings it will actually use, resolved
/// from the request and the session's record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeSessionResponse {
    pub session_id: String,
    pub workspace_id: String,
    pub profile_id: String,
    pub target_id: String,
}

/// What a suspend was accepted as. A suspend stops the session's active
/// Mjolnir sub-agents without a checkpoint and suspends the session alone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuspendSessionResponse {
    #[serde(default)]
    pub session_id: String,
    /// Active sub-agents the suspend stops.
    #[serde(default)]
    pub stopped_subagents: usize,
    /// How many of those have not handed back their report.
    #[serde(default)]
    pub subagents_not_handed_back: usize,
    /// "N sub-agents have not handed back; suspending stops them", when any
    /// have not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartSessionResponse {
    pub session_id: String,
    /// The turn the follow-up prompt was accepted as, once it has been
    /// submitted. Creation answers before that, so it is usually absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentSourceRange {
    pub file: PathBuf,
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpawnSubagentRequest {
    pub task_name: String,
    pub instructions: String,
    #[serde(default)]
    pub profile_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub working_directory: Option<PathBuf>,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub files: Vec<SubagentSourceRange>,
}

/// One profile a parent may start a sub-agent on, with what it offers and how
/// much of its quota is left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentCandidate {
    pub profile_id: String,
    pub harness: mj_core::config::HarnessKind,
    pub choices: mj_core::worker_launch::ProfileConfig,
    /// The lower of the profile's quota windows (the 5-hour and weekly ones
    /// for Codex and Claude), 100 for a pay-per-use profile, and `None` when
    /// no usable report exists.
    pub remaining_percent: Option<u8>,
}

/// The profiles a parent may delegate to, split into those whose choices are
/// known and those whose discovery failed, with the reason.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubagentCandidates {
    pub offered: Vec<SubagentCandidate>,
    pub unavailable: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubagentView {
    pub parent_session_id: String,
    pub task_name: String,
    pub session: ApiSession,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubagentListResponse {
    pub subagents: Vec<SubagentView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptRequest {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptResponse {
    /// The relay acceptance ordinal for this prompt, which is what `wait`
    /// takes as `turn_id`.
    pub turn_id: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitRequest {
    /// Return when the harness presents a structured input request.
    #[serde(default)]
    pub return_on_input: bool,
    /// Wait for this specific prompt. Absent means "wait until the session is
    /// idle with nothing queued", which is what a caller that lost its turn id
    /// wants.
    #[serde(default)]
    pub turn_id: Option<u64>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// How a wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitOutcome {
    /// A structured elicitation needs an answer; only returned by opt-in waits.
    InputRequired,
    /// The turn completed normally.
    Finished,
    /// The turn failed, was rejected, or the session reported an error.
    Error,
    /// The turn was cancelled or interrupted.
    Cancelled,
    /// The model was at capacity and no retry is armed.
    QuotaLimit,
    /// The wait's deadline passed with the turn still running.
    Timeout,
    /// The session stopped or is stopping, so no turn can finish on it.
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaitCapacityRetry {
    pub attempt: u32,
    pub retry_at_ms: i64,
}

impl From<&CapacityRetry> for WaitCapacityRetry {
    fn from(retry: &CapacityRetry) -> Self {
        Self {
            attempt: retry.attempt,
            retry_at_ms: retry.retry_at_ms,
        }
    }
}

/// How the daemon's live view of a session's relay is doing.
///
/// This reports; it never decides an outcome. A caller that gets `timeout`
/// needs to tell "the turn is still working" from "the daemon cannot see the
/// worker at all", and those look identical without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayState {
    /// The daemon is attached to the worker and following its events.
    Connected,
    /// Not attached, with no error recorded yet: attaching, or between tries.
    Disconnected,
    /// The worker could not be reached.
    Unreachable,
    /// The session's target is gone.
    TargetMissing,
    /// The event stream did not line up with what the daemon had projected.
    ProjectionIntegrity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayHealth {
    pub state: RelayState,
    /// The view's own description of the problem, when it recorded one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl From<&mj_client::session::ManagedSessionView> for RelayHealth {
    fn from(view: &mj_client::session::ManagedSessionView) -> Self {
        use mj_client::session::ViewError;
        // A recorded error outranks `connected`: it is the specific thing
        // standing between the caller and a finished turn.
        match &view.error {
            Some(error) => Self {
                state: match error {
                    ViewError::Unreachable(_) => RelayState::Unreachable,
                    ViewError::TargetMissing(_) => RelayState::TargetMissing,
                    ViewError::ProjectionIntegrity(_) => RelayState::ProjectionIntegrity,
                },
                detail: Some(error.detail().to_owned()),
            },
            None if view.connected => Self {
                state: RelayState::Connected,
                detail: None,
            },
            None => Self {
                state: RelayState::Disconnected,
                detail: None,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaitResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<mj_core::diagnostic::TurnDiagnostic>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_elicitations: Vec<mj_core::elicitation::ElicitationRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<mj_core::usage::TokenUsage>,
    pub outcome: WaitOutcome,
    /// The harness's own stop reason, when the turn reached one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Why the wait ended this way, when there is something to say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The agent's last message of the turn, flattened to text. For a
    /// sub-agent child it is the report the child handed back, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_message: Option<String>,
    /// For a sub-agent child: `handback` when `final_message` is the report
    /// the child handed back, `last_message` when it is the turn's last
    /// message. Absent for every other session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    /// One-based position of this turn in the conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_number: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<i64>,
    /// Legacy alias for a worker-owned server retry, retained for older clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity_retry: Option<WaitCapacityRetry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_retry: Option<WaitCapacityRetry>,
    #[serde(default)]
    pub retry_assessment_pending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_recovery: Option<mj_core::continuation::QuotaRecovery>,
    /// The health of the daemon's live view of this session. Absent when no
    /// live actor holds the session, because there is then no view to report
    /// on and inventing one would be worse than saying nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<RelayHealth>,
    pub session: ApiSession,
}

/// How many transcript items a page carries when the caller names no limit,
/// and the most it may ask for. A caller that asks for more gets the ceiling
/// rather than an error: paging is the point, and refusing a large limit would
/// only make the caller retry with a smaller one.
pub const DEFAULT_TRANSCRIPT_LIMIT: usize = 200;
pub const MAX_TRANSCRIPT_LIMIT: usize = 1_000;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TranscriptQuery {
    #[serde(default)]
    pub role: Option<mj_core::transcript::TranscriptRole>,
    /// Resume from the highest sequence the caller has already seen.
    #[serde(default)]
    pub after_seq: Option<u64>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptItemView {
    pub stable_id: String,
    pub position: u64,
    /// What to pass as the next `after_seq`. It is the position for everything
    /// but an agent message, which carries the ordinal of its latest content.
    pub seq: u64,
    pub role: String,
    /// The item flattened to text, which is what a reading caller wants.
    pub text: String,
    pub created_at_ms: i64,
    pub last_changed_at_ms: i64,
    /// The stored body, for a caller that needs the structure behind the text.
    pub body: mj_core::transcript::TranscriptBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptResponse {
    #[serde(default)]
    pub next_after_seq: u64,
    pub session_id: String,
    /// The newest sequence in the whole transcript. A page whose last item
    /// reaches this is up to date.
    pub latest_seq: u64,
    pub execution: MaterializedExecutionState,
    pub items: Vec<TranscriptItemView>,
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

/// Where a session stands turn by turn, read from the durable projection when
/// no live actor holds the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnState {
    pub execution: MaterializedExecutionState,
    pub active_turn: Option<MaterializedTurn>,
    pub last_turn_outcome: Option<MaterializedTurnOutcome>,
}

pub use crate::database::TurnSummary;

/// Configuration and a first prompt to apply once a newly created session's
/// harness is ready. Served in M2.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartFollowup {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub prompt: Option<String>,
    pub fast_mode: bool,
}

/// How far a created session's follow-up has got. Served in M2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartStatus {
    /// The session is still provisioning, or its harness is not ready.
    Pending,
    /// The follow-up prompt was submitted and accepted as this turn.
    Submitted { turn_id: u64 },
    /// The session could not be started, or the follow-up could not be applied.
    Failed { message: String },
}

/// A page of transcript items, read from the durable projection.
pub use crate::database::TranscriptPage;

/// A branch the daemon pushed on the caller's behalf.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushedBranch {
    pub branch: String,
    pub remote: String,
}

/// Which file of the session's workspace to read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileQuery {
    /// Path relative to the session's workspace root.
    pub path: String,
}

/// What form the caller wants the session's work in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportKind {
    /// A unified diff, as `GET /diff` returns.
    Patch,
    /// A branch pushed to the repository's push remote.
    Branch,
    /// The git bundle of the session's committed work.
    Bundle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportRequest {
    pub kind: ExportKind,
    /// The branch to push. Required when `kind` is `branch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// A git bundle of the session's work.
#[derive(Debug, Clone)]
pub struct BundleExport {
    pub repository: String,
    pub bytes: Vec<u8>,
}

/// Why an export could not be produced. Served in M4.
#[derive(Debug)]
pub enum ExportError {
    /// The session is in a state where this export is not possible. The caller
    /// can act on it, so it answers 409.
    Refused(String),
    /// The export was attempted and failed.
    Failed(anyhow::Error),
}

impl From<ExportError> for ApiFailure {
    fn from(error: ExportError) -> Self {
        match error {
            ExportError::Refused(message) => Self::conflict(message),
            ExportError::Failed(error) => Self::from(error),
        }
    }
}

// ---------------------------------------------------------------------------
// SessionWiki
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WikiSearchQuery {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WikiBriefQuery {
    #[serde(default)]
    pub max_chars: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WikiBriefResponse {
    pub markdown: String,
}

/// The query for the matching passages of one indexed session.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WikiHitsQuery {
    pub q: String,
    #[serde(default)]
    pub context_messages: Option<usize>,
    #[serde(default)]
    pub per_message_chars: Option<usize>,
}

/// The fields of a start request a restore needs. The archived session decides
/// the rest: its title, and the project it ran in when the caller names none.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WikiRestoreBody {
    #[serde(default)]
    pub workspace_id: Option<String>,
    pub profile_id: String,
    pub target_id: String,
    #[serde(default)]
    pub project_directory: Option<PathBuf>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
}

// ---------------------------------------------------------------------------
// Launch options
// ---------------------------------------------------------------------------

/// What a caller may choose when it starts a session, and which pair to use
/// when it chooses nothing.
///
/// Served by `GET /api/v1/options` so a caller that has never read
/// `config.toml` can enumerate the profiles and targets this daemon knows,
/// learn whether each host answered its last check, and read the remembered
/// default. It is the public projection in `server/viewer_types.rs`, narrowed
/// to what a launch decision needs: no harness home, no SSH host or key, no
/// container environment, no AWS detail, and no controller-side path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchOptions {
    /// The projection revision these lists came from. Two reads carrying the
    /// same revision describe the same configuration, so a caller can build a
    /// form without mixing halves of two different worlds.
    pub revision: u64,
    pub profiles: Vec<LaunchProfile>,
    pub targets: Vec<LaunchTarget>,
    pub bundles: Vec<LaunchBundle>,
    /// One entry per host that has published a capacity reading. The label is
    /// how a person names the host; it is never a locator or an address.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<LaunchHost>,
    /// The pair the user last saved as their default, which the first setup
    /// also becomes. Absent when nothing has ever been saved, and never an
    /// error: a caller that cannot read a preference still has the lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<LaunchDefault>,
}

/// One account this daemon can run work under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchProfile {
    pub id: String,
    /// The harness kind, such as `codex` or `claude`. Which account it is, and
    /// where its credentials live, stay on the controller.
    pub harness: String,
}

/// One runtime template a session can run on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchTarget {
    pub id: String,
    /// The runtime kind, such as `local-bare` or `ssh-podman`.
    pub kind: String,
    /// Whether this target needs an existing Git directory on its machine
    /// instead of provisioning repositories into a workspace.
    pub requires_project_directory: bool,
    pub availability: LaunchAvailability,
    /// What to tell a person when the host did not answer. This repository
    /// composes the sentence: a probe's own message names hosts and commands
    /// and stays on the controller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
    /// How a person names the host, when a reading covers this target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

/// How much this daemon knows about a target's host.
///
/// A reading arrives from a background poll, so it can be absent, old, or
/// failed. Only `Unavailable` is a statement that the target cannot be used;
/// `Unknown` means nobody has checked yet, which is the ordinary state
/// immediately after startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LaunchAvailability {
    /// A reading covers this target and its last check succeeded.
    Ready,
    /// A reading covers this target but is marked stale.
    Stale,
    /// A reading covers this target and its last check failed.
    Unavailable,
    /// No reading covers this target yet.
    Unknown,
}

/// One repository set a managed target can provision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchBundle {
    pub id: String,
    pub primary_repository: String,
    pub repositories: Vec<LaunchRepository>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchRepository {
    pub id: String,
    /// The GitHub source, when this repository has one. A repository sourced
    /// from a local directory publishes nothing here, because that source is a
    /// path on the controller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<String>,
    pub destination: String,
}

/// One host or fleet, as much as a caller needs to explain an unavailable
/// target. The probe's own error text is deliberately absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchHost {
    pub id: String,
    pub label: String,
    /// The targets this reading covers.
    pub targets: Vec<String>,
    pub stale: bool,
    pub refreshing: bool,
    pub has_error: bool,
}

/// The pair a caller may leave unnamed when it starts a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchDefault {
    pub profile_id: String,
    pub target_id: String,
}
