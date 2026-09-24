//! Authenticated local daemon protocol and client transport.
use crate::executable::describe_running_daemon_and_client_builds;
use crate::review::RuntimeReviewView;
use crate::session::{ManagedSessionView, ViewError};
use anyhow::{Context, Result, bail, ensure};
use mj_core::config::{Config, data_dir};
use mj_core::credentials::CredentialSyncSignal;
use mj_core::elicitation::ElicitationResponse;
use mj_core::relay::{RelayCommand, RelayOperationalState};
use mj_core::review::driver::Resolution;
use mj_core::state::*;
use mj_core::targets::{AdditionalMount, ProvisionStage};
use mj_core::workspace::WorkspaceRecord;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
pub fn metadata_path() -> PathBuf {
    data_dir().join("daemon.json")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonMetadata {
    pub protocol_version: u32,
    pub pid: u32,
    pub address: SocketAddr,
    pub token: String,
    pub started_at: String,
    pub build_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceListing {
    pub workspace: WorkspaceRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPreview {
    pub id: String,
    pub title: String,
    pub project: String,
    pub harness: String,
    pub state: String,
    pub active: bool,
    pub updated_at: String,
}

/// One session in the user's SessionWiki index, as a control surface shows it.
///
/// It is the daemon's own shape rather than SessionWiki's row: it carries the
/// search snippet that found the row and, for a Mjolnir session this daemon
/// still holds, the id that resumes it instead of restoring it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WikiRow {
    pub id: String,
    pub tool: String,
    pub project: String,
    pub title: String,
    pub started: Option<String>,
    pub msgs: i64,
    pub preview: Option<String>,
    /// The tool deleted its own copy and SessionWiki kept the transcript.
    pub archived: bool,
    /// The session id the tool that ran it knows it by, when its stored path
    /// carries one. It is what matches a row against an import scan.
    pub native_id: Option<String>,
    /// The matching text, when this row came from a search.
    pub snippet: Option<String>,
    /// The live Mjolnir session this row describes, when this daemon has it.
    pub hel_session_id: Option<String>,
    /// The Mjolnir target template the session ran under, when the index
    /// carries it. Only Mjolnir's own rows have one: the daemon writes it into
    /// the index as an `mj-target:` tag while the session is still known, so an
    /// archived row can still say where it ran.
    #[serde(default)]
    pub target: Option<String>,
    /// The Mjolnir harness profile the session last ran under, from the index's
    /// `mj-profile:` tag. Only Mjolnir's own rows have one.
    #[serde(default)]
    pub profile: Option<String>,
    /// The harness kind the session ran (`codex`, `claude`, `kimi`, `grok`,
    /// `muse`), from the index's `mj-harness:` tag. It stays meaningful after
    /// the profile id has been removed from the configuration.
    #[serde(default)]
    pub harness: Option<String>,
}

/// How far along the daemon's SessionWiki index is when a search answers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WikiIndexState {
    /// The index has completed one full build; its answers are complete.
    Ready,
    /// The first full build has not finished yet, so a search can miss
    /// sessions that exist. This is the state a fresh index starts in.
    #[default]
    Indexing,
    /// The index file on disk was written by a different SessionWiki schema
    /// version. Mjolnir will not open it, because opening it would drop and
    /// rebuild the user's whole cache.
    VersionMismatch,
}

/// What a search says about the index it answered from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WikiStatus {
    pub state: WikiIndexState,
    /// A sync is running now, so repeating the query may return more.
    pub topping_up: bool,
}

/// One page of search results with the state of the index behind them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WikiSearchPage {
    pub rows: Vec<WikiRow>,
    pub status: WikiStatus,
}

/// One message of an indexed transcript, reduced to what a search preview
/// shows: the text around the query's matches, with the matches located in it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WikiHitBlock {
    /// `user`, `assistant` or `tool`.
    pub role: String,
    /// The message text, redacted, and windowed to the caller's per-message
    /// budget when the message is longer than that.
    pub text: String,
    /// Byte ranges of the matches inside `text`, on character boundaries, in
    /// order. A context message has none.
    pub hits: Vec<(usize, usize)>,
    /// Messages between the previous block and this one that no group covered.
    /// Non-zero only on the first block of a group.
    pub omitted_before: usize,
    /// `text` is a window of the message rather than the whole of it.
    pub truncated: bool,
}

/// The matching passages of one indexed transcript.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WikiHitTranscript {
    pub blocks: Vec<WikiHitBlock>,
    /// Messages after the last block that no group covered.
    pub omitted_after: usize,
}

/// What one indexed session is, as far as continuing it is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WikiSessionStatus {
    /// A Mjolnir session this daemon still has a record of.
    Mine,
    /// A Mjolnir session whose record the archive job destroyed; only the
    /// indexed transcript is left.
    Archived,
    /// Another tool's session, which Mjolnir would have to import.
    Native,
}

/// One row of the SessionWiki index, with what Mjolnir knows about it.
///
/// This is what `mj resume --wiki` branches on and what `mj sessions
/// --session` reports when the id names no Mjolnir session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WikiSessionInfo {
    /// The SessionWiki id of the row.
    pub wiki_id: String,
    /// The tool that produced the session, as SessionWiki names it.
    pub tool: String,
    /// The transcript path the index stores for the row.
    pub path: PathBuf,
    pub status: WikiSessionStatus,
    /// The Mjolnir session id, for a row Mjolnir itself published.
    pub mjolnir_session_id: Option<String>,
    /// The profile the session last ran under, from the index's own tags.
    pub profile_id: Option<String>,
    /// The target template the session ran on, from the index's own tags.
    pub target_template_id: Option<String>,
    /// The harness that drove the session, from the tags for a Mjolnir row and
    /// from the tool name for a native one.
    pub harness: Option<mj_core::config::HarnessKind>,
    pub title: String,
    pub project: String,
}

