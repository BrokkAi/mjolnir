//! Authenticated local daemon protocol and client transport.
mod chunk_bytes;
use crate::executable::describe_running_daemon_and_client_builds;
use crate::session::{ManagedSessionView, ViewError};
use agent_client_protocol::v1::schema::ContentBlock;
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
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
/// Durable terminal results for caller-owned startup prompt identities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StartupPromptOutcome {
    Accepted,
    Withdrawn,
    Refused {
        encoded_draft: String,
        reason: String,
    },
}

pub fn metadata_path() -> PathBuf {
    data_dir().join("daemon.json")
}

/// The environment variable naming the container a daemon was launched in.
pub use mj_core::placement::DAEMON_CONTAINER_ENV;

/// The environment variable naming the engine that owns the daemon container.
pub const DAEMON_CONTAINER_RUNTIME_ENV: &str = "MJ_DAEMON_CONTAINER_RUNTIME";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonMetadata {
    pub protocol_version: u32,
    pub pid: u32,
    pub address: SocketAddr,
    pub token: String,
    pub started_at: String,
    pub build_version: String,
    /// Random per-start identity. It is skipped on disk so older clients keep
    /// reading `daemon.json`; clients ask the daemon for it with `Identity`.
    #[serde(skip)]
    pub instance_id: String,
    /// Where this daemon runs. Also skipped on disk: the running daemon is its
    /// owner, and a container daemon can report it on every platform.
    #[serde(skip)]
    pub placement: DaemonPlacement,
}

/// Where the daemon runs, as the daemon itself reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "placement")]
pub enum DaemonPlacement {
    #[default]
    Host,
    Container {
        name: String,
        /// The engine that owns the container, when the launcher reported it.
        /// `None` is a container daemon from before this action existed.
        #[serde(default)]
        runtime: Option<mj_core::config::DaemonRuntime>,
    },
}

impl DaemonPlacement {
    /// The container name when this placement is a container.
    pub fn container(&self) -> Option<&str> {
        match self {
            Self::Host => None,
            Self::Container { name, .. } => Some(name),
        }
    }
}

/// One daemon start, as the daemon reports it to a client.
///
/// Unlike the frozen [`DaemonStatus`], this action is not part of the
/// management subset. It answers the questions clients used to ask the
/// operating system: is this the daemon I connected to before, where does it
/// run, and which build is it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonIdentity {
    pub instance_id: String,
    pub build_version: String,
    pub placement: DaemonPlacement,
}

/// What automatic first-run setup found and configured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirstRunSetupReport {
    pub agents: Vec<mj_core::config::HarnessKind>,
    pub repository: Option<String>,
}

impl FirstRunSetupReport {
    pub fn summary(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if self.agents.is_empty() {
            lines.push(
                "No installed coding agent was found. Mjolnir can install and sign in to one for you."
                    .to_owned(),
            );
        } else {
            lines.push(format!(
                "Agents: {}.",
                self.agents
                    .iter()
                    .map(|kind| kind.display_name())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if let Some(repository) = &self.repository {
            lines.push(format!("Project: {repository}."));
        }
        lines
    }
}

/// A project synthesized while importing a native session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportedBundle {
    pub bundle_id: String,
    pub bundle: mj_core::config::ProjectBundle,
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
    /// `muse`, `opencode`, or `dsh`), from the index's `mj-harness:` tag. It
    /// stays meaningful after the profile id has been removed from the
    /// configuration.
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
    /// The index file on disk was written by a newer SessionWiki schema
    /// version. Mjolnir will not open it, because rebuilding it down would only
    /// make the newer program rebuild it back. An older index is not this
    /// state: the daemon rebuilds it, and it reads as `Indexing` meanwhile.
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

/// Where in a session's conversation a text search matched. A match in a
/// user message ranks ahead of one in an agent message; a match only in tool
/// output is not a match at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionTextMatchKind {
    User,
    Agent,
}

/// One live session whose conversation contains a search query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTextMatch {
    pub session_id: String,
    pub kind: SessionTextMatchKind,
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
    /// Set for an archived Mjolnir row whose transcript holds no prompt,
    /// which `mj resume --wiki` has nothing to restore from. Sent only when
    /// set, so a daemon that predates it and a client that predates it both
    /// read every other row unchanged.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub nothing_to_restore: bool,
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

/// What the terminal dashboard reads from the store before it takes the
/// terminal. The daemon assembles it, so a client never opens the store, which
/// may live in a container volume the client cannot reach.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardStartup {
    /// The snapshot and cursor from which the runtime feed continues.
    pub runtime: crate::runtime_feed::RuntimeBootstrap,
    pub metadata: StoredSessionMetadata,
    pub pane_sizes: std::collections::BTreeMap<String, mj_core::workspace::PaneSizes>,
    pub conversation_layouts:
        std::collections::BTreeMap<String, mj_core::workspace::ConversationLayout>,
    pub queued_prompts: std::collections::BTreeMap<String, Vec<mj_core::relay::QueuedPrompt>>,
}

/// Stored per-workspace layouts, keyed by workspace id.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceLayouts {
    pub pane_sizes: std::collections::BTreeMap<String, mj_core::workspace::PaneSizes>,
    pub conversation_layouts:
        std::collections::BTreeMap<String, mj_core::workspace::ConversationLayout>,
}

/// The store-wide metadata the dashboard keeps beside its session rows.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredSessionMetadata {
    pub mount_history: mj_core::snapshot_map::SnapshotMap<String, Vec<PathBuf>>,
    pub container_sizes: mj_core::snapshot_map::SnapshotMap<String, HostContainerSize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSnapshot {
    pub workspace: WorkspaceRecord,
    pub sessions: Vec<SessionPreview>,
    pub drafts: Vec<DraftPreview>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSessionView {
    /// Config selectors owned by the target permission policy.
    #[serde(default)]
    pub policy_config_keys: Vec<String>,
    pub session_id: String,
    pub projection_ordinal: u64,
    pub projection_digest: String,
    pub operational: Option<mj_core::snapshot_map::SharedSnapshot<RelayOperationalState>>,
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
            policy_config_keys: Vec::new(),
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
    /// A sub-agent stopped and removed because its parent is being
    /// suspended. The same teardown as `ForceDestroy`, which surfaces call a
    /// stop, as the suspend does.
    StopSubagent,
    Cleanup,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeLifecycleView {
    pub operation_id: String,
    #[serde(default)]
    pub move_operation_id: Option<String>,
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

/// A lifecycle mutation whose execution belongs to the daemon after acceptance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "operation", content = "options")]
pub enum SessionLifecycleRequest {
    Suspend {
        session_id: String,
        acknowledge_unpublished_work: bool,
        ignore_nested_repositories: bool,
    },
    Restart {
        session_id: String,
        acknowledge_container_recreation: bool,
    },
    Resume(Box<ResumeSessionRequest>),
    Move(Box<MoveSessionRequest>),
    Discard {
        session_id: String,
        checkpoint: mj_core::state::CheckpointMetadata,
    },
    DestroyStopped {
        session_id: String,
        delete_branch: bool,
    },
    ForceDestroy {
        session_id: String,
        delete_branch: bool,
    },
}

impl SessionLifecycleRequest {
    pub fn session_id(&self) -> &str {
        match self {
            Self::Suspend { session_id, .. }
            | Self::Restart { session_id, .. }
            | Self::Discard { session_id, .. }
            | Self::DestroyStopped { session_id, .. }
            | Self::ForceDestroy { session_id, .. } => session_id,
            Self::Resume(request) => &request.session_id,
            Self::Move(request) => &request.preparation.selection.session_id,
        }
    }
}

/// A lost acceptance reply never proves that the daemon refused this mutation.
#[derive(Debug)]
pub struct LifecycleAdmissionUnconfirmed(pub String);

impl std::fmt::Display for LifecycleAdmissionUnconfirmed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Could not confirm daemon acceptance for session {}; inspect session status before retrying",
            self.0
        )
    }
}
impl std::error::Error for LifecycleAdmissionUnconfirmed {}

/// Observation was lost; this says nothing about the mutation's outcome.
#[derive(Debug)]
pub struct LifecycleObservationUnavailable(pub LifecycleReceipt);

impl std::fmt::Display for LifecycleObservationUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Observation of operation {} for session {} is unavailable; inspect session status for its current state",
            self.0.operation_id, self.0.session_id
        )
    }
}
impl std::error::Error for LifecycleObservationUnavailable {}

/// Identifies the admitted operation, independently of later work on its session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleReceipt {
    pub session_id: String,
    pub operation_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSessionRequest {
    /// Allow this interactive launch to seed later new-session forms.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub remember_launch_choices: bool,
    /// Save this session's container size as the default for its physical
    /// host. Independent of the broader interactive launch preferences.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub remember_container_size: bool,
    #[serde(default)]
    pub create_managed_worktree: Option<bool>,
    /// Full commit object ID to start the workspace at: an exact checkout of
    /// the bundle's primary repository, detached unless `branch` is given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    /// With `at`, the new branch created there; otherwise the existing branch
    /// to check out in a new isolated workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Diff base, when it is not `at`. Without `at`, also the starting
    /// revision for a raw managed worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// With `at`, start the workspace with `at`, `base` and their ancestors
    /// only: no `origin` remote, remote-tracking refs, tags, or clone-cache
    /// objects. `base` must then be a full commit object ID, and the bundle
    /// must hold one repository. The session cannot be resumed or moved.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_later_history: bool,
    /// Omitted uses the selected profile's subagent setting.
    #[serde(
        default,
        alias = "mjolnir_subagents",
        deserialize_with = "mj_core::subagent::deserialize_optional_policy"
    )]
    pub subagents: Option<mj_core::subagent::SubagentPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oracle: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oracle_choice: Option<mj_core::config::SubagentChoice>,
    /// Omitted follows `[review]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<mj_core::config::SessionReview>,
    /// Start the session with no project memory. See
    /// `mj_core::state::SessionRecord::no_project_memory`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_project_memory: bool,
    /// Exclude this session from GitHub watcher delivery targets. See
    /// `mj_core::state::SessionRecord::no_github_events`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_github_events: bool,
    /// Withhold GitHub credentials; inherited by children.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_github_credentials: bool,
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

/// The commit author in the daemon host's Git configuration. Sessions on the
/// host use it directly and container sessions inherit it, so without both
/// parts no session can commit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitIdentity {
    pub name: Option<String>,
    pub email: Option<String>,
    /// The global configuration picks an identity per directory with
    /// `includeIf`, which only applies inside a repository. Writing a plain
    /// identity would override those choices, so it is not offered.
    #[serde(default)]
    pub per_directory: bool,
}

impl GitIdentity {
    pub fn is_complete(&self) -> bool {
        self.name.is_some() && self.email.is_some()
    }