/// Start a new session carrying a compacted hand-off from an archived one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WikiRestoreRequest {
    /// The SessionWiki session id to restore from.
    pub wiki_id: String,
    pub workspace_id: String,
    pub profile_id: String,
    pub target_template_id: String,
    /// Where the new session opens. None takes the project the archived
    /// session ran in, when that directory still exists.
    #[serde(default)]
    pub project_directory: Option<PathBuf>,
    #[serde(default)]
    pub additional_mounts: Vec<AdditionalMount>,
    #[serde(default)]
    pub resource_allocation: Option<SessionResourceAllocation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSnapshot {
    pub workspace: WorkspaceRecord,
    pub sessions: Vec<SessionPreview>,
    pub drafts: Vec<DraftPreview>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSessionView {
    pub session_id: String,
    pub projection_ordinal: u64,
    pub projection_digest: String,
    pub operational: Option<RelayOperationalState>,
    pub latest_credential_sync_signal: Option<CredentialSyncSignal>,
    pub connected: bool,
    pub error: Option<ViewError>,
}

impl RuntimeSessionView {
    pub fn from_managed(session_id: String, view: ManagedSessionView) -> Self {
        let (projection_ordinal, projection_digest, operational, signal) =
            view.snapshot
                .map_or((0, String::new(), None, None), |snapshot| {
                    (
                        snapshot.materialized.applied_event_ordinal,
                        snapshot.materialized.applied_event_digest,
                        Some(snapshot.operational),
                        snapshot.latest_credential_sync_signal,
                    )
                });
        Self {
            session_id,
            projection_ordinal,
            projection_digest,
            operational,
            latest_credential_sync_signal: signal,
            connected: view.connected,
            error: view.error,
        }
    }
}

/// Something the daemon did on its own that a surface should report once.
///
/// Background work has no lifecycle entry to hang a message on, so notices
/// travel with the snapshot and carry an id: a surface reports the ones newer
/// than the last it saw and nothing else, however often it polls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeNotice {
    pub id: u64,
    pub session_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshot {
    #[serde(default)]
    pub native_agents: Vec<mj_core::native_agent::NativeAgentSummary>,
    #[serde(default)]
    pub workspace_names: BTreeMap<String, String>,
    #[serde(default)]
    pub moves: Vec<mj_core::state::MoveOperation>,
    pub revision: u64,
    pub config: Config,
    pub records: Vec<SessionRecord>,
    pub sessions: Vec<RuntimeSessionView>,
    pub lifecycles: Vec<RuntimeLifecycleView>,
    /// Reviews the daemon is running, so every surface renders the same one.
    #[serde(default)]
    pub reviews: Vec<RuntimeReviewView>,
    /// Recent background events for this workspace's sessions, oldest first.
    #[serde(default)]
    pub notices: Vec<RuntimeNotice>,
    /// Parent/child relations for the sessions in `records`, so a surface can
    /// keep a daemon-created child out of the real workspace without a full
    /// state reload.
    #[serde(default)]
    pub subagents: Vec<mj_core::subagent::SubagentRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeLifecycleKind {
    Create,
    Suspend,
    Resume,
    Move,
    ForceStop,
    DestroyStopped,
    ForceDestroy,
    Cleanup,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeLifecycleView {
    pub operation_id: String,
    pub cancellable: bool,
    pub session_id: String,
    pub kind: RuntimeLifecycleKind,
    pub started_at_epoch_seconds: u64,
    pub active_stages: Vec<(ProvisionStage, u64)>,
    pub resume_destination: Option<(String, String)>,
    pub notice: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeSessionRequest {
    pub session_id: String,
    pub workspace_id: String,
    pub profile_id: String,
    pub target_template_id: String,
    pub additional_mounts: Option<Vec<AdditionalMount>>,
    pub resource_allocation: Option<SessionResourceAllocation>,
    pub discard_queue: bool,
    pub repository_preflight: Option<ResumeRepositorySourceReceipt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSessionRequest {
    #[serde(default)]
    pub create_managed_worktree: Option<bool>,
    /// Git revision the session starts at, as the caller typed it.
    #[serde(default)]
    pub launch_base: Option<String>,
    #[serde(default)]
    pub launch_branch: Option<String>,
    /// None follows the global `[subagents] enabled` setting at launch time.
    #[serde(default)]
    pub mjolnir_subagents: Option<bool>,
    #[serde(default)]
    pub initial_prompt: Option<String>,
    pub workspace_id: String,
    pub profile_id: String,
    pub bundle_id: String,
    pub project_directory: Option<PathBuf>,
    pub target_template_id: String,
    pub additional_mounts: Vec<AdditionalMount>,
    pub resource_allocation: Option<SessionResourceAllocation>,
    pub title: String,
    pub session_title_override: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisteredSession {
    pub session: SessionRecord,
    pub remembered_container_size: Option<(String, HostContainerSize)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftPreview {
    pub id: String,
    pub session_id: Option<String>,
    pub source: String,
    pub owner_pid: Option<u32>,
    pub saved_at: String,
}

/// `Ping`, `Status`, and `Stop` form the frozen management subset: their wire
/// encoding — together with `RequestEnvelope`, `ResponseEnvelope`,
/// `DaemonStatus`, and `WebViewerStatus` — must never change shape, because
/// clients and daemons of *any* protocol version rely on them to identify,
/// stop, and replace each other. `PrepareUpgrade` and its `Done` /
/// `UpgradePending` replies are also frozen from protocol 33 onward: they
/// drain accepted work before closing admission atomically. Every other
/// action may change freely behind a `PROTOCOL_VERSION` bump.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action", content = "arguments")]
pub enum DaemonAction {
    NativeAgentHistory {
        owner: String,
        child: String,
        before: Option<(u64, String)>,
    },
    Ping,
    /// Frozen upgrade handshake, available from daemon protocol 33 onward.
    /// Busy replies leave every operation and control channel running.
    PrepareUpgrade,
    /// Names the daemon-owned work that is currently holding an automatic
    /// handoff open, for the CLI's wait notice.
    ///
    /// Added after protocol 33, so it is not part of the frozen management
    /// subset. A daemon that predates it cannot deserialize the frame: it
    /// fails the request and closes the connection. Callers must therefore
    /// treat any failure, including a dropped connection or an error frame,
    /// as "unknown" and say nothing about what the upgrade is waiting for.
    UpgradeBlockers,
    Status,
    WebViewerAccess,
    RecoverWebViewer(crate::web::WebViewerRecovery),
    InspectWebListener,
    ListWorkspaces,
    CreateWorkspace {
        name: String,
    },
    RenameWorkspace {
        workspace_id: String,
        name: String,
    },
    TouchWorkspace {
        workspace_id: String,
    },
    CloseWorkspace {
        workspace_id: String,
    },
    CancelWorkspaceClose {
        workspace_id: String,
    },
    DeleteWorkspace {
        workspace_id: String,
    },
    Attach {
        client_id: String,
        pid: u32,
    },
    Detach {
        client_id: String,
    },
    PersistReadReceipt {
        client_id: String,
        workspace_id: String,
        session_id: String,
        through: u64,
    },
    PersistDetachedSessionState {
        client_id: String,
        workspace_id: String,
        session_id: String,
        through: u64,
        owner_pid: u32,
        draft: mj_core::storage::DetachedSessionDraft,
    },
    SaveActiveReview {
        session_id: String,
        review: mj_core::storage::StoredReview,
    },
    ClearActiveReview {
        session_id: String,
    },
    SaveWorkspacePaneSizes {
        workspace_id: String,
        sizes: mj_core::workspace::PaneSizes,
    },
    SaveWorkspaceLayout {
        workspace_id: String,
        layout: mj_core::workspace::ConversationLayout,
    },
    PersistImportedSession {
        session: Box<SessionRecord>,
    },
    SetSessionTitle {
        session_id: String,
        title: String,
    },
    SetSessionContainerSettings {
        session_id: String,
        cpus: Option<String>,
        memory: Option<String>,
        mounts: Vec<AdditionalMount>,
        mount_history: Vec<PathBuf>,
    },
    SetSessionAcpTitle {
        session_id: String,
        title: Option<String>,
    },
    MarkSessionTargetMissing {
        session_id: String,
        detail: String,
        updated_at: String,
    },
    CheckpointSession {
        session_id: String,
    },
    /// Search the user's SessionWiki index. An empty query lists the most
    /// recent sessions.
    WikiSearch {
        query: String,
        limit: usize,
    },
    /// The markdown briefing for one indexed session.
    WikiBrief {
        wiki_id: String,
        max_chars: usize,
    },
    /// The passages of one indexed session that match a query, with context.
    WikiHits {
        wiki_id: String,
        query: String,
        context_messages: usize,
        per_message_chars: usize,
    },
    /// What one indexed session is, and what continuing it would mean.
    WikiSession {
        wiki_id: String,
    },
    /// Start a new session from an archived one's transcript.
    WikiRestore(WikiRestoreRequest),
    ScanRecovery {
        all_instances: bool,
    },
    AdoptRecovery {
        session_id: String,
        target_id: String,
        profile: Option<String>,
        bundle: Option<String>,
        all_instances: bool,
    },
    DestroyRecovery {
        session_id: String,
        target_id: String,
        confirmation: String,
        all_instances: bool,
    },
    Snapshot {
        workspace_id: String,
    },
    RuntimeSnapshot {
        workspace_id: String,
        after_revision: u64,
        #[serde(default)]
        all_workspaces: bool,
    },
    RenameProfile {
        old_id: String,
        new_id: String,
    },
    RenameTarget {
        old_id: String,
        new_id: String,
    },
    SubmitSessionCommand {
        #[serde(default)]
        inherited_draft: Option<String>,
        session_id: String,
        command_id: String,
        command: RelayCommand,
    },
    /// Deliver a prompt typed while a session was still starting, once the
    /// daemon sees that session's harness become ready. The daemon owns the
    /// wait, so the prompt arrives whether or not this client is still
    /// running or still showing that session.
    QueueStartupPrompt {
        session_id: String,
        text: String,
        /// The saved draft text this prompt was typed from, if the client
        /// also persisted it. Cleared after a successful submit so the
        /// delivered prompt does not reappear as a draft.
        #[serde(default)]
        inherited_draft: Option<String>,
    },
    SyncSession {
        session_id: String,
    },
    RespondElicitation {
        session_id: String,
        elicitation_id: String,
        response: ElicitationResponse,
    },
    StopBackgroundTask {
        session_id: String,
        background_task_id: String,
    },
    /// Drive a session's second-opinion reviewer. The reviewer is a sidecar of
    /// the session's worker, so it travels the session's own relay rather than
    /// becoming a session of its own here.
    ReviewerAction {
        session_id: String,
        /// Which reviewing role the action drives; absent means the default
        /// one, which is what plan review uses.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<String>,
        action: crate::session::ReviewerAction,
    },
    /// Review the turn this session just finished, on a surface's request.
    StartTurnReview {
        session_id: String,
    },
    /// Forward, dismiss, or cancel the open review.
    ResolveTurnReview {
        session_id: String,
        resolution: Resolution,
    },
    SuspendSession {
        session_id: String,
        #[serde(default)]
        acknowledge_unpublished_work: bool,
    },
    StartCreateSession(CreateSessionRequest),
    WaitCreateSession {
        session_id: String,
    },
    ResumeSession(ResumeSessionRequest),
    PrepareMoveSession(MoveSelection),
    MoveSession(MoveSessionRequest),
    DiscardSinceCheckpoint {
        session_id: String,
        checkpoint: mj_core::state::CheckpointMetadata,
    },
    DestroyStoppedSession {
        session_id: String,
        /// Whether to delete the session's managed git branch as well. The
        /// branch can hold work the user still wants, so destroying keeps it
        /// unless the request asks for the deletion.
        delete_branch: bool,
    },
    ForceDestroySession {
        session_id: String,
        /// See [`DaemonAction::DestroyStoppedSession`].
        delete_branch: bool,
    },
    ForceDeleteWorkspace {
        workspace_id: String,
    },
    CancelLifecycle {
        session_id: String,
    },
    RecoverDraft {
        draft_id: String,
    },
    Stop,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestEnvelope {
    pub protocol_version: u32,
    pub request_id: u64,
    pub token: String,
    pub action: DaemonAction,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseEnvelope {
    pub protocol_version: u32,
    pub request_id: u64,
    pub result: std::result::Result<DaemonReply, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reply", content = "value")]
pub enum DaemonReply {
    NativeAgentHistory(mj_core::native_agent::NativeAgentHistoryPage),
    Pong,
    UpgradePending,
    /// Labels for the work holding an automatic handoff open, empty when
    /// nothing is. Answers `DaemonAction::UpgradeBlockers`.
    UpgradeBlockers(Vec<String>),
    Status(DaemonStatus),
    WebViewerAccess(crate::web::WebViewerAccess),
    WebListeners(Vec<crate::web::WebListenerProcess>),
    Workspaces(Vec<WorkspaceListing>),
    Workspace(WorkspaceRecord),
    Snapshot(WorkspaceSnapshot),
    RuntimeSnapshot(Box<RuntimeSnapshot>),
    RegisteredSession(Box<RegisteredSession>),
    MovePreparation(Box<MovePreparation>),
    MoveOutcome(MoveOutcome),
    Ordinal(u64),
    Text(String),
    OptionalSessionState(Option<SessionState>),
    Checkpoint(mj_core::state::CheckpointMetadata),
    RecoveryScan(mj_core::state::RecoveryScan),
    WikiRows(WikiSearchPage),
    WikiHits(Option<WikiHitTranscript>),
    WikiSession(Option<Box<WikiSessionInfo>>),
    Reviewer(Box<crate::session::ReviewerOutcome>),
    Done,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonStatus {
    pub pid: u32,
    pub started_at: String,
    pub build_version: String,
    pub attached_clients: usize,
    pub phone_status: WebViewerStatus,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum WebViewerStatus {
    Disabled,
    Starting,
    Ready {
        viewer_url: String,
        viewer_code: String,
        qr_login_url: Option<String>,
        fallback_reason: Option<String>,
    },
    Stopped,
    Error {
        message: String,
    },
}

impl std::fmt::Debug for WebViewerStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready {
                viewer_url,
                viewer_code,
                fallback_reason,
                ..
            } => formatter
                .debug_struct("Ready")
                .field("viewer_url", viewer_url)
                .field("viewer_code", viewer_code)
                .field("qr_login_url", &"[redacted]")
                .field("fallback_reason", fallback_reason)
                .finish(),
            Self::Disabled => formatter.write_str("Disabled"),
            Self::Starting => formatter.write_str("Starting"),
            Self::Stopped => formatter.write_str("Stopped"),
            Self::Error { message } => formatter.debug_tuple("Error").field(message).finish(),
        }
    }
}

impl std::fmt::Display for WebViewerStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => formatter.write_str("disabled"),
            Self::Starting => formatter.write_str("starting"),
            Self::Stopped => formatter.write_str("stopped unexpectedly"),
            Self::Error { message } => write!(formatter, "error: {message}"),
            Self::Ready {
                viewer_url,
                viewer_code,
                fallback_reason,
                ..
            } => {
                write!(formatter, "{viewer_url}; viewer code {viewer_code}")?;
                if let Some(reason) = fallback_reason {
                    write!(
                        formatter,
                        "; local only because Tailscale HTTPS is unavailable: {reason}"
                    )?;
                }
                Ok(())
            }
        }
    }
}

/// Whether a non-child process has exited but not yet been reaped.
///
/// A zombie still answers `kill(pid, 0)`, because its process-table entry
/// survives until its parent waits for it — so an existence probe alone calls
/// it alive forever and anything waiting for it to leave waits forever. That is
/// exactly the shape of `mj daemon restart` refusing to restart a daemon that
/// had already stopped: `spawn_detached` used to leave the daemon a child of a
/// long-lived Mjolnir process that never reaped it. It now double-forks, so the
/// daemon is init's to reap, but any other unreaped child of this process would
/// look the same, and the check stays cheap.
///
/// Treating a zombie as gone is also safe in the direction that matters: a
/// zombie's PID cannot be reused until it is reaped, so nothing else can be
/// occupying that number while this returns true.
#[cfg(unix)]
pub fn process_is_zombie(pid: u32) -> bool {
    let pid = sysinfo::Pid::from_u32(pid);
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    system
        .process(pid)
        .is_some_and(|process| process.status() == sysinfo::ProcessStatus::Zombie)
}

/// Wait for a process to leave, within [`STOP_TIMEOUT`].
///
/// The error says the process was still running rather than that it "did not
/// stop": a daemon that is still winding down has not refused, and the two
/// read very differently to somebody deciding whether to reach for a kill.
pub async fn wait_for_exit(pid: u32) -> Result<()> {
    let deadline = Instant::now() + STOP_TIMEOUT;
    while daemon_process_is_alive(pid) {
        ensure!(Instant::now() < deadline, "process {pid} is still running");
        tokio::time::sleep(RETRY_DELAY).await;
    }
    Ok(())
}

/// Whether a daemon that Mjolnir launched in its own process group is alive.
///
/// Reaping is deliberately confined to this daemon-specific path. Attachment
/// PIDs are merely observations and may alias unrelated children owned by this
/// process, so their liveness probe below must never call `waitpid`.
pub fn daemon_process_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        if pid == 0 {
            return false;
        }
        let Ok(raw_pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        let mut status = 0;
        // SAFETY: `status` is writable for the call and WNOHANG never blocks.
        // A non-child fails with ECHILD without changing any process state.
        let waited = unsafe { libc::waitpid(raw_pid, &mut status, libc::WNOHANG) };
        if waited == raw_pid {
            return false;
        }
        if waited == 0 {
            return true;
        }
        let wait_error = std::io::Error::last_os_error();
        if wait_error.raw_os_error() != Some(libc::ECHILD) {
            return true;
        }

        #[cfg(target_os = "macos")]
        return owned_daemon_group_is_alive(raw_pid);

        #[cfg(not(target_os = "macos"))]
        process_is_alive(pid)
    }
    #[cfg(not(unix))]
    process_is_alive(pid)
}

#[cfg(target_os = "macos")]
pub fn owned_daemon_group_is_alive(pid: libc::pid_t) -> bool {
    // `spawn_detached` makes the daemon a process-group leader. Darwin
    // excludes zombies from group signal probes: ESRCH means the group is gone
    // and EPERM means only exiting members remain. The latter is safe here
    // because this is a group we created for our own same-user child, not an
    // arbitrary process group.
    // SAFETY: signal 0 is only an existence probe, and the negative PID targets
    // the daemon-owned group rather than another process.
    if unsafe { libc::kill(-pid, 0) } == 0 {
        return true;
    }
    let error = std::io::Error::last_os_error();
    !matches!(error.raw_os_error(), Some(libc::ESRCH) | Some(libc::EPERM))
}

pub fn process_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        if pid == 0 {
            return false;
        }
        let Ok(raw_pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // SAFETY: kill(pid, 0) sends no signal and is the standard existence
        // probe. EPERM still means the process exists.
        let result = unsafe { libc::kill(raw_pid, 0) };
        let exists =
            result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        exists && !process_is_zombie(pid)
    }
    #[cfg(not(unix))]
    {
        let process_id = sysinfo::Pid::from_u32(pid);
        let mut system = sysinfo::System::new();
        system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[process_id]), true);
        system.process(process_id).is_some()
    }
}

pub fn read_metadata() -> Result<DaemonMetadata> {
    let metadata = read_metadata_any()?;
    ensure!(
        metadata.protocol_version == PROTOCOL_VERSION,
        "daemon protocol {} is incompatible with client protocol {}",
        metadata.protocol_version,
        PROTOCOL_VERSION
    );
    Ok(metadata)
}

pub fn read_metadata_any() -> Result<DaemonMetadata> {
    let path = metadata_path();
    let body = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let metadata: DaemonMetadata =
        serde_json::from_slice(&body).with_context(|| format!("parse {}", path.display()))?;
    Ok(metadata)
}

pub async fn write_frame<T: Serialize>(stream: &mut TcpStream, value: &T) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    write_encoded_frame(stream, &body).await
}

pub async fn write_encoded_frame(stream: &mut TcpStream, body: &[u8]) -> Result<()> {
    ensure!(
        body.len() <= MAX_FRAME_BYTES,
        "daemon frame is too large: {} bytes exceeds {MAX_FRAME_BYTES}",
        body.len()
    );
    stream.write_u32(body.len() as u32).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

pub async fn read_frame<T: for<'de> Deserialize<'de>>(stream: &mut TcpStream) -> Result<T> {
    let length = stream.read_u32().await? as usize;
    ensure!(
        length <= MAX_FRAME_BYTES,
        "daemon frame exceeds {MAX_FRAME_BYTES} bytes"
    );
    let mut body = vec![0_u8; length];
    stream.read_exact(&mut body).await?;
    serde_json::from_slice(&body).context("decode daemon frame")
}

pub struct DaemonClient {
    metadata: DaemonMetadata,
    stream: TcpStream,
    next_request_id: u64,
}

impl DaemonClient {
    pub async fn connect(metadata: DaemonMetadata) -> Result<Self> {
        let stream =
            tokio::time::timeout(Duration::from_secs(1), TcpStream::connect(metadata.address))
                .await
                .context("time out connecting to Mjolnir daemon")??;
        Ok(Self {
            metadata,
            stream,
            next_request_id: 1,
        })
    }

    /// Speak the daemon's advertised dialect, not this build's: management
    /// requests must reach daemons of any protocol version, and the frozen
    /// subset encodes identically across all of them.
    pub async fn request(&mut self, action: DaemonAction) -> Result<DaemonReply> {
        self.request_with_reconnect(action, || async {
            loop {
                if let Ok(client) = connect_existing().await {
                    return Ok(client);
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await
    }

    async fn request_with_reconnect<F, Fut>(
        &mut self,
        action: DaemonAction,
        mut reconnect: F,
    ) -> Result<DaemonReply>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<Self>>,
    {
        loop {
            let response = self.request_once(action.clone()).await?;
            if matches!(response, DaemonReply::UpgradePending)
                && !matches!(action, DaemonAction::PrepareUpgrade)
            {
                // Only an explicit refusal guarantees non-admission. Never
                // replay arbitrary mutations after a lost acknowledgement.
                tokio::time::sleep(Duration::from_millis(100)).await;
                *self = reconnect().await?;
            } else {
                return Ok(response);
            }
        }
    }

    async fn request_once(&mut self, action: DaemonAction) -> Result<DaemonReply> {
        let protocol_version = self.metadata.protocol_version;
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        write_frame(
            &mut self.stream,
            &RequestEnvelope {
                protocol_version,
                request_id,
                token: self.metadata.token.clone(),
                action,
            },
        )
        .await?;
        let response: ResponseEnvelope = read_frame(&mut self.stream).await?;
        ensure!(
            response.protocol_version == protocol_version,
            "daemon changed protocol"
        );
        ensure!(
            response.request_id == request_id,
            "daemon crossed request IDs"
        );
        response.result.map_err(anyhow::Error::msg)
    }

    pub async fn status(&mut self) -> Result<DaemonStatus> {
        match self.request(DaemonAction::Status).await? {
            DaemonReply::Status(status) => Ok(status),
            reply => bail!("unexpected daemon status reply {reply:?}"),
        }
    }

    pub async fn web_access(&mut self) -> Result<crate::web::WebViewerAccess> {
        match self.request(DaemonAction::WebViewerAccess).await? {
            DaemonReply::WebViewerAccess(access) => Ok(access),
            reply => bail!("unexpected web viewer reply {reply:?}"),
        }
    }

    pub async fn recover_web_viewer(
        &mut self,
        action: crate::web::WebViewerRecovery,
    ) -> Result<()> {
        match self.request(DaemonAction::RecoverWebViewer(action)).await? {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected web viewer recovery reply {reply:?}"),
        }
    }

    pub async fn inspect_web_listener(&mut self) -> Result<Vec<crate::web::WebListenerProcess>> {
        match self.request(DaemonAction::InspectWebListener).await? {
            DaemonReply::WebListeners(processes) => Ok(processes),
            reply => bail!("unexpected listener inspection reply {reply:?}"),
        }
    }

    pub async fn list_workspaces(&mut self) -> Result<Vec<WorkspaceListing>> {
        match self.request(DaemonAction::ListWorkspaces).await? {
            DaemonReply::Workspaces(workspaces) => Ok(workspaces),
            reply => bail!("unexpected daemon workspace reply {reply:?}"),
        }
    }

    pub async fn rename_profile(&mut self, old_id: String, new_id: String) -> Result<()> {
        match self
            .request(DaemonAction::RenameProfile { old_id, new_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected rename-profile reply {reply:?}"),
        }
    }

    pub async fn rename_target(&mut self, old_id: String, new_id: String) -> Result<()> {
        match self
            .request(DaemonAction::RenameTarget { old_id, new_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected rename-target reply {reply:?}"),
        }
    }

    pub async fn create_workspace(&mut self, name: String) -> Result<WorkspaceRecord> {
        match self.request(DaemonAction::CreateWorkspace { name }).await? {
            DaemonReply::Workspace(workspace) => Ok(workspace),
            reply => bail!("unexpected create-workspace reply {reply:?}"),
        }
    }

    pub async fn rename_workspace(&mut self, workspace_id: String, name: String) -> Result<()> {
        match self
            .request(DaemonAction::RenameWorkspace { workspace_id, name })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected rename-workspace reply {reply:?}"),
        }
    }

    pub async fn touch_workspace(&mut self, workspace_id: String) -> Result<()> {
        match self
            .request(DaemonAction::TouchWorkspace { workspace_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected touch-workspace reply {reply:?}"),
        }
    }

    pub async fn cancel_workspace_close(&mut self, workspace_id: String) -> Result<()> {
        match self
            .request(DaemonAction::CancelWorkspaceClose { workspace_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected cancel workspace close reply: {reply:?}"),
        }
    }

    pub async fn close_workspace(&mut self, workspace_id: String) -> Result<()> {
        match self
            .request(DaemonAction::CloseWorkspace { workspace_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected close workspace reply: {reply:?}"),
        }
    }

    pub async fn delete_workspace(&mut self, workspace_id: String) -> Result<()> {
        match self
            .request(DaemonAction::DeleteWorkspace { workspace_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected delete-workspace reply {reply:?}"),
        }
    }

    pub async fn attach(&mut self, client_id: String, pid: u32) -> Result<()> {
        match self
            .request(DaemonAction::Attach { client_id, pid })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected attach reply {reply:?}"),
        }
    }

    pub async fn detach(&mut self, client_id: String) -> Result<()> {
        match self.request(DaemonAction::Detach { client_id }).await? {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected detach reply {reply:?}"),
        }
    }

    pub async fn persist_read_receipt(
        &mut self,
        client_id: String,
        workspace_id: String,
        session_id: String,
        through: u64,
    ) -> Result<u64> {
        match self
            .request(DaemonAction::PersistReadReceipt {
                client_id,
                workspace_id,
                session_id,
                through,
            })
            .await?
        {
            DaemonReply::Ordinal(ordinal) => Ok(ordinal),
            reply => bail!("unexpected read-receipt reply {reply:?}"),
        }
    }

    pub async fn persist_detached_session_state(
        &mut self,
        client_id: String,
        workspace_id: String,
        session_id: String,
        through: u64,
        owner_pid: u32,
        draft: mj_core::storage::DetachedSessionDraft,
    ) -> Result<()> {
        match self
            .request(DaemonAction::PersistDetachedSessionState {
                client_id,
                workspace_id,
                session_id,
                through,
                owner_pid,
                draft,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected detached-session-state reply {reply:?}"),
        }
    }

    pub async fn save_active_review(
        &mut self,
        session_id: String,
        review: mj_core::storage::StoredReview,
    ) -> Result<()> {
        match self
            .request(DaemonAction::SaveActiveReview { session_id, review })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected save-review reply {reply:?}"),
        }
    }

    pub async fn clear_active_review(&mut self, session_id: String) -> Result<()> {
        match self
            .request(DaemonAction::ClearActiveReview { session_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected clear-review reply {reply:?}"),
        }
    }

    pub async fn save_workspace_pane_sizes(
        &mut self,
        workspace_id: String,
        sizes: mj_core::workspace::PaneSizes,
    ) -> Result<()> {
        match self
            .request(DaemonAction::SaveWorkspacePaneSizes {
                workspace_id,
                sizes,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected pane-size save reply {reply:?}"),
        }
    }

    pub async fn save_workspace_layout(
        &mut self,
        workspace_id: String,
        layout: mj_core::workspace::ConversationLayout,
    ) -> Result<()> {
        match self
            .request(DaemonAction::SaveWorkspaceLayout {
                workspace_id,
                layout,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected layout save reply {reply:?}"),
        }
    }

    pub async fn persist_imported_session(&mut self, session: SessionRecord) -> Result<()> {
        match self
            .request(DaemonAction::PersistImportedSession {
                session: Box::new(session),
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected imported-session reply {reply:?}"),
        }
    }

    pub async fn set_session_title(&mut self, session_id: String, title: String) -> Result<String> {
        match self
            .request(DaemonAction::SetSessionTitle { session_id, title })
            .await?
        {
            DaemonReply::Text(title) => Ok(title),
            reply => bail!("unexpected session-title reply {reply:?}"),
        }
    }

    pub async fn set_session_container_settings(
        &mut self,
        session_id: String,
        cpus: Option<String>,
        memory: Option<String>,
        mounts: Vec<AdditionalMount>,
        mount_history: Vec<PathBuf>,
    ) -> Result<()> {
        match self
            .request(DaemonAction::SetSessionContainerSettings {
                session_id,
                cpus,
                memory,
                mounts,
                mount_history,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected container-settings reply {reply:?}"),
        }
    }

    pub async fn set_session_acp_title(
        &mut self,
        session_id: String,
        title: Option<String>,
    ) -> Result<()> {
        match self
            .request(DaemonAction::SetSessionAcpTitle { session_id, title })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected ACP-title reply {reply:?}"),
        }
    }

    pub async fn mark_session_target_missing(
        &mut self,
        session_id: String,
        detail: String,
        updated_at: String,
    ) -> Result<Option<SessionState>> {
        match self
            .request(DaemonAction::MarkSessionTargetMissing {
                session_id,
                detail,
                updated_at,
            })
            .await?
        {
            DaemonReply::OptionalSessionState(state) => Ok(state),
            reply => bail!("unexpected target-missing reply {reply:?}"),
        }
    }

    pub async fn checkpoint_session(
        &mut self,
        session_id: String,
    ) -> Result<mj_core::state::CheckpointMetadata> {
        match self
            .request(DaemonAction::CheckpointSession { session_id })
            .await?
        {
            DaemonReply::Checkpoint(checkpoint) => Ok(checkpoint),
            reply => bail!("unexpected checkpoint reply {reply:?}"),
        }
    }

    /// Search the user's SessionWiki index, newest first when the query is
    /// empty and best match first otherwise. The reply carries the state of
    /// the index as well as the rows, so a caller can say the first build is
    /// still running.
    pub async fn wiki_search(&mut self, query: String, limit: usize) -> Result<WikiSearchPage> {
        match self
            .request(DaemonAction::WikiSearch { query, limit })
            .await?
        {
            DaemonReply::WikiRows(page) => Ok(page),
            reply => bail!("unexpected SessionWiki search reply {reply:?}"),
        }
    }

    /// The markdown briefing for one indexed session.
    pub async fn wiki_brief(&mut self, wiki_id: String, max_chars: usize) -> Result<String> {
        match self
            .request(DaemonAction::WikiBrief { wiki_id, max_chars })
            .await?
        {
            DaemonReply::Text(markdown) => Ok(markdown),
            reply => bail!("unexpected SessionWiki brief reply {reply:?}"),
        }
    }

    /// The passages of one indexed session that match a query, each matching
    /// message with `context_messages` neighbours on either side and its text
    /// capped at `per_message_chars`. `None` when the index holds no session
    /// with that id.
    pub async fn wiki_hits(
        &mut self,
        wiki_id: String,
        query: String,
        context_messages: usize,
        per_message_chars: usize,
    ) -> Result<Option<WikiHitTranscript>> {
        match self
            .request(DaemonAction::WikiHits {
                wiki_id,
                query,
                context_messages,
                per_message_chars,
            })
            .await?
        {
            DaemonReply::WikiHits(transcript) => Ok(transcript),
            reply => bail!("unexpected SessionWiki hits reply {reply:?}"),
        }
    }

    /// What the index knows about one session, or `None` when the index holds
    /// no session with that id.
    pub async fn wiki_session(&mut self, wiki_id: String) -> Result<Option<WikiSessionInfo>> {
        match self.request(DaemonAction::WikiSession { wiki_id }).await? {
            DaemonReply::WikiSession(info) => Ok(info.map(|info| *info)),
            reply => bail!("unexpected SessionWiki session reply {reply:?}"),
        }
    }

    /// Start a new session carrying a hand-off compacted from an archived one.
    /// It answers like any other session start: the record exists and is
    /// provisioning, and the hand-off follows once the harness is ready.
    pub async fn wiki_restore(&mut self, request: WikiRestoreRequest) -> Result<RegisteredSession> {
        match self.request(DaemonAction::WikiRestore(request)).await? {
            DaemonReply::RegisteredSession(registered) => Ok(*registered),
            reply => bail!("unexpected SessionWiki restore reply {reply:?}"),
        }
    }

    pub async fn scan_recovery(
        &mut self,
        all_instances: bool,
    ) -> Result<mj_core::state::RecoveryScan> {
        match self
            .request(DaemonAction::ScanRecovery { all_instances })
            .await?
        {
            DaemonReply::RecoveryScan(scan) => Ok(scan),
            reply => bail!("unexpected recovery-scan reply {reply:?}"),
        }
    }

    pub async fn adopt_recovery(
        &mut self,
        session_id: String,
        target_id: String,
        profile: Option<String>,
        bundle: Option<String>,
        all_instances: bool,
    ) -> Result<()> {
        match self
            .request(DaemonAction::AdoptRecovery {
                session_id,
                target_id,
                profile,
                bundle,
                all_instances,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected recovery-adopt reply {reply:?}"),
        }
    }

    pub async fn destroy_recovery(
        &mut self,
        session_id: String,
        target_id: String,
        confirmation: String,
        all_instances: bool,
    ) -> Result<()> {
        match self
            .request(DaemonAction::DestroyRecovery {
                session_id,
                target_id,
                confirmation,
                all_instances,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected recovery-destroy reply {reply:?}"),
        }
    }

    pub async fn snapshot(&mut self, workspace_id: String) -> Result<WorkspaceSnapshot> {
        match self
            .request(DaemonAction::Snapshot { workspace_id })
            .await?
        {
            DaemonReply::Snapshot(snapshot) => Ok(snapshot),
            reply => bail!("unexpected snapshot reply {reply:?}"),
        }
    }

    pub async fn runtime_snapshot(
        &mut self,
        workspace_id: String,
        after_revision: u64,
        all_workspaces: bool,
    ) -> Result<RuntimeSnapshot> {
        match self
            .request(DaemonAction::RuntimeSnapshot {
                workspace_id,
                after_revision,
                all_workspaces,
            })
            .await?
        {
            DaemonReply::RuntimeSnapshot(snapshot) => Ok(*snapshot),
            reply => bail!("unexpected runtime snapshot reply {reply:?}"),
        }
    }

    pub async fn submit_session_command(
        &mut self,
        session_id: String,
        command_id: String,
        command: RelayCommand,
        inherited_draft: Option<String>,
    ) -> Result<u64> {
        match self
            .request(DaemonAction::SubmitSessionCommand {
                inherited_draft,
                session_id,
                command_id,
                command,
            })
            .await?
        {
            DaemonReply::Ordinal(ordinal) => Ok(ordinal),
            reply => bail!("unexpected session command reply {reply:?}"),
        }
    }

    /// Hand the daemon a prompt for a session that is still starting. The
    /// daemon replies as soon as the prompt is queued, not when it is
    /// delivered; delivery failures come back as a session notice.
    pub async fn queue_startup_prompt(
        &mut self,
        session_id: String,
        text: String,
        inherited_draft: Option<String>,
    ) -> Result<()> {
        match self
            .request(DaemonAction::QueueStartupPrompt {
                session_id,
                text,
                inherited_draft,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected startup prompt reply {reply:?}"),
        }
    }

    /// Ask the daemon to review the turn this session just finished.
    ///
    /// The refusal is a sentence for a person -- "prompts are queued", "set
    /// [review] profile in config.toml" -- so it travels as text rather than
    /// as a code every surface would have to translate.
    pub async fn start_turn_review(&mut self, session_id: String) -> Result<()> {
        match self
            .request(DaemonAction::StartTurnReview { session_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected turn-review reply {reply:?}"),
        }
    }

    pub async fn resolve_turn_review(
        &mut self,
        session_id: String,
        resolution: Resolution,
    ) -> Result<()> {
        match self
            .request(DaemonAction::ResolveTurnReview {
                session_id,
                resolution,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected turn-review resolution reply {reply:?}"),
        }
    }

    pub async fn reviewer_action(
        &mut self,
        session_id: String,
        role: Option<String>,
        action: crate::session::ReviewerAction,
    ) -> Result<crate::session::ReviewerOutcome> {
        match self
            .request(DaemonAction::ReviewerAction {
                session_id,
                role,
                action,
            })
            .await?
        {
            DaemonReply::Reviewer(outcome) => Ok(*outcome),
            reply => bail!("unexpected reviewer reply {reply:?}"),
        }
    }

    pub async fn sync_session(&mut self, session_id: String) -> Result<()> {
        match self
            .request(DaemonAction::SyncSession { session_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected session sync reply {reply:?}"),
        }
    }

    pub async fn respond_elicitation(
        &mut self,
        session_id: String,
        elicitation_id: String,
        response: ElicitationResponse,
    ) -> Result<()> {
        match self
            .request(DaemonAction::RespondElicitation {
                session_id,
                elicitation_id,
                response,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected elicitation reply {reply:?}"),
        }
    }

    pub async fn native_agent_history(
        &mut self,
        owner: String,
        child: String,
        before: Option<(u64, String)>,
    ) -> Result<mj_core::native_agent::NativeAgentHistoryPage> {
        match self
            .request(DaemonAction::NativeAgentHistory {
                owner,
                child,
                before,
            })
            .await?
        {
            DaemonReply::NativeAgentHistory(page) => Ok(page),
            reply => bail!("unexpected native agent history reply {reply:?}"),
        }
    }

    pub async fn stop_background_task(
        &mut self,
        session_id: String,
        background_task_id: String,
    ) -> Result<()> {
        match self
            .request(DaemonAction::StopBackgroundTask {
                session_id,
                background_task_id,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected background task stop reply {reply:?}"),
        }
    }

    pub async fn suspend_session(&mut self, session_id: String) -> Result<()> {
        self.suspend_session_with_ack(session_id, false).await
    }

    pub async fn suspend_session_with_ack(
        &mut self,
        session_id: String,
        acknowledge_unpublished_work: bool,
    ) -> Result<()> {
        match self
            .request(DaemonAction::SuspendSession {
                session_id,
                acknowledge_unpublished_work,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected close-session reply {reply:?}"),
        }
    }

    pub async fn start_create_session(
        &mut self,
        request: CreateSessionRequest,
    ) -> Result<RegisteredSession> {
        match self
            .request(DaemonAction::StartCreateSession(request))
            .await?
        {
            DaemonReply::RegisteredSession(registered) => Ok(*registered),
            reply => bail!("unexpected start-create reply {reply:?}"),
        }
    }

    pub async fn wait_create_session(&mut self, session_id: String) -> Result<()> {
        match self
            .request(DaemonAction::WaitCreateSession { session_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected wait-create reply {reply:?}"),
        }
    }

    pub async fn resume_session(&mut self, request: ResumeSessionRequest) -> Result<()> {
        match self.request(DaemonAction::ResumeSession(request)).await? {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected resume-session reply {reply:?}"),
        }
    }

    pub async fn discard_since_checkpoint(
        &mut self,
        session_id: String,
        checkpoint: mj_core::state::CheckpointMetadata,
    ) -> Result<()> {
        match self
            .request(DaemonAction::DiscardSinceCheckpoint {
                session_id,
                checkpoint,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected force-stop reply {reply:?}"),
        }
    }

    pub async fn destroy_stopped_session(
        &mut self,
        session_id: String,
        delete_branch: bool,
    ) -> Result<()> {
        match self
            .request(DaemonAction::DestroyStoppedSession {
                session_id,
                delete_branch,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected destroy-stopped reply {reply:?}"),
        }
    }

    pub async fn force_destroy_session(
        &mut self,
        session_id: String,
        delete_branch: bool,
    ) -> Result<()> {
        match self
            .request(DaemonAction::ForceDestroySession {
                session_id,
                delete_branch,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected force-destroy reply {reply:?}"),
        }
    }

    pub async fn force_delete_workspace(&mut self, workspace_id: String) -> Result<()> {
        match self
            .request(DaemonAction::ForceDeleteWorkspace { workspace_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected force-delete-workspace reply {reply:?}"),
        }
    }

    pub async fn cancel_lifecycle(&mut self, session_id: String) -> Result<()> {
        match self
            .request(DaemonAction::CancelLifecycle { session_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected cancel-lifecycle reply {reply:?}"),
        }
    }

    pub async fn recover_draft(&mut self, draft_id: String) -> Result<()> {
        match self
            .request(DaemonAction::RecoverDraft { draft_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected recover-draft reply {reply:?}"),
        }
    }

    pub async fn stop(&mut self) -> Result<()> {
        match self.request(DaemonAction::Stop).await? {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected stop reply {reply:?}"),
        }
    }
}

pub async fn connect_existing() -> Result<DaemonClient> {
    let metadata = tokio::task::spawn_blocking(read_metadata)
        .await
        .context("read daemon metadata task failed")??;
    DaemonClient::connect(metadata).await
}

/// A handle to whatever daemon the metadata file advertises, regardless of its
/// protocol version. It only exposes the frozen management subset (`Ping`,
/// `Status`, `Stop`), which encodes identically in every protocol version.
pub struct ManagementClient {
    inner: DaemonClient,
}

impl ManagementClient {
    pub fn new(inner: DaemonClient) -> Self {
        Self { inner }
    }
    pub fn protocol_version(&self) -> u32 {
        self.inner.metadata.protocol_version
    }

    pub async fn status(&mut self) -> Result<DaemonStatus> {
        self.inner.status().await
    }

    pub async fn stop(&mut self) -> Result<()> {
        tokio::time::timeout(STOP_TIMEOUT, self.inner.stop())
            .await
            .context("Mjolnir daemon did not acknowledge the stop before the deadline")?
    }

    /// Ask the daemon to stop and wait for its process to actually exit.
    pub async fn stop_and_wait(mut self) -> Result<()> {
        let pid = self.inner.metadata.pid;
        self.stop().await?;
        wait_for_exit(pid).await.with_context(|| {
            format!(
                "Mjolnir daemon {pid} accepted the stop but was still running after {}s",
                STOP_TIMEOUT.as_secs()
            )
        })
    }
}

pub async fn connect_management() -> Result<ManagementClient> {
    Ok(ManagementClient {
        inner: DaemonClient::connect(read_metadata_any()?).await?,
    })
}

/// Refuse to speak to a daemon whose protocol this build does not know.
///
/// The two protocol numbers alone do not say which `mj` ran. The usual cause is
/// a second installation: a `cargo install`ed client sits earlier on PATH than
/// the build whose daemon is running, so every command fails here while the
/// other binary works, and nothing in the message says where either lives. It
/// therefore names both executables and both versions.
pub fn ensure_supported_daemon_protocol(metadata: &DaemonMetadata) -> Result<()> {
    ensure!(
        metadata.protocol_version <= PROTOCOL_VERSION,
        "{}",
        unsupported_daemon_protocol_message(
            metadata.protocol_version,
            &describe_running_daemon_and_client_builds(metadata.pid, &metadata.build_version),
        )
    );
    Ok(())
}

/// The message [`ensure_supported_daemon_protocol`] fails with, given the
/// sentence that names both builds, so it can be read without a daemon.
fn unsupported_daemon_protocol_message(daemon_protocol: u32, builds: &str) -> String {
    format!(
        "the daemon uses a newer protocol ({daemon_protocol}) than this client ({PROTOCOL_VERSION}). {builds}. \
         Put the daemon's directory first on PATH, or reinstall this client from that build."
    )
}
pub const PROTOCOL_VERSION: u32 = 33;
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// How long a daemon is given to exit after it accepts a stop.
///
/// Stopping cancels a token and returns immediately; the daemon then unwinds
/// its session manager, its phone server and its pollers. That is normally
/// fast, but a daemon whose database has been migrated out from under it fails
/// every read while it winds down and has been observed taking over five
/// seconds — which the previous five-second bound missed by a fraction,
/// reporting a stop that had in fact worked as `did not stop` and aborting the
/// restart that depended on it.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(30);
pub const RETRY_DELAY: Duration = Duration::from_millis(40);
impl DaemonClient {
    pub async fn prepare_move_session(
        &mut self,
        selection: MoveSelection,
    ) -> Result<MovePreparation> {
        match self
            .request(DaemonAction::PrepareMoveSession(selection))
            .await?
        {
            DaemonReply::MovePreparation(preparation) => Ok(*preparation),
            _ => bail!("daemon returned an unexpected move preparation reply"),
        }
    }

    pub async fn move_session(&mut self, request: MoveSessionRequest) -> Result<MoveOutcome> {
        match self.request(DaemonAction::MoveSession(request)).await? {
            DaemonReply::MoveOutcome(outcome) => Ok(outcome),
            _ => bail!("daemon returned an unexpected move reply"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executable::{BuildDescription, describe_daemon_and_client_builds};
    use std::path::Path;

    #[tokio::test]
    async fn upgrade_refusal_retries_the_identical_command_but_lost_acknowledgements_do_not() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let metadata = DaemonMetadata {
            protocol_version: PROTOCOL_VERSION,
            pid: std::process::id(),
            address: listener.local_addr().unwrap(),
            token: "test-token".into(),
            started_at: "test".into(),
            build_version: env!("CARGO_PKG_VERSION").into(),
        };
        let action = DaemonAction::SubmitSessionCommand {
            inherited_draft: None,
            session_id: "test-session".into(),
            command_id: "steer-command".into(),
            command: RelayCommand::Steer {
                active_prompt_id: "active-command".into(),
                queued_prompt_id: "queued-command".into(),
            },
        };
        let expected = serde_json::to_value(&action).unwrap();
        let server = tokio::spawn(async move {
            for reply in [
                Some(DaemonReply::UpgradePending),
                Some(DaemonReply::Done),
                None,
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request: RequestEnvelope = read_frame(&mut stream).await.unwrap();
                assert_eq!(serde_json::to_value(&request.action).unwrap(), expected);
                if let Some(reply) = reply {
                    write_frame(
                        &mut stream,
                        &ResponseEnvelope {
                            protocol_version: request.protocol_version,
                            request_id: request.request_id,
                            result: Ok(reply),
                        },
                    )
                    .await
                    .unwrap();
                }
            }
        });
        let mut client = DaemonClient::connect(metadata.clone()).await.unwrap();
        let reply = client
            .request_with_reconnect(action.clone(), || DaemonClient::connect(metadata.clone()))
            .await
            .unwrap();
        assert!(matches!(reply, DaemonReply::Done));
        let mut client = DaemonClient::connect(metadata).await.unwrap();
        assert!(
            client
                .request_with_reconnect(action, || async {
                    panic!("an ambiguous acknowledgement must not replay a mutation");
                })
                .await
                .is_err()
        );
        server.await.unwrap();
    }

    #[test]
    fn unsupported_protocol_message_names_both_binaries_and_versions() {
        let message = unsupported_daemon_protocol_message(
            PROTOCOL_VERSION + 5,
            &describe_daemon_and_client_builds(
                4242,
                BuildDescription {
                    executable: Some(Path::new("/home/dev/mj/target/release/mj")),
                    version: "2.14.0",
                },
                BuildDescription {
                    executable: Some(Path::new("/home/dev/.cargo/bin/mj")),
                    version: "2.9.0",
                },
            ),
        );
        assert_eq!(
            message,
            format!(
                "the daemon uses a newer protocol ({}) than this client ({PROTOCOL_VERSION}). \
                 Daemon 4242 runs /home/dev/mj/target/release/mj (version 2.14.0), \
                 while this client runs /home/dev/.cargo/bin/mj (version 2.9.0). \
                 Put the daemon's directory first on PATH, or reinstall this client from that build.",
                PROTOCOL_VERSION + 5
            )
        );
    }

    #[test]
    fn unsupported_protocol_message_still_names_the_client_when_the_daemon_file_is_unknown() {
        let message = unsupported_daemon_protocol_message(
            PROTOCOL_VERSION + 1,
            &describe_daemon_and_client_builds(
                4242,
                BuildDescription {
                    executable: None,
                    version: "2.14.0",
                },
                BuildDescription {
                    executable: Some(Path::new("/home/dev/.cargo/bin/mj")),
                    version: "2.9.0",
                },
            ),
        );
        assert!(
            message.contains("Daemon 4242 runs an unknown file (version 2.14.0)"),
            "{message}"
        );
        assert!(
            message.contains("this client runs /home/dev/.cargo/bin/mj (version 2.9.0)"),
            "{message}"
        );
    }
}