    /// Whether a surface should offer to set the identity.
    pub fn needs_setup(&self) -> bool {
        !self.is_complete() && !self.per_directory
    }
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
    PreviewBuildCache {
        machine: mj_core::config::Machine,
    },
    InstallMbx {
        machine: mj_core::config::Machine,
    },
    PreflightNewSession {
        request: NewSessionPreflightRequest,
    },
    ValidateProjectDirectory {
        target_id: String,
        directory: PathBuf,
    },
    CheckpointArchiveSizes {
        paths: std::collections::BTreeMap<String, PathBuf>,
    },
    SaveSpinnerStyle {
        style: mj_core::config::SpinnerStyle,
    },
    TestTarget {
        target_id: String,
    },
    ProfileLoginStatuses,
    CheckAvailability,
    DiscoverSetup {
        scope: crate::discovery::DetectScope,
        machines: Vec<(String, mj_core::config::Machine)>,
    },
    CompletePath {
        host: mj_core::path_completion::CompletionHost,
        prefix: String,
        kind: mj_core::path_completion::CompletionKind,
    },
    CheckProjectDirectories {
        paths: Vec<std::path::PathBuf>,
    },
    DiscoverProjects {
        request: mj_core::project_picker::ProjectDiscoveryRequest,
    },
    ResolveContainerPath {
        target_id: String,
        source: std::path::PathBuf,
    },
    ResolveSetupPath {
        machine: mj_core::config::Machine,
        source: std::path::PathBuf,
    },
    ValidateMountSource {
        target_id: String,
        source: std::path::PathBuf,
    },
    ValidateSessionMounts {
        target_id: String,
        mounts: Vec<mj_core::targets::AdditionalMount>,
    },
    RecordOperatorActivity,
    SetOperatorAway {
        away: bool,
    },
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
    /// This daemon's per-start identity, build and placement. Added after
    /// protocol 74, so it is deliberately not part of the frozen management
    /// subset: a client asks only a daemon of its own protocol.
    Identity,
    /// The configuration the daemon serves. Clients read it here instead of
    /// opening `config.toml`, which belongs to the daemon.
    Configuration,
    /// Discover and persist the first-run configuration in the daemon's own
    /// environment. A client in another placement asks its daemon instead of
    /// writing the client's `config.toml`.
    RunFirstRunSetup,
    /// Run the doctor in the daemon's own environment and return the
    /// actionable errors, which a client shows after first-run setup.
    RunDoctor,
    /// Check a project's Git remotes before a new session starts, in the
    /// environment that owns the project paths.
    NewSessionRemoteRepairs {
        bundle_id: String,
        target_id: String,
    },
    /// Apply the remote-tracking repairs a preflight proposed, in the
    /// environment that owns the project checkout.
    RepairRepositoryRemotes {
        bundle_id: String,
        repairs: Vec<mj_core::local_git::LocalRemoteRepair>,
    },
    /// The project source label for a session, read in the environment that
    /// owns the checkout.
    SessionProjectSource {
        session_id: String,
    },
    Status,
    WebViewerAccess,
    /// The daemon host's Git commit author, which its sessions inherit.
    GitIdentity,
    /// Write the daemon host's global Git commit author.
    SetGitIdentity {
        name: String,
        email: String,
    },
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
    /// Take or renew a client's attachment lease.
    Attach {
        client_id: String,
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
    PersistTerminalDraft {
        client_id: String,
        session_id: String,
        owner_pid: u32,
        revision: u64,
        draft: mj_core::storage::DetachedSessionDraft,
    },
    RecordAbandonedPrompt {
        session_id: String,
        bundle_id: String,
        text: String,
    },
    /// Persist the shared sidebar preference and publish it before replying.
    SaveSidebarSize {
        size: mj_core::workspace::PaneSize,
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
        /// A project the import synthesized. The daemon writes it with the
        /// session, so a client never edits the daemon's configuration.
        #[serde(default)]
        bundle: Option<ImportedBundle>,
    },
    SetSessionTitle {
        session_id: String,
        title: String,
    },
    SetSessionWorkspace {
        session_id: String,
        workspace_id: String,
    },
    SetSessionContainerSettings {
        session_id: String,
        cpus: Option<String>,
        memory: Option<String>,
        mounts: Vec<AdditionalMount>,
        mount_history: Vec<PathBuf>,
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
    ProjectCatalog {
        refresh: bool,
        retry: bool,
    },
    CreateProject {
        sources: Vec<String>,
    },
    RemoveProject {
        bundle_id: String,
    },
    WikiSearch {
        query: String,
        limit: usize,
    },
    /// The live sessions whose user or agent messages contain a query,
    /// ignoring case. Tool calls and tool output do not count.
    SessionTextSearch {
        query: String,
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
    /// Everything the terminal dashboard reads from the store at startup.
    DashboardStartup {
        client_id: String,
        workspace_id: String,
    },
    /// Stored pane sizes and conversation layouts of these workspaces.
    WorkspaceLayouts {
        workspace_ids: Vec<String>,
    },
    /// Mount history and remembered container sizes.
    StoredSessionMetadata,
    /// The stored tail of a session's projection, at most `items` transcript
    /// entries, or `None` when the session has no stored projection.
    StoredProjectionTail {
        session_id: String,
        items: usize,
    },
    /// The stored projection summary of one session.
    StoredSessionSummary {
        session_id: String,
    },
    /// Check that a stopped session's checkpointed repositories can be
    /// fetched again before it resumes on `target_id`.
    ResumeRepositoryPreflight {
        session_id: String,
        target_id: String,
    },
    /// Point one of a stopped session's repositories at a new origin, then
    /// check the sources again.
    ReplaceResumeRepositoryOrigin {
        session_id: String,
        repository_id: String,
        replacement: String,
    },
    /// How much session storage an archive age would free.
    ArchiveSpacePreview {
        older_than_days: Option<u32>,
    },
    /// Save the terminal Setup editor's configuration, checked against the
    /// stored sessions it would disturb. Both are configuration JSON.
    SaveSetup {
        original: String,
        updated: String,
    },
    RuntimeChanges {
        cursor: Option<crate::runtime_feed::RuntimeCursor>,
        wait: bool,
        subagent_parents: std::collections::BTreeSet<String>,
    },
    /// One live session's transcript tail as it was at `cursor`, so the
    /// deltas after that cursor apply to it exactly.
    SessionTail {
        session_id: String,
        cursor: crate::runtime_feed::RuntimeCursor,
    },
    /// The sessions the resume dialog lists. The runtime feed carries only
    /// live sessions, so the dialog asks for these when it opens.
    ResumeCandidates,
    /// One session's whole record, live or stopped. The resume wizard reads
    /// it for the row picked in the resume dialog.
    SessionRecord {
        session_id: String,
    },
    /// The session `mj go` opens: the remembered one while it is still
    /// eligible, otherwise the most recently updated eligible session of the
    /// workspace, live or stopped.
    GoStartupSession {
        workspace_id: String,
        last_session_id: Option<String>,
    },
    /// Ask the daemon's quota poller to probe now. The daemon is the only
    /// process that probes; the result arrives in the runtime feed. Added in
    /// protocol 43.
    RefreshQuota,
    /// Discover eligible subagent models in the daemon, which owns the cache.
    SubagentOptions {
        profile: String,
        model: Option<String>,
        /// Setup can probe an unsaved draft without changing live settings.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config: Option<Box<Config>>,
    },
    /// Ensure valid unsaved profile definitions are hydrating. Results arrive
    /// through runtime publications; this request does no discovery itself.
    WarmProfileCapabilities {
        config: Box<Config>,
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
        command_id: String,
        text: String,
        prompt: Vec<ContentBlock>,
        encoded_draft: String,
        #[serde(default)]
        move_operation_id: Option<String>,
        /// The saved draft text this prompt was typed from, if the client
        /// also persisted it. Cleared after a successful submit so the
        /// delivered prompt does not reappear as a draft.
        #[serde(default)]
        inherited_draft: Option<String>,
    },
    /// Take back a prompt queued with `QueueStartupPrompt` that the daemon has
    /// not started delivering, so the person can edit it. The daemon cancels
    /// the prompt with this caller-owned command ID and replies
    /// `PromptWithdrawn(true)`, or `PromptWithdrawn(false)` when none is
    /// waiting because delivery already started.
    WithdrawStartupPrompt {
        session_id: String,
        command_id: String,
    },
    StartupPromptOutcomes {
        session_id: String,
        command_ids: Vec<String>,
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
    /// Review the turn this session just finished, on a surface's request.
    StartTurnReview {
        session_id: String,
    },
    /// Forward, dismiss, or cancel the open review.
    ResolveTurnReview {
        session_id: String,
        resolution: Resolution,
    },
    StartSessionLifecycle(SessionLifecycleRequest),
    WaitSessionLifecycle(LifecycleReceipt),
    SuspendSession {
        session_id: String,
        #[serde(default)]
        acknowledge_unpublished_work: bool,
    },
    /// Explicitly omit untracked nested Git repositories from this suspension.
    IgnoreNestedRepositoriesAndSuspend {
        session_id: String,
    },
    RestartSession {
        session_id: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        acknowledge_container_recreation: bool,
    },
    PreviewContainerUpgrade {
        session_id: String,
    },
    UpgradeContainer(mj_core::state::ContainerUpgradeRequest),
    StartCreateSession(CreateSessionRequest),
    WaitCreateSession {
        session_id: String,
    },
    ResumeSession(ResumeSessionRequest),
    PrepareMoveSession(MoveSelection),
    MoveSession(MoveSessionRequest),
    MoveSources {
        session_id: String,
        cleanup_operation_id: Option<String>,
    },
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
    CancelLifecycle {
        session_id: String,
    },
    CancelContainerUpgrade {
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

/// The daemon answered a request with an error. Unlike a lost connection,
/// this is a complete round trip: the daemon read the request and said why it
/// did not carry it out.
#[derive(Debug)]
pub struct DaemonRefusal(pub String);

impl std::fmt::Display for DaemonRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DaemonRefusal {}

/// An existing startup ID belongs to another immutable request. Its original
/// work may already be accepted, so this never proves that work was refused.
#[derive(Debug)]
pub struct StartupPromptIdentityConflict;

impl std::fmt::Display for StartupPromptIdentityConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("startup command ID was reused with a different payload")
    }
}

impl std::error::Error for StartupPromptIdentityConflict {}

impl DaemonRefusal {
    /// Whether the daemon itself could not confirm delivery. Its message is
    /// then led by `session::DeliveryUnconfirmed`.
    #[must_use]
    pub fn delivery_unconfirmed(&self) -> bool {
        self.0
            .starts_with(&crate::session::DeliveryUnconfirmed.to_string())
    }
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
    BuildCachePreview(Option<mj_core::state::BuildCachePreview>, bool),
    MbxInstalled(String),
    CheckpointArchiveSizes(
        std::collections::BTreeMap<String, std::result::Result<Option<u64>, String>>,
    ),
    SetupDiscovery(Box<crate::discovery::SetupDetection>),
    Availability(
        #[serde(deserialize_with = "deserialize_availability")]
        crate::availability::AvailabilityChecked,
    ),
    ProfileLoginStatuses(std::collections::BTreeMap<String, mj_core::credentials::LoginStatus>),
    NewSessionPreflight(PreflightNew),
    PathCompletion(mj_core::path_completion::PathCompletion),
    ProjectDirectories(Vec<(std::path::PathBuf, bool)>),
    ProjectDiscovery(mj_core::project_picker::ProjectDiscovery),
    ResolvedPath(std::path::PathBuf),
    MountValidation(std::path::PathBuf, Option<String>),
    SessionMountValidation(Option<(String, String)>),
    LifecycleAccepted(LifecycleReceipt),
    /// Only a captured operation result may report this failure.
    LifecycleFailed(String),
    LifecycleObservationUnavailable(LifecycleReceipt),
    NativeAgentHistory(mj_core::native_agent::NativeAgentHistoryPage),
    Pong,
    UpgradePending,
    /// Labels for the work holding an automatic handoff open, empty when
    /// nothing is. Answers `DaemonAction::UpgradeBlockers`.
    UpgradeBlockers(Vec<String>),
    Identity(DaemonIdentity),
    Configuration(Box<Config>),
    FirstRunSetup(Option<FirstRunSetupReport>),
    DoctorErrors(Vec<String>),
    RemoteRepairs(Vec<mj_core::local_git::LocalRemoteRepair>),
    ProjectSource(mj_core::state::ProjectSourceIdentity),
    Status(DaemonStatus),
    WebViewerAccess(crate::web::WebViewerAccess),
    GitIdentity(GitIdentity),
    WebListeners(Vec<crate::web::WebListenerProcess>),
    SubagentOptions(mj_core::subagent::SubagentOptions),
    Workspaces(Vec<WorkspaceListing>),
    Workspace(WorkspaceRecord),
    Snapshot(WorkspaceSnapshot),
    DashboardStartup(Box<DashboardStartup>),
    WorkspaceLayouts(WorkspaceLayouts),
    StoredSessionMetadata(StoredSessionMetadata),
    StoredProjectionTail(
        #[serde(serialize_with = "crate::runtime_feed::serialize_client_projection")]
        Option<Box<MaterializedSession>>,
    ),
    StoredSessionSummary(Option<Box<MaterializedSessionSummary>>),
    SavedSetup(Box<Config>),
    ArchiveSpacePreview(ArchiveSpacePreview),
    ResumeRepositoryPreflight(Box<ResumeRepositorySourcePreflight>),
    RuntimeChanges(Box<crate::runtime_feed::RuntimeFrame>),
    SessionTail(Box<crate::runtime_feed::SessionTailReply>),
    ResumeCandidates(Box<ResumeCandidates>),
    GoStartupSession(Option<Box<SessionRecord>>),
    SessionRecord(Option<Box<SessionRecord>>),
    /// Transport fragments of one chunked reply (see
    /// [`DaemonReply::is_chunked`]); never exposed to consumers.
    ReplyChunk {
        #[serde(with = "chunk_bytes")]
        bytes: Vec<u8>,
        finished: bool,
    },
    RegisteredSession(Box<RegisteredSession>),
    ContainerUpgradePreview(mj_core::state::ContainerUpgradePreview),
    MovePreparation(Box<MovePreparation>),
    MoveOutcome(MoveOutcome),
    MoveSources(Vec<mj_core::move_workspace::RetainedMoveSource>),
    Ordinal(u64),
    Text(String),
    OptionalSessionState(Option<SessionState>),
    Checkpoint(mj_core::state::CheckpointMetadata),
    RecoveryScan(mj_core::state::RecoveryScan),
    WikiRows(WikiSearchPage),
    ProjectCatalog(mj_core::project_catalog::ProjectCatalogView),
    ProjectCreated(mj_core::project_catalog::SavedProject),
    SessionTextMatches(Vec<SessionTextMatch>),
    WikiHits(Option<WikiHitTranscript>),
    WikiSession(Option<Box<WikiSessionInfo>>),
    /// Whether a queued startup prompt was withdrawn before delivery.
    PromptWithdrawn(bool),
    StartupPromptIdentityConflict,
    StartupPromptOutcomes(std::collections::BTreeMap<String, StartupPromptOutcome>),
    Done,
}

impl DaemonReply {
    /// Replies that can outgrow one frame. The daemon sends them as
    /// [`DaemonReply::ReplyChunk`] fragments and the client reassembles them.
    pub fn is_chunked(&self) -> bool {
        matches!(
            self,
            Self::Configuration(_)
                | Self::DashboardStartup(_)
                | Self::RuntimeChanges(_)
                | Self::SessionTail(_)
                | Self::ResumeCandidates(_)
        )
    }
}

/// What the resume dialog lists, read on demand.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeCandidates {
    /// One row for every inactive session that is not a sub-agent.
    pub candidates: Vec<ResumeCandidate>,
    /// Durable moves of those sessions, for "move needs recovery" marks.
    pub moves: Vec<MoveOperation>,
    /// Native sessions that some record, live or not, already holds. The
    /// import list must not offer them again.
    pub adopted_native_sessions: Vec<(mj_core::config::HarnessKind, String)>,
    /// Local checkouts that Mjolnir created for its sessions. Every native
    /// thread that ran inside one belongs to that session.
    pub local_checkout_roots: Vec<PathBuf>,
}

/// One row of the resume dialog's Mjolnir tab: what the row shows, without
/// the rest of the record. A session's title can be tens of kilobytes, and
/// most of a store's history is never resumed, so the resume wizard fetches
/// the whole record (`DaemonAction::SessionRecord`) only for the row picked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeCandidate {
    pub session_id: String,
    pub state: SessionState,
    pub last_profile: String,
    /// The listed title, cut to [`ResumeCandidate::TITLE_CHARS`].
    pub title: String,
    /// Where the session ran, as the session list names it.
    pub origin: String,
    pub project: String,
    pub has_checkpoint: bool,
    /// The checkpoint's creation time, or the record's last update without
    /// one; `None` when neither parses.
    pub last_activity_ms: Option<i64>,
    pub publication: Option<PublicationState>,
    /// The checkpoint archive whose size the row shows. Only a stopped
    /// record is described by its checkpoint.
    pub checkpoint_archive: Option<PathBuf>,
    /// Whether destroying the session can also delete its worktree branch.
    pub worktree_checkout: bool,
}

impl ResumeCandidate {
    /// Longer than any row draws it.
    pub const TITLE_CHARS: usize = 200;

    /// The row for `record`. The daemon and the dashboard both build rows
    /// here, so a row reads the same whichever of them held the record.
    pub fn of(record: &SessionRecord, config: &Config) -> Self {
        let timestamp_ms = |timestamp: &str| {
            chrono::DateTime::parse_from_rfc3339(timestamp)
                .ok()
                .map(|parsed| parsed.timestamp_millis())
        };
        Self {
            session_id: record.id.clone(),
            state: record.state,
            last_profile: record.last_profile.clone(),
            title: record
                .listed_title()
                .chars()
                .take(Self::TITLE_CHARS)
                .collect(),
            origin: record.project_target(config, &record.target_template_id),
            project: record.project_name(config),
            has_checkpoint: record.checkpoint.is_some(),
            last_activity_ms: record
                .checkpoint
                .as_ref()
                .and_then(|checkpoint| timestamp_ms(&checkpoint.created_at))
                .or_else(|| timestamp_ms(&record.updated_at)),
            publication: record.publication_state(),
            checkpoint_archive: record
                .checkpoint
                .as_ref()
                .filter(|_| record.state == SessionState::Stopped)
                .map(|checkpoint| checkpoint.archive_path.clone()),
            worktree_checkout: record
                .checkout()
                .managed_worktree()
                .is_some_and(|owned| owned.kind == ManagedCheckoutKind::Worktree),
        }
    }
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
///
/// On Linux this reads the one `/proc/<pid>/stat` file. `sysinfo` scans every
/// process on the machine for a single-PID refresh, so it is not used here:
/// this probe runs every 250 ms while an upgrade waits on an older daemon.
#[cfg(target_os = "linux")]
pub fn process_is_zombie(pid: u32) -> bool {
    let Ok(stat) = std::fs::read(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // The command name in parentheses may itself contain ") ", so the state
    // is the first field after the last closing parenthesis.
    stat.iter()
        .rposition(|byte| *byte == b')')
        .and_then(|end| stat.get(end + 2))
        .is_some_and(|state| *state == b'Z')
}

/// On macOS this asks the kernel for the one process's BSD info. `sysinfo`
/// cannot be used: its lookups skip zombies, so it reports an exited, unreaped
/// process as absent rather than as a zombie, and `kill(pid, 0)` then calls it
/// alive forever. A nonzero `arg` to `PROC_PIDTBSDINFO` makes the kernel
/// include zombies.
#[cfg(target_os = "macos")]
pub fn process_is_zombie(pid: u32) -> bool {
    let Ok(raw_pid) = libc::c_int::try_from(pid) else {
        return false;
    };
    let size = std::mem::size_of::<libc::proc_bsdinfo>();
    // SAFETY: proc_bsdinfo is plain old data, so all-zero is a valid value.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    // SAFETY: the buffer is a writable proc_bsdinfo of exactly `size` bytes.
    let written = unsafe {
        libc::proc_pidinfo(
            raw_pid,
            libc::PROC_PIDTBSDINFO,
            1,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size as libc::c_int,
        )
    };
    usize::try_from(written).is_ok_and(|written| written == size) && info.pbi_status == libc::SZOMB
}

#[cfg(all(unix, not(target_os = "linux"), not(target_os = "macos")))]
pub fn process_is_zombie(pid: u32) -> bool {
    let pid = sysinfo::Pid::from_u32(pid);
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    system
        .process(pid)
        .is_some_and(|process| process.status() == sysinfo::ProcessStatus::Zombie)
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

/// No daemon has published its endpoint: `daemon.json` does not exist.
///
/// This is the ordinary stopped state, not a failure. A daemon removes the
/// file when it stops, so callers show "not running" rather than the raw
/// file error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonNotRunning {
    /// Where the endpoint would be, which also names the instance.
    pub metadata_path: std::path::PathBuf,
}

/// What a caller shows for [`DaemonNotRunning`].
pub const DAEMON_NOT_RUNNING_MESSAGE: &str = "the Mjolnir daemon is not running";

impl std::fmt::Display for DaemonNotRunning {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(DAEMON_NOT_RUNNING_MESSAGE)
    }
}

impl std::error::Error for DaemonNotRunning {}

/// The stopped state, if `error` reports it anywhere in its chain.
pub fn daemon_not_running(error: &anyhow::Error) -> Option<&DaemonNotRunning> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<DaemonNotRunning>())
}

pub fn read_metadata_any() -> Result<DaemonMetadata> {
    read_metadata_at(&metadata_path())
}

fn read_metadata_at(path: &std::path::Path) -> Result<DaemonMetadata> {
    let body = match fs::read(path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(DaemonNotRunning {
                metadata_path: path.to_owned(),
            }
            .into());
        }
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
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

/// Read one reply, reassembling a chunked one before exposing it. All
/// fragments belong to the same response; disconnects discard partial state.
pub async fn read_response(stream: &mut TcpStream) -> Result<ResponseEnvelope> {
    let mut response: ResponseEnvelope = read_frame(stream).await?;
    if !matches!(response.result, Ok(DaemonReply::ReplyChunk { .. })) {
        return Ok(response);
    }
    let protocol_version = response.protocol_version;
    let request_id = response.request_id;
    let mut body = Vec::new();
    loop {
        ensure!(
            response.protocol_version == protocol_version && response.request_id == request_id,
            "daemon crossed reply fragment identities"
        );
        let Ok(DaemonReply::ReplyChunk { bytes, finished }) = response.result else {
            bail!("daemon interrupted a chunked reply");
        };
        ensure!(!bytes.is_empty(), "empty reply fragment");
        body.extend(bytes);
        if finished {
            break;
        }
        response = read_frame(stream).await?;
    }
    let reply: DaemonReply = tokio::task::spawn_blocking(move || serde_json::from_slice(&body))
        .await
        .context("reply decoder task failed")??;
    ensure!(
        reply.is_chunked(),
        "daemon chunked a reply that is never chunked"
    );
    Ok(ResponseEnvelope {
        protocol_version,
        request_id,
        result: Ok(reply),
    })
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
        let response = read_response(&mut self.stream).await?;
        ensure!(
            response.protocol_version == protocol_version,
            "daemon changed protocol"
        );
        ensure!(
            response.request_id == request_id,
            "daemon crossed request IDs"
        );
        response
            .result
            .map_err(|message| anyhow::Error::new(DaemonRefusal(message)))
    }

    pub async fn identity(&mut self) -> Result<DaemonIdentity> {
        match self.request(DaemonAction::Identity).await? {
            DaemonReply::Identity(identity) => Ok(identity),
            reply => bail!("unexpected daemon identity reply {reply:?}"),
        }
    }

    /// The configuration the daemon serves. Clients use this instead of
    /// opening the daemon's `config.toml`.
    pub async fn config(&mut self) -> Result<Config> {
        match self.request(DaemonAction::Configuration).await? {
            DaemonReply::Configuration(config) => Ok(*config),
            reply => bail!("unexpected daemon configuration reply {reply:?}"),
        }
    }

    pub async fn run_first_run_setup(&mut self) -> Result<Option<FirstRunSetupReport>> {
        match self.request(DaemonAction::RunFirstRunSetup).await? {
            DaemonReply::FirstRunSetup(report) => Ok(report),
            reply => bail!("unexpected first-run setup reply {reply:?}"),
        }
    }

    pub async fn run_doctor(&mut self) -> Result<Vec<String>> {
        match self.request(DaemonAction::RunDoctor).await? {
            DaemonReply::DoctorErrors(errors) => Ok(errors),
            reply => bail!("unexpected doctor reply {reply:?}"),
        }
    }

    pub async fn new_session_remote_repairs(
        &mut self,
        bundle_id: String,
        target_id: String,
    ) -> Result<Vec<mj_core::local_git::LocalRemoteRepair>> {
        match self
            .request(DaemonAction::NewSessionRemoteRepairs {
                bundle_id,
                target_id,
            })
            .await?
        {
            DaemonReply::RemoteRepairs(repairs) => Ok(repairs),
            reply => bail!("unexpected new-session preflight reply {reply:?}"),
        }
    }

    pub async fn repair_repository_remotes(
        &mut self,
        bundle_id: String,
        repairs: Vec<mj_core::local_git::LocalRemoteRepair>,
    ) -> Result<()> {
        match self
            .request(DaemonAction::RepairRepositoryRemotes { bundle_id, repairs })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected remote-repair reply {reply:?}"),
        }
    }

    pub async fn session_project_source(
        &mut self,
        session_id: String,
    ) -> Result<mj_core::state::ProjectSourceIdentity> {
        match self
            .request(DaemonAction::SessionProjectSource { session_id })
            .await?
        {
            DaemonReply::ProjectSource(source) => Ok(source),
            reply => bail!("unexpected project-source reply {reply:?}"),
        }
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

    pub async fn subagent_options(
        &mut self,
        profile: String,
        model: Option<String>,
        config: Option<Config>,
    ) -> Result<mj_core::subagent::SubagentOptions> {
        match self
            .request(DaemonAction::SubagentOptions {
                profile,
                model,
                config: config.map(Box::new),
            })
            .await?
        {
            DaemonReply::SubagentOptions(options) => Ok(options),
            reply => bail!("unexpected subagent options reply {reply:?}"),
        }
    }

    pub async fn warm_profile_capabilities(&mut self, config: Config) -> Result<()> {
        match self
            .request(DaemonAction::WarmProfileCapabilities {
                config: Box::new(config),
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected capability hydration reply {reply:?}"),
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

    /// Ask the daemon to probe every profile that is not on hold. Returns when
    /// the daemon has accepted the request, not when the probes finish.
    pub async fn refresh_quota(&mut self) -> Result<()> {
        match self.request(DaemonAction::RefreshQuota).await? {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected refresh-quota reply {reply:?}"),
        }
    }

    pub async fn git_identity(&mut self) -> Result<GitIdentity> {
        match self.request(DaemonAction::GitIdentity).await? {
            DaemonReply::GitIdentity(identity) => Ok(identity),
            reply => bail!("unexpected git-identity reply {reply:?}"),
        }
    }

    pub async fn set_git_identity(&mut self, name: String, email: String) -> Result<GitIdentity> {
        match self
            .request(DaemonAction::SetGitIdentity { name, email })
            .await?
        {
            DaemonReply::GitIdentity(identity) => Ok(identity),
            reply => bail!("unexpected set-git-identity reply {reply:?}"),
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

    pub async fn attach(&mut self, client_id: String) -> Result<()> {
        match self.request(DaemonAction::Attach { client_id }).await? {
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

    pub async fn persist_terminal_draft(
        &mut self,
        client_id: String,
        session_id: String,
        owner_pid: u32,
        revision: u64,
        draft: mj_core::storage::DetachedSessionDraft,
    ) -> Result<()> {
        match self
            .request(DaemonAction::PersistTerminalDraft {
                client_id,
                session_id,
                owner_pid,
                revision,
                draft,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected terminal-draft reply {reply:?}"),
        }
    }

    /// Persist a prompt the user abandoned in the chat composer.
    pub async fn record_abandoned_prompt(
        &mut self,
        session_id: String,
        bundle_id: String,
        text: String,
    ) -> Result<()> {
        match self
            .request(DaemonAction::RecordAbandonedPrompt {
                session_id,
                bundle_id,
                text,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected abandoned-prompt reply {reply:?}"),
        }
    }

    pub async fn save_sidebar_size(&mut self, size: mj_core::workspace::PaneSize) -> Result<()> {
        match self.request(DaemonAction::SaveSidebarSize { size }).await? {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected sidebar-size save reply {reply:?}"),
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

    pub async fn persist_imported_session(
        &mut self,
        session: SessionRecord,
        bundle: Option<ImportedBundle>,
    ) -> Result<()> {
        match self
            .request(DaemonAction::PersistImportedSession {
                session: Box::new(session),
                bundle,
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

    pub async fn set_session_workspace(
        &mut self,
        session_id: String,
        workspace_id: String,
    ) -> Result<()> {
        match self
            .request(DaemonAction::SetSessionWorkspace {
                session_id,
                workspace_id,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected session-workspace reply {reply:?}"),
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
    pub async fn project_catalog(
        &mut self,
        refresh: bool,
        retry: bool,
    ) -> Result<mj_core::project_catalog::ProjectCatalogView> {
        match self
            .request(DaemonAction::ProjectCatalog { refresh, retry })
            .await?
        {
            DaemonReply::ProjectCatalog(view) => Ok(view),
            other => bail!("unexpected project catalog reply: {other:?}"),
        }
    }

    pub async fn create_project(
        &mut self,
        sources: Vec<String>,
    ) -> Result<mj_core::project_catalog::SavedProject> {
        match self
            .request(DaemonAction::CreateProject { sources })
            .await?
        {
            DaemonReply::ProjectCreated(project) => Ok(project),
            other => bail!("unexpected project creation reply: {other:?}"),
        }
    }

    pub async fn remove_project(&mut self, bundle_id: String) -> Result<()> {
        match self
            .request(DaemonAction::RemoveProject { bundle_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            other => bail!("unexpected project removal reply: {other:?}"),
        }
    }

    pub async fn wiki_search(&mut self, query: String, limit: usize) -> Result<WikiSearchPage> {
        match self
            .request(DaemonAction::WikiSearch { query, limit })
            .await?
        {
            DaemonReply::WikiRows(page) => Ok(page),
            reply => bail!("unexpected SessionWiki search reply {reply:?}"),
        }
    }

    /// The live sessions whose user or agent messages contain `query`, and
    /// which of the two matched. Answers from the SessionWiki index, so a very
    /// new message can be missing until the next sync.
    pub async fn session_text_search(&mut self, query: String) -> Result<Vec<SessionTextMatch>> {
        match self
            .request(DaemonAction::SessionTextSearch { query })
            .await?
        {
            DaemonReply::SessionTextMatches(matches) => Ok(matches),
            reply => bail!("unexpected session text search reply {reply:?}"),
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

    pub async fn dashboard_startup(
        &mut self,
        client_id: String,
        workspace_id: String,
    ) -> Result<DashboardStartup> {
        match self
            .request(DaemonAction::DashboardStartup {
                client_id,
                workspace_id,
            })
            .await?
        {
            DaemonReply::DashboardStartup(startup) => Ok(*startup),
            reply => bail!("unexpected dashboard startup reply {reply:?}"),
        }
    }

    pub async fn workspace_layouts(
        &mut self,
        workspace_ids: Vec<String>,
    ) -> Result<WorkspaceLayouts> {
        match self
            .request(DaemonAction::WorkspaceLayouts { workspace_ids })
            .await?
        {
            DaemonReply::WorkspaceLayouts(layouts) => Ok(layouts),
            reply => bail!("unexpected workspace layouts reply {reply:?}"),
        }
    }

    pub async fn stored_session_metadata(&mut self) -> Result<StoredSessionMetadata> {
        match self.request(DaemonAction::StoredSessionMetadata).await? {
            DaemonReply::StoredSessionMetadata(metadata) => Ok(metadata),
            reply => bail!("unexpected stored session metadata reply {reply:?}"),
        }
    }

    pub async fn stored_projection_tail(
        &mut self,
        session_id: String,
        items: usize,
    ) -> Result<Option<MaterializedSession>> {
        match self
            .request(DaemonAction::StoredProjectionTail { session_id, items })
            .await?
        {
            DaemonReply::StoredProjectionTail(tail) => Ok(tail.map(|tail| *tail)),
            reply => bail!("unexpected stored projection tail reply {reply:?}"),
        }
    }

    pub async fn stored_session_summary(
        &mut self,
        session_id: String,
    ) -> Result<Option<MaterializedSessionSummary>> {
        match self
            .request(DaemonAction::StoredSessionSummary { session_id })
            .await?
        {
            DaemonReply::StoredSessionSummary(summary) => Ok(summary.map(|summary| *summary)),
            reply => bail!("unexpected stored session summary reply {reply:?}"),
        }
    }

    pub async fn resume_repository_preflight(
        &mut self,
        session_id: String,
        target_id: String,
    ) -> Result<ResumeRepositorySourcePreflight> {
        match self
            .request(DaemonAction::ResumeRepositoryPreflight {
                session_id,
                target_id,
            })
            .await?
        {
            DaemonReply::ResumeRepositoryPreflight(preflight) => Ok(*preflight),
            reply => bail!("unexpected resume repository preflight reply {reply:?}"),
        }
    }

    pub async fn replace_resume_repository_origin(
        &mut self,
        session_id: String,
        repository_id: String,
        replacement: String,
    ) -> Result<ResumeRepositorySourcePreflight> {
        match self
            .request(DaemonAction::ReplaceResumeRepositoryOrigin {
                session_id,
                repository_id,
                replacement,
            })
            .await?
        {
            DaemonReply::ResumeRepositoryPreflight(preflight) => Ok(*preflight),
            reply => bail!("unexpected repository origin replacement reply {reply:?}"),
        }
    }

    pub async fn archive_space_preview(
        &mut self,
        older_than_days: Option<u32>,
    ) -> Result<ArchiveSpacePreview> {
        match self
            .request(DaemonAction::ArchiveSpacePreview { older_than_days })
            .await?
        {
            DaemonReply::ArchiveSpacePreview(preview) => Ok(preview),
            reply => bail!("unexpected archive space preview reply {reply:?}"),
        }
    }

    pub async fn save_setup(&mut self, original: String, updated: String) -> Result<Config> {
        match self
            .request(DaemonAction::SaveSetup { original, updated })
            .await?
        {
            DaemonReply::SavedSetup(config) => Ok(*config),
            reply => bail!("unexpected setup save reply {reply:?}"),
        }
    }

    pub async fn runtime_changes(
        &mut self,
        cursor: Option<crate::runtime_feed::RuntimeCursor>,
        wait: bool,
    ) -> Result<crate::runtime_feed::RuntimeFrame> {
        self.runtime_changes_for_subagent_parents(cursor, wait, Default::default())
            .await
    }

    pub async fn runtime_changes_for_subagent_parents(
        &mut self,
        cursor: Option<crate::runtime_feed::RuntimeCursor>,
        wait: bool,
        subagent_parents: std::collections::BTreeSet<String>,
    ) -> Result<crate::runtime_feed::RuntimeFrame> {
        match self
            .request(DaemonAction::RuntimeChanges {
                cursor,
                wait,
                subagent_parents,
            })
            .await?
        {
            DaemonReply::RuntimeChanges(frame) => Ok(*frame),
            reply => bail!("unexpected runtime changes reply {reply:?}"),
        }
    }

    pub async fn session_tail(
        &mut self,
        session_id: String,
        cursor: crate::runtime_feed::RuntimeCursor,
    ) -> Result<crate::runtime_feed::SessionTailReply> {
        match self
            .request(DaemonAction::SessionTail { session_id, cursor })
            .await?
        {
            DaemonReply::SessionTail(reply) => Ok(*reply),
            reply => bail!("unexpected session tail reply {reply:?}"),
        }
    }

    pub async fn resume_candidates(&mut self) -> Result<ResumeCandidates> {
        match self.request(DaemonAction::ResumeCandidates).await? {
            DaemonReply::ResumeCandidates(candidates) => Ok(*candidates),
            reply => bail!("unexpected resume candidates reply {reply:?}"),
        }
    }

    pub async fn session_record(&mut self, session_id: String) -> Result<Option<SessionRecord>> {
        match self
            .request(DaemonAction::SessionRecord { session_id })
            .await?
        {
            DaemonReply::SessionRecord(record) => Ok(record.map(|record| *record)),
            reply => bail!("unexpected session record reply {reply:?}"),
        }
    }

    pub async fn go_startup_session(
        &mut self,
        workspace_id: String,
        last_session_id: Option<String>,
    ) -> Result<Option<SessionRecord>> {
        match self
            .request(DaemonAction::GoStartupSession {
                workspace_id,
                last_session_id,
            })
            .await?
        {
            DaemonReply::GoStartupSession(record) => Ok(record.map(|record| *record)),
            reply => bail!("unexpected go startup session reply {reply:?}"),
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
    #[allow(clippy::too_many_arguments)] // Mirrors the structured durable startup action.
    pub async fn queue_startup_prompt(
        &mut self,
        session_id: String,
        command_id: String,
        text: String,
        prompt: Vec<ContentBlock>,
        inherited_draft: Option<String>,
        encoded_draft: String,
        move_operation_id: Option<String>,
    ) -> Result<()> {
        match self
            .request(DaemonAction::QueueStartupPrompt {
                session_id,
                command_id,
                text,
                prompt,
                encoded_draft,
                move_operation_id,
                inherited_draft,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            DaemonReply::StartupPromptIdentityConflict => Err(StartupPromptIdentityConflict.into()),
            reply => bail!("unexpected startup prompt reply {reply:?}"),
        }
    }

    /// Take a prompt back from the daemon's startup queue before delivery.
    /// `Ok(true)` means it is withdrawn and will not be sent; `Ok(false)`
    /// means delivery had already started, so it was sent. An error means the
    /// daemon could not be asked, and the prompt may still be queued.
    pub async fn withdraw_startup_prompt(
        &mut self,
        session_id: String,
        command_id: String,
    ) -> Result<bool> {
        match self
            .request(DaemonAction::WithdrawStartupPrompt {
                session_id,
                command_id,
            })
            .await?
        {
            DaemonReply::PromptWithdrawn(withdrawn) => Ok(withdrawn),
            reply => bail!("unexpected startup prompt withdrawal reply {reply:?}"),
        }
    }

    /// Read only the requested durable terminal receipts (at most 128 IDs).
    pub async fn startup_prompt_outcomes(
        &mut self,
        session_id: String,
        command_ids: Vec<String>,
    ) -> Result<std::collections::BTreeMap<String, StartupPromptOutcome>> {
        match self
            .request(DaemonAction::StartupPromptOutcomes {
                session_id,
                command_ids,
            })
            .await?
        {
            DaemonReply::StartupPromptOutcomes(outcomes) => Ok(outcomes),
            reply => bail!("unexpected startup prompt outcomes reply {reply:?}"),
        }
    }

    /// Ask the daemon to review committed work through the push gate.
    ///
    /// The refusal includes how to proceed, such as committing tracked
    /// changes or choosing a harness with push-gate support. It travels as text rather than
    /// as a code every surface would have to translate.
    pub async fn start_turn_review(&mut self, session_id: String) -> Result<()> {
        match self
            .request(DaemonAction::StartTurnReview { session_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected review reply {reply:?}"),
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

    /// Submit once and retain the receipt; a lost reply does not permit replay.
    pub async fn start_session_lifecycle(
        &mut self,
        request: SessionLifecycleRequest,
    ) -> Result<LifecycleReceipt> {
        let session_id = request.session_id().to_owned();
        let reply = self
            .request(DaemonAction::StartSessionLifecycle(request))
            .await
            .map_err(|error| {
                if error.is::<DaemonRefusal>() {
                    error
                } else {
                    anyhow::Error::new(LifecycleAdmissionUnconfirmed(session_id.clone()))
                        .context(error)
                }
            })?;
        match reply {
            DaemonReply::LifecycleAccepted(receipt) => Ok(receipt),
            reply => Err(
                anyhow::Error::new(LifecycleAdmissionUnconfirmed(session_id))
                    .context(format!("unexpected lifecycle acceptance reply {reply:?}")),
            ),
        }
    }

    /// Observe exactly this operation. Dropping the observer never cancels it.
    pub async fn wait_session_lifecycle(
        &mut self,
        receipt: LifecycleReceipt,
    ) -> Result<DaemonReply> {
        let reply = self
            .request(DaemonAction::WaitSessionLifecycle(receipt.clone()))
            .await
            .map_err(|error| {
                anyhow::Error::new(LifecycleObservationUnavailable(receipt.clone())).context(error)
            })?;
        match reply {
            reply @ (DaemonReply::Done | DaemonReply::MoveOutcome(_)) => Ok(reply),
            DaemonReply::LifecycleFailed(detail) => Err(DaemonRefusal(detail).into()),
            DaemonReply::LifecycleObservationUnavailable(receipt) => {
                Err(LifecycleObservationUnavailable(receipt).into())
            }
            reply => Err(anyhow::Error::new(LifecycleObservationUnavailable(receipt))
                .context(format!("unexpected lifecycle completion reply {reply:?}"))),
        }
    }

    pub async fn suspend_session(&mut self, session_id: String) -> Result<()> {
        self.suspend_session_with_ack(session_id, false).await
    }

    pub async fn ignore_nested_repositories_and_suspend(
        &mut self,
        session_id: String,
    ) -> Result<()> {
        match self
            .request(DaemonAction::IgnoreNestedRepositoriesAndSuspend { session_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected ignore-and-suspend reply {reply:?}"),
        }
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

    pub async fn restart_session(&mut self, session_id: String) -> Result<()> {
        self.restart_session_with_ack(session_id, false).await
    }

    pub async fn restart_session_with_ack(
        &mut self,
        session_id: String,
        acknowledge_container_recreation: bool,
    ) -> Result<()> {
        ensure!(
            self.metadata.protocol_version == PROTOCOL_VERSION,
            "cannot Restart a session: daemon protocol {} does not match client protocol {}; restart the daemon, then try again",
            self.metadata.protocol_version,
            PROTOCOL_VERSION
        );
        match self
            .request(DaemonAction::RestartSession {
                session_id,
                acknowledge_container_recreation,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected restart-session reply {reply:?}"),
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

    pub async fn cancel_lifecycle(&mut self, session_id: String) -> Result<()> {
        match self
            .request(DaemonAction::CancelLifecycle { session_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected cancel-lifecycle reply {reply:?}"),
        }
    }

    pub async fn cancel_container_upgrade(&mut self, session_id: String) -> Result<()> {
        match self
            .request(DaemonAction::CancelContainerUpgrade { session_id })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected container-upgrade cancellation reply {reply:?}"),
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

    /// The endpoint this management client dialed.
    pub fn metadata(&self) -> &DaemonMetadata {
        &self.inner.metadata
    }

    pub async fn identity(&mut self) -> Result<DaemonIdentity> {
        self.inner.identity().await
    }

    pub async fn status(&mut self) -> Result<DaemonStatus> {
        self.inner.status().await
    }

    pub async fn stop(&mut self) -> Result<()> {
        tokio::time::timeout(STOP_TIMEOUT, self.inner.stop())
            .await
            .context("Mjolnir daemon did not acknowledge the stop before the deadline")?
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
// Plan-only review persistence actions were removed.
// Settings can warm draft profile capabilities through the daemon-owned catalog.
// Runtime deltas carry transcript item changes, and a client fetches a
// session's tail from the daemon at its cursor instead of reading SQLite.
// Lifecycle stages can name the harness preparation step a worker reports.
// A pending form can say it is a tool permission and whether it offers Skip.
// A usage report cost is a Result whose Err means the cost is unknown.
// Creation and runtime metadata carry interactive launch-choice provenance.
// New-session creation independently controls whether its container size
// becomes the host's saved default.
// Operator-away action and runtime metadata.
// Structured startup prompts, caller-owned command IDs, and ID withdrawal.
// Terminals no longer save harness titles; the daemon alone projects them.
// Human interface activity can reset the daemon's automatic AFK clock.
// Project removal runs through the daemon-owned database writer.
// Per-profile remembered subagent preferences change runtime metadata shape.
// Explicit Ignore and Suspend preserves checkpoint verification while omitting nested repositories.
// Web viewer access includes the bound local connection address.
// Review role rows carry their child session IDs; older strict clients reject them.
// Dashboards read stored state through the daemon instead of opening the store,
// and client attachments are renewable leases rather than client process IDs.
// Nodes connect with NodeConnect and then speak the node protocol.
// Dashboard startup uses chunked replies, which older clients reject for this reply type.
// Shared state publishes child metadata; launch intent remains in the daemon store.
// Startup seeds runtime cursors with family scope; reply chunks use base64 bytes.
// Clients read and set the daemon host's Git commit author.
// The daemon reports its per-start identity, build and placement instead of
// clients reading its PID and `/proc`, and it serves the configuration that
// owns the store instead of clients opening `config.toml`.
// SessionReview::Inherit adds a value older API clients cannot deserialize.
// Client views no longer decode forwarding phases or turn-review resolutions;
// reviewer capture actions no longer carry turn scope or baselines.
// RuntimeSessionView carries policy-owned config keys; older strict readers reject them.
// Terminal draft persistence carries per-client revisions and clear tombstones.
// The host node is gone: NodeConnect is no longer an action.
// Lifecycle submission acknowledges ownership separately from completion.
pub const PROTOCOL_VERSION: u32 = 88;
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
/// How long a requested stop may take: the daemon waits up to 60 seconds for
/// session destroys in flight (#1191) before it winds down.
pub const STOP_DRAIN_TIMEOUT: Duration = Duration::from_secs(90);
pub const RETRY_DELAY: Duration = Duration::from_millis(40);
impl DaemonClient {
    pub async fn preview_container_upgrade(
        &mut self,
        session_id: String,
    ) -> Result<mj_core::state::ContainerUpgradePreview> {
        match self
            .request(DaemonAction::PreviewContainerUpgrade { session_id })
            .await?
        {
            DaemonReply::ContainerUpgradePreview(preview) => Ok(preview),
            reply => bail!("unexpected container upgrade preview reply: {reply:?}"),
        }
    }

    pub async fn upgrade_container(
        &mut self,
        request: mj_core::state::ContainerUpgradeRequest,
    ) -> Result<()> {
        match self
            .request(DaemonAction::UpgradeContainer(request))
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected container upgrade reply: {reply:?}"),
        }
    }

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

    pub async fn move_sources(
        &mut self,
        session_id: String,
        cleanup_operation_id: Option<String>,
    ) -> Result<Vec<mj_core::move_workspace::RetainedMoveSource>> {
        match self
            .request(DaemonAction::MoveSources {
                session_id,
                cleanup_operation_id,
            })
            .await?
        {
            DaemonReply::MoveSources(sources) => Ok(sources),
            reply => bail!("unexpected Move sources reply: {reply:?}"),
        }
    }

    pub async fn move_session(&mut self, request: MoveSessionRequest) -> Result<MoveOutcome> {
        match self.request(DaemonAction::MoveSession(request)).await? {
            DaemonReply::MoveOutcome(outcome) => Ok(outcome),
            _ => bail!("daemon returned an unexpected move reply"),
        }
    }
}

impl DaemonAction {
    /// Disposable machine work uses dedicated connections. Closing one or
    /// handing off the daemon cancels it; interrupted installs can be rerun.
    pub fn is_cancellable_machine_action(&self) -> bool {
        if let Self::PreflightNewSession { request } = self
            && !request.remote_repairs.is_empty()
        {
            return false;
        }
        matches!(
            self,
            Self::InstallMbx { .. }
                | Self::PreviewBuildCache { .. }
                | Self::CheckpointArchiveSizes { .. }
                | Self::TestTarget { .. }
                | Self::ProfileLoginStatuses
                | Self::CheckAvailability
                | Self::DiscoverSetup { .. }
                | Self::PreflightNewSession { .. }
                | Self::ValidateProjectDirectory { .. }
                | Self::CompletePath { .. }
                | Self::CheckProjectDirectories { .. }
                | Self::DiscoverProjects { .. }
                | Self::ResolveContainerPath { .. }
                | Self::ResolveSetupPath { .. }
                | Self::ValidateMountSource { .. }
                | Self::ValidateSessionMounts { .. }
        )
    }
}

/// One configured repository's network clone and publication destinations.
/// URLs have already been passed through the shared display sanitizer before
/// they reach a phone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreflightRepository {
    pub id: String,
    pub fetch_url: String,
    pub default_branch: String,
    pub push_urls: Vec<String>,
}

/// What a preflight found. Isolated sessions expose their complete network
/// source plan so the person can review it before creation. Raw-local targets
/// leave the plan empty because they use the selected checkout directly;
/// isolated targets set `local_changes_excluded` to make the copy boundary
/// explicit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreflightNew {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_directory: Option<PathBuf>,
    #[serde(default)]
    pub managed_worktree: mj_core::state::ManagedWorktreeOptions,
    #[serde(default)]
    pub remote_repairs: Vec<mj_core::local_git::LocalRemoteRepair>,
    #[serde(default)]
    pub dirty_repositories: Vec<String>,
    #[serde(default)]
    pub remote_repositories: Vec<PreflightRepository>,
    pub local_changes_excluded: bool,
    /// Why creating this session turns its review off, when it will.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_notice: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewSessionPreflightRequest {
    #[serde(default)]
    pub additional_mounts: Vec<mj_core::targets::AdditionalMount>,
    #[serde(default)]
    pub remote_repairs: Vec<mj_core::local_git::LocalRemoteRepair>,
    #[serde(default)]
    pub workspace_id: String,
    pub profile_id: String,
    pub bundle_id: String,
    pub target_id: String,
    #[serde(default)]
    pub project_directory: Option<PathBuf>,
}

fn deserialize_availability<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<crate::availability::AvailabilityChecked, D::Error> {
    mj_core::config::with_environment_sources_only(|| serde::Deserialize::deserialize(deserializer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executable::{BuildDescription, describe_daemon_and_client_builds};
    use std::path::Path;

    // Hard-won: c672eb41a1bf: macOS startup hang waited forever for an exited but unreaped daemon
    #[cfg(unix)]
    #[test]
    fn an_exited_unreaped_process_is_a_zombie_and_not_alive() {
        // An upgrade waits for the old daemon to leave. If that daemon exited
        // but is still a child nobody has reaped, `kill(pid, 0)` keeps finding
        // it, so only the zombie check lets the wait end.
        use std::time::{Duration, Instant};

        let mut running = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        assert!(process_is_alive(running.id()));
        assert!(!process_is_zombie(running.id()));
        running.kill().unwrap();
        running.wait().unwrap();

        let mut exited = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = exited.id();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !process_is_zombie(pid) {
            assert!(
                Instant::now() < deadline,
                "an exited, unreaped child was never reported as a zombie"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!process_is_alive(pid));
        exited.wait().unwrap();
    }

    // Hard-won: 372572b0b7b6: stopped daemon endpoint removal broke status and session refresh
    #[test]
    fn a_missing_endpoint_file_reads_as_a_stopped_daemon() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("daemon.json");
        let error = read_metadata_at(&path).unwrap_err();
        let stopped = daemon_not_running(&error).expect("stopped, not an I/O failure");
        assert_eq!(stopped.metadata_path, path);
        assert_eq!(format!("{error:#}"), "the Mjolnir daemon is not running");

        std::fs::write(&path, b"not json").unwrap();
        let error = read_metadata_at(&path).unwrap_err();
        assert!(daemon_not_running(&error).is_none());
    }

    #[tokio::test]
    async fn restart_session_rejects_an_old_daemon_before_sending_the_action() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let metadata = DaemonMetadata {
            protocol_version: PROTOCOL_VERSION - 1,
            pid: std::process::id(),
            address: listener.local_addr().unwrap(),
            token: "old-restart-daemon".into(),
            started_at: "test".into(),
            build_version: env!("CARGO_PKG_VERSION").into(),
            instance_id: String::new(),
            placement: DaemonPlacement::Host,
        };
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut byte = [0_u8; 1];
            assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
        });
        let mut client = DaemonClient::connect(metadata).await.unwrap();

        let error = client
            .restart_session("session-1".into())
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(&format!("daemon protocol {}", PROTOCOL_VERSION - 1)));
        assert!(message.contains(&format!("client protocol {PROTOCOL_VERSION}")));
        assert!(message.contains("restart the daemon"));
        drop(client);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn abandoned_prompt_history_uses_the_daemon_protocol_action() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let metadata = DaemonMetadata {
            protocol_version: PROTOCOL_VERSION,
            pid: std::process::id(),
            address: listener.local_addr().unwrap(),
            token: "abandoned-prompt-test".into(),
            started_at: "test".into(),
            build_version: env!("CARGO_PKG_VERSION").into(),
            instance_id: String::new(),
            placement: DaemonPlacement::Host,
        };
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request: RequestEnvelope = read_frame(&mut stream).await.unwrap();
            assert_eq!(request.protocol_version, PROTOCOL_VERSION);
            assert_eq!(request.token, "abandoned-prompt-test");
            assert_eq!(
                serde_json::to_value(request.action).unwrap(),
                serde_json::json!({
                    "action": "record_abandoned_prompt",
                    "arguments": {
                        "session_id": "session-1",
                        "bundle_id": "project-1",
                        "text": "unfinished draft"
                    }
                })
            );
            write_frame(
                &mut stream,
                &ResponseEnvelope {
                    protocol_version: request.protocol_version,
                    request_id: request.request_id,
                    result: Ok(DaemonReply::Done),
                },
            )
            .await
            .unwrap();
        });
        let mut client = DaemonClient::connect(metadata).await.unwrap();

        client
            .record_abandoned_prompt(
                "session-1".into(),
                "project-1".into(),
                "unfinished draft".into(),
            )
            .await
            .unwrap();
        server.await.unwrap();
    }

    // Hard-won: c502e9bdf662: cold-cache discovery failed to save and left Codex profiles out of model choices
    #[tokio::test]
    async fn subagent_discovery_uses_the_daemon_and_preserves_choices_and_failures() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let metadata = DaemonMetadata {
            protocol_version: PROTOCOL_VERSION,
            pid: std::process::id(),
            address: listener.local_addr().unwrap(),
            token: "subagent-discovery-test".into(),
            started_at: "test".into(),
            build_version: env!("CARGO_PKG_VERSION").into(),
            instance_id: String::new(),
            placement: DaemonPlacement::Host,
        };
        let options: mj_core::subagent::SubagentOptions =
            serde_json::from_value(serde_json::json!({
                "models": [{"value": "gpt-6-luna", "name": "Luna", "description": null}],
                "efforts": [{"value": "high", "name": "High", "description": null}],
                "unavailable": ["another-profile: authentication failed"]
            }))
            .unwrap();
        let expected = serde_json::to_value(&options).unwrap();
        let mut draft = Config::default();
        draft.profiles.insert(
            "codex".into(),
            serde_json::from_value(serde_json::json!({
                "kind": "codex", "home": "/unsaved/profile",
                "subagents": {"mode": "single_model", "model": "gpt-6-luna", "effort": "high"}
            }))
            .unwrap(),
        );
        let expected_draft = serde_json::to_value(&draft).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            for (index, model) in [
                None,
                Some("gpt-6-luna".to_owned()),
                Some("gpt-6-luna".to_owned()),
            ]
            .into_iter()
            .enumerate()
            {
                let request: RequestEnvelope = read_frame(&mut stream).await.unwrap();
                assert_eq!(request.token, "subagent-discovery-test");
                let DaemonAction::SubagentOptions {
                    profile,
                    model: requested,
                    config,
                } = request.action
                else {
                    panic!("unexpected discovery request")
                };
                assert_eq!(profile, "codex");
                assert_eq!(requested, model);
                assert_eq!(
                    serde_json::to_value(config).unwrap(),
                    if index == 1 {
                        expected_draft.clone()
                    } else {
                        serde_json::Value::Null
                    }
                );
                write_frame(
                    &mut stream,
                    &ResponseEnvelope {
                        protocol_version: request.protocol_version,
                        request_id: request.request_id,
                        result: if index != 2 {
                            Ok(DaemonReply::SubagentOptions(options.clone()))
                        } else {
                            Err("profile discovery cancelled".into())
                        },
                    },
                )
                .await
                .unwrap();
            }
        });
        let mut client = DaemonClient::connect(metadata).await.unwrap();
        let discovered = client
            .subagent_options("codex".into(), None, None)
            .await
            .unwrap();
        assert_eq!(serde_json::to_value(discovered).unwrap(), expected);
        let discovered = client
            .subagent_options("codex".into(), Some("gpt-6-luna".into()), Some(draft))
            .await
            .unwrap();
        assert_eq!(serde_json::to_value(discovered).unwrap(), expected);
        let error = client
            .subagent_options("codex".into(), Some("gpt-6-luna".into()), None)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "profile discovery cancelled");
        server.await.unwrap();
    }
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
            instance_id: String::new(),
            placement: DaemonPlacement::Host,
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

    // Hard-won: 25fee3db784d: protocol-mismatch advice omitted which installed mj the user ran
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

    #[tokio::test]
    async fn unavailable_lifecycle_observation_distinguishes_disconnect_from_operation_failure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let metadata = DaemonMetadata {
            protocol_version: PROTOCOL_VERSION,
            pid: std::process::id(),
            address: listener.local_addr().unwrap(),
            token: "lifecycle-observation".into(),
            started_at: "test".into(),
            build_version: env!("CARGO_PKG_VERSION").into(),
            instance_id: String::new(),
            placement: DaemonPlacement::Host,
        };
        let receipt = LifecycleReceipt {
            session_id: "observed-session".into(),
            operation_id: "accepted-operation".into(),
        };
        let server = tokio::spawn(async move {
            for phase in 0..5 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request: RequestEnvelope = read_frame(&mut stream).await.unwrap();
                assert!(matches!(
                    (&request.action, phase),
                    (DaemonAction::WaitSessionLifecycle(_), 0 | 1 | 4)
                        | (DaemonAction::StartSessionLifecycle(_), 2 | 3)
                ));
                if matches!(phase, 1 | 3 | 4) {
                    write_frame(
                        &mut stream,
                        &ResponseEnvelope {
                            protocol_version: request.protocol_version,
                            request_id: request.request_id,
                            result: if phase == 4 {
                                Ok(DaemonReply::LifecycleFailed(
                                    "checkpoint verification failed".into(),
                                ))
                            } else {
                                Err("daemon is shutting down; retry to reach a fresh daemon".into())
                            },
                        },
                    )
                    .await
                    .unwrap();
                }
            }
        });
        let mut client = DaemonClient::connect(metadata.clone()).await.unwrap();
        let error = client
            .wait_session_lifecycle(receipt.clone())
            .await
            .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<LifecycleObservationUnavailable>()
                .unwrap()
                .0,
            receipt
        );
        drop(client);
        let mut client = DaemonClient::connect(metadata.clone()).await.unwrap();
        let error = client
            .wait_session_lifecycle(receipt.clone())
            .await
            .unwrap_err();
        assert!(error.is::<LifecycleObservationUnavailable>());
        assert!(format!("{error:#}").contains("daemon is shutting down"));
        drop(client);
        for disconnected in [true, false] {
            let mut client = DaemonClient::connect(metadata.clone()).await.unwrap();
            let error = client
                .start_session_lifecycle(SessionLifecycleRequest::Suspend {
                    session_id: "observed-session".into(),
                    acknowledge_unpublished_work: true,
                    ignore_nested_repositories: false,
                })
                .await
                .unwrap_err();
            if disconnected {
                assert_eq!(
                    error
                        .downcast_ref::<LifecycleAdmissionUnconfirmed>()
                        .unwrap()
                        .0,
                    "observed-session"
                );
                assert!(!error.is::<DaemonRefusal>());
            } else {
                assert!(error.is::<DaemonRefusal>());
                assert!(!error.is::<LifecycleAdmissionUnconfirmed>());
            }
        }
        let mut client = DaemonClient::connect(metadata).await.unwrap();
        let error = client.wait_session_lifecycle(receipt).await.unwrap_err();
        assert!(error.is::<DaemonRefusal>());
        assert!(!error.is::<LifecycleObservationUnavailable>());
        assert!(error.to_string().contains("checkpoint verification failed"));
        drop(client);
        server.await.unwrap();
    }
}
