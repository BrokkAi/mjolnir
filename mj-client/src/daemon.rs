//! Authenticated local daemon protocol and client transport.
use crate::http::{
    AbandonedPromptRequest, LifecycleCompletion, MoveSourcesRequest, RestartSessionRequest,
    SessionCommandRequest, StartupPromptOutcomesRequest, StartupPromptRequest,
    SuspendPreflightRequest,
};
pub mod connector;
pub mod login_terminal;
mod reply_fragment;
pub mod ssh;
mod ssh_http_proxy;
use crate::executable::describe_running_daemon_and_client_builds;
use crate::session::{ManagedSessionView, ViewError};
use agent_client_protocol::v1::schema::ContentBlock;
use anyhow::{Context, Result, bail, ensure};
use mj_core::config::{Config, data_dir};
use mj_core::credentials::CredentialSyncSignal;
use mj_core::elicitation::ElicitationResponse;
use mj_core::relay::{RelayCommand, RelayOperationalState};
use mj_core::state::*;
use mj_core::targets::{AdditionalMount, ProvisionStage};
use serde::{Deserialize, Serialize};
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Debug)]
pub enum StoredTranscriptItem {
    Found(Box<TranscriptItem>),
    Missing,
}

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

#[derive(Clone, Serialize, Deserialize)]
pub struct ApiEndpointMetadata {
    pub address: SocketAddr,
    pub token: String,
    pub pid: u32,
    pub instance_id: String,
    pub protocol_version: u32,
}

pub fn api_endpoint_path() -> PathBuf {
    api_endpoint_path_in(&data_dir())
}

pub fn api_endpoint_path_in(dir: &Path) -> PathBuf {
    dir.join("api-endpoint.json")
}

pub fn read_api_endpoint() -> Result<Option<ApiEndpointMetadata>> {
    read_api_endpoint_from(&api_endpoint_path())
}

pub fn read_api_endpoint_from(path: &Path) -> Result<Option<ApiEndpointMetadata>> {
    let body = match fs::read(path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    serde_json::from_slice(&body)
        .map(Some)
        .with_context(|| format!("parse {}", path.display()))
}

pub fn write_api_endpoint(metadata: &ApiEndpointMetadata) -> Result<()> {
    write_api_endpoint_to(&api_endpoint_path(), metadata)
}

pub fn write_api_endpoint_to(path: &Path, metadata: &ApiEndpointMetadata) -> Result<()> {
    use std::io::Write as _;

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    mj_core::config::create_owner_only_directory(parent)?;
    // NamedTempFile creates the file with mode 0600 on Unix.
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&serde_json::to_vec_pretty(metadata)?)?;
    file.as_file().sync_all()?;
    #[cfg(windows)]
    if let Err(error) = fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error).with_context(|| format!("replace {}", path.display()));
    }
    file.persist(path)
        .with_context(|| format!("publish {}", path.display()))?;
    Ok(())
}

/// The caller serializes publication and cleanup with its ownership lock.
pub fn remove_api_endpoint_if_matching(
    matches: impl FnOnce(&ApiEndpointMetadata) -> bool,
) -> Result<()> {
    let path = api_endpoint_path();
    if read_api_endpoint_from(&path)?.as_ref().is_some_and(matches) {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("remove {}", path.display())),
        }
    }
    Ok(())
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

pub const CONTAINER_OWNER_SCHEMA: u32 = 1;

/// Launch credentials for a container daemon, including before it is ready.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerDaemonRecord {
    #[serde(default = "default_container_owner_schema")]
    pub schema: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<u32>,
    pub runtime: mj_core::config::DaemonRuntime,
    pub container_id: String,
    pub container_name: String,
    pub address: SocketAddr,
    pub token: String,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_address: Option<SocketAddr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_token: Option<String>,
}

fn default_container_owner_schema() -> u32 {
    CONTAINER_OWNER_SCHEMA
}

impl ContainerDaemonRecord {
    pub fn matches(&self, metadata: &DaemonMetadata) -> bool {
        self.address == metadata.address && self.token == metadata.token
    }
}

pub fn container_owner_path() -> PathBuf {
    data_dir().join("daemon-container.json")
}

pub fn read_container_owner() -> Result<Option<ContainerDaemonRecord>> {
    read_container_owner_at(&container_owner_path())
}

fn read_container_owner_at(path: &std::path::Path) -> Result<Option<ContainerDaemonRecord>> {
    let body = match fs::read(path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    serde_json::from_slice(&body)
        .map(Some)
        .with_context(|| format!("parse {}", path.display()))
}

pub fn write_container_owner(record: &ContainerDaemonRecord) -> Result<()> {
    let path = container_owner_path();
    let parent = path
        .parent()
        .context("container owner path has no parent")?;
    mj_core::config::create_owner_only_directory(parent)?;
    mj_core::config::atomic_write_existing(&path, &serde_json::to_vec_pretty(record)?)
}

/// The caller serializes owner changes with the daemon startup lock.
pub fn remove_container_owner_if_matching(address: SocketAddr, token: &str) -> Result<()> {
    remove_container_owner_at(&container_owner_path(), address, token)
}

fn remove_container_owner_at(
    path: &std::path::Path,
    address: SocketAddr,
    token: &str,
) -> Result<()> {
    if read_container_owner_at(path)?
        .is_some_and(|record| record.address == address && record.token == token)
    {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("remove {}", path.display())),
        }
    }
    Ok(())
}

/// Process-private credentials returned by `mj daemon endpoint --json`.
/// This is not local PID metadata and must never be written to daemon.json.
#[derive(Clone, Serialize, Deserialize)]
pub struct DaemonEndpoint {
    #[serde(default)]
    pub config_path: Option<std::path::PathBuf>,
    pub protocol_version: u32,
    pub build_version: String,
    pub control_address: SocketAddr,
    pub token: String,
    pub instance_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_address: Option<SocketAddr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_token: Option<String>,
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
                "No coding agent found yet. Mjolnir can install one and sign you in.".to_owned(),
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
}

/// Progress within one adapter of the current sync phase.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WikiSyncProgress {
    pub phase: String,
    pub tool: String,
    pub adapter: usize,
    pub adapters: usize,
    pub done: usize,
    pub total: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WikiSyncOutcome {
    Succeeded,
    Failed(String),
}

/// One ordered snapshot from the indexer's generation owner.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WikiSyncStatus {
    pub epoch: String,
    pub version: u64,
    pub requested: u64,
    pub started: u64,
    pub finished: u64,
    pub successful: u64,
    pub outcome: Option<WikiSyncOutcome>,
    pub running: Option<WikiSyncProgress>,
    pub target: Option<u64>,
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTextSearchPage {
    pub matches: Vec<SessionTextMatch>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSessionView {
    /// In-process viewers need full queue and form bodies. Terminal clients
    /// receive them through their transcript tails.
    #[serde(skip)]
    pub materialized: Option<mj_core::snapshot_map::SharedSnapshot<RuntimeMaterializedSession>>,
    /// A retained projection for a session with no relay view.
    #[serde(skip)]
    pub materialized_only: bool,
    pub attention: mj_core::attention::SessionAttention,
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

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RuntimeMaterializedSession {
    pub execution: mj_core::state::MaterializedExecutionState,
    pub last_activity_at_ms: Option<i64>,
    pub queued_prompts: Vec<mj_core::relay::QueuedPrompt>,
    pub pending_elicitations: Vec<mj_core::elicitation::ElicitationRequest>,
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
            materialized: None,
            materialized_only: false,
            session_id,
            attention: view.attention,
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
        #[serde(default)]
        discard_untracked: mj_core::suspend_consent::UntrackedConsent,
        #[serde(default)]
        abort_operations: mj_core::suspend_consent::OperationConsent,
        #[serde(default)]
        without_checkpoint: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        confirm_no_checkpoint: bool,
    },
    Restart {
        session_id: String,
        acknowledge_container_recreation: bool,
    },
    Resume(Box<ResumeSessionRequest>),
    Move(Box<MoveSessionRequest>),
    DestroyStopped {
        session_id: String,
    },
    ForceDestroy {
        session_id: String,
    },
}

impl SessionLifecycleRequest {
    pub fn session_id(&self) -> &str {
        match self {
            Self::Suspend { session_id, .. }
            | Self::Restart { session_id, .. }
            | Self::DestroyStopped { session_id }
            | Self::ForceDestroy { session_id } => session_id,
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
    /// Start with notifications ignored; see `SessionRecord::notifications_ignored`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub notifications_ignored: bool,
    #[serde(default)]
    pub initial_prompt: Option<String>,
    pub workspace_id: String,
    pub profile_id: String,
    /// When `profile_id` is empty and `provider` is set, load-balance across
    /// enabled profiles belonging to this provider instead of `[[subagents.primary]]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<mj_core::config::QuotaProvider>,
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
    pub session_title: Option<String>,
    pub preview: String,
    pub source: String,
    pub owner_pid: Option<u32>,
    pub saved_at: String,
}

/// One workspace's detached drafts, with bounded previews.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceDrafts {
    pub workspace_id: String,
    pub drafts: Vec<DraftPreview>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryAdoptionRequest {
    pub session_id: String,
    pub target_id: String,
    pub profile: Option<String>,
    pub bundle: Option<String>,
    pub all_instances: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryAdoptionProgress {
    Waiting {
        blockers: Vec<String>,
        destroy_command: Option<String>,
    },
    Installed {
        session_id: String,
    },
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
    ProveIdentity {
        nonce: String,
    },
    LoginTerminal {
        profile_id: String,
        cols: u16,
        rows: u16,
    },
    PreviewBuildCache {
        machine: mj_core::config::Machine,
    },
    InstallMbx {
        machine: mj_core::config::Machine,
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
    SetQuotaPreferences {
        prefer: Vec<String>,
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
    CheckProjectDirectories {
        paths: Vec<std::path::PathBuf>,
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
    /// Whether first-run setup has work to do: a new installation, or one
    /// whose setup did not finish. A dashboard asks before it shows the
    /// first-run welcome, which takes the keyboard.
    FirstRunSetupDue,
    /// Discover and persist the first-run configuration in the daemon's own
    /// environment. A client in another placement asks its daemon instead of
    /// writing the client's `config.toml`.
    RunFirstRunSetup,
    /// Use the client's directory when both processes can see the same paths.
    RunFirstRunSetupIn {
        project_directory: PathBuf,
    },
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
    ApiCredential,
    /// The daemon host's Git commit author, which its sessions inherit.
    /// Write the daemon host's global Git commit author.
    RecoverWebViewer(crate::web::WebViewerRecovery),
    InspectWebListener,
    /// Legacy wire tombstone: rejected with guidance to suspend.
    CheckpointSession {
        session_id: String,
    },
    ScanRecovery {
        all_instances: bool,
    },
    AdoptRecovery(RecoveryAdoptionRequest),
    DestroyRecovery {
        session_id: String,
        target_id: String,
        confirmation: String,
        all_instances: bool,
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
    /// Every host and fleet with the capacity service's latest CPU, RAM and
    /// filesystem readings. Added in protocol 101.
    HostCapacity,
    /// Re-read the daemon probe definitions and sample every host.
    RefreshCapacity,
    /// Ask the daemon's quota poller to probe now. The daemon is the only
    /// process that probes; the result arrives in the runtime feed. Added in
    /// protocol 43.
    RefreshQuota,
    /// Probe exactly these profiles once, even disabled ones, and serve their
    /// reports in the runtime feed. Added in protocol 99; `forced` (probe now,
    /// off the profile's schedule, even with a fresh report) in protocol 100.
    ProbeQuotaProfiles {
        profile_ids: Vec<String>,
        forced: bool,
    },
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

/// The connection to the daemon dropped after the request was sent and before
/// an answer arrived. The daemon may or may not have acted on it, so only a
/// read-only request may be repeated on that evidence. Failing to connect, a
/// protocol mismatch and a refusal are different errors and never carry this.
#[derive(Debug)]
pub struct DaemonTransportLost;

impl std::fmt::Display for DaemonTransportLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("lost the daemon connection before it answered")
    }
}

impl std::error::Error for DaemonTransportLost {}

/// Mark a failure of the established stream as a lost transport. An error that
/// did not come from the socket (an oversized frame, an undecodable reply)
/// stays as it is.
fn transport_lost(error: anyhow::Error) -> anyhow::Error {
    if error.chain().any(|cause| cause.is::<std::io::Error>()) {
        error.context(DaemonTransportLost)
    } else {
        error
    }
}

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
    AdoptionInstalled {
        session_id: String,
    },
    AdoptionWaiting {
        blockers: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        destroy_command: Option<String>,
    },
    IdentityProof {
        mac: Vec<u8>,
    },
    LoginTerminalReady,
    BuildCachePreview(Option<mj_core::state::BuildCachePreview>, bool),
    MbxInstalled(String),
    CheckpointArchiveSizes(
        std::collections::BTreeMap<String, std::result::Result<Option<u64>, String>>,
    ),
    SetupDiscovery(Box<crate::discovery::SetupDetection>),
    ProfileLoginStatuses(std::collections::BTreeMap<String, mj_core::credentials::LoginStatus>),
    NewSessionPreflight(PreflightNew),
    ProjectDirectories(Vec<(std::path::PathBuf, bool)>),
    ResolvedPath(std::path::PathBuf),
    MountValidation(std::path::PathBuf, Option<String>),
    SessionMountValidation(Option<(String, String)>),
    Pong,
    UpgradePending,
    /// Labels for the work holding an automatic handoff open, empty when
    /// nothing is. Answers `DaemonAction::UpgradeBlockers`.
    UpgradeBlockers(Vec<String>),
    Identity(DaemonIdentity),
    Configuration(Box<Config>),
    FirstRunSetupDue(bool),
    FirstRunSetup(Option<FirstRunSetupReport>),
    DoctorErrors(Vec<String>),
    RemoteRepairs(Vec<mj_core::local_git::LocalRemoteRepair>),
    ProjectSource(mj_core::state::ProjectSourceIdentity),
    Status(DaemonStatus),
    WebViewerAccess(crate::web::WebViewerAccess),
    ApiCredential(String),
    WebListeners(Vec<crate::web::WebListenerProcess>),
    SubagentOptions(mj_core::subagent::SubagentOptions),
    HostCapacity(Vec<mj_core::targets::HostCapacityRow>),
    SavedSetup(Box<Config>),
    ArchiveSpacePreview(ArchiveSpacePreview),
    ResumeRepositoryPreflight(Box<ResumeRepositorySourcePreflight>),
    RuntimeChanges(Box<crate::runtime_feed::RuntimeFrame>),
    /// Transport fragments of one chunked reply (see
    /// [`DaemonReply::is_chunked`]); never exposed to consumers.
    ReplyChunk {
        #[serde(with = "reply_fragment")]
        bytes: Vec<u8>,
        finished: bool,
    },
    RecoveryScan(mj_core::state::RecoveryScan),
    Done,
}

impl DaemonReply {
    /// Replies that can outgrow one frame. The daemon sends them as
    /// [`DaemonReply::ReplyChunk`] fragments and the client reassembles them.
    pub fn is_chunked(&self) -> bool {
        matches!(self, Self::Configuration(_) | Self::RuntimeChanges(_))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterruptAllResult {
    pub requested: usize,
    pub failures: Vec<(String, String)>,
}

/// An on-demand record and its verdict from the same capture.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRecordWithVerdict {
    pub record: SessionRecord,
    pub verdict: crate::runtime_feed::SessionVerdict,
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
/// the whole record (`GET /api/v1/sessions/{id}/record`) only for the row picked.
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

pub async fn write_frame<T: Serialize>(
    stream: &mut (impl tokio::io::AsyncWrite + Unpin),
    value: &T,
) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    write_encoded_frame(stream, &body).await
}

pub async fn write_encoded_frame(
    stream: &mut (impl tokio::io::AsyncWrite + Unpin),
    body: &[u8],
) -> Result<()> {
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

pub async fn read_frame<T: for<'de> Deserialize<'de>>(
    stream: &mut (impl tokio::io::AsyncRead + Unpin),
) -> Result<T> {
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
    let reply: DaemonReply =
        mj_core::runtime::spawn_blocking(move || serde_json::from_slice(&body))
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
    ssh_lease: Option<std::sync::Arc<ssh::SshLease>>,
    connector: Option<std::sync::Arc<connector::DaemonConnector>>,
    metadata: DaemonMetadata,
    stream: Option<TcpStream>,
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
            stream: Some(stream),
            next_request_id: 1,
            connector: None,
            ssh_lease: None,
        })
    }

    /// Connect through an SSH tunnel's local port and require the daemon to
    /// prove it holds the token on this very connection before any request
    /// carries the token. The tunnel may have exited since an earlier proof,
    /// and another process may now hold its port.
    pub(crate) async fn connect_proven(metadata: DaemonMetadata) -> Result<Self> {
        let mut client = Self::connect(metadata).await?;
        let mut stream = client.take_stream()?;
        ensure!(
            ssh::prove_stream(&mut stream, &client.metadata.token).await?,
            "the SSH tunnel's local port {} did not prove it reaches the daemon",
            client.metadata.address
        );
        client.stream = Some(stream);
        Ok(client)
    }

    pub(crate) fn metadata(&self) -> &DaemonMetadata {
        &self.metadata
    }

    pub fn pid(&self) -> u32 {
        self.metadata.pid
    }

    /// Connector-owned SSH origin; never inferred from daemon-host placement.
    pub fn connector(&self) -> Option<&std::sync::Arc<connector::DaemonConnector>> {
        self.connector.as_ref()
    }

    /// Describe the authenticated endpoint, retaining the address actually
    /// dialed (including a container's published loopback port).
    pub async fn endpoint(&mut self) -> Result<DaemonEndpoint> {
        let identity = self.identity().await?;
        let api = if self.connector().is_some_and(|connector| connector.is_ssh()) {
            None
        } else {
            self.matching_local_api_endpoint(&identity)?
        };
        Ok(DaemonEndpoint {
            api_address: api.as_ref().map(|endpoint| endpoint.address),
            api_token: api.map(|endpoint| endpoint.token),
            config_path: None,
            protocol_version: self.metadata.protocol_version,
            build_version: identity.build_version,
            control_address: self.metadata.address,
            token: self.metadata.token.clone(),
            instance_id: identity.instance_id,
        })
    }

    fn matching_local_api_endpoint(
        &self,
        identity: &DaemonIdentity,
    ) -> Result<Option<ApiEndpointMetadata>> {
        if identity.instance_id.is_empty() {
            return Ok(None);
        }
        Ok(read_api_endpoint()?.filter(|endpoint| {
            endpoint.pid == self.metadata.pid
                && endpoint.instance_id == identity.instance_id
                && !endpoint.token.is_empty()
        }))
    }

    pub fn direct_api_endpoint(
        &self,
        identity: &DaemonIdentity,
    ) -> Result<Option<(String, String)>> {
        if self.connector().is_some_and(|connector| connector.is_ssh()) {
            return Ok(self
                .ssh_lease
                .as_deref()
                .and_then(ssh::SshLease::api_endpoint));
        }
        Ok(self
            .matching_local_api_endpoint(identity)?
            .map(|endpoint| (format!("http://{}", endpoint.address), endpoint.token)))
    }

    /// Keep the process-private forward alive for an HTTP client that outlives
    /// this control connection.
    pub fn ssh_lease(&self) -> Option<std::sync::Arc<ssh::SshLease>> {
        self.ssh_lease.clone()
    }

    /// Speak the daemon's advertised dialect, not this build's: management
    /// requests must reach daemons of any protocol version, and the frozen
    /// subset encodes identically across all of them.
    pub async fn request(&mut self, action: DaemonAction) -> Result<DaemonReply> {
        let connector = self.connector.clone();
        let previous = self.metadata.clone();
        self.request_with_reconnect(action, || async {
            if let Some(connector) = &connector {
                return connector.refresh(&previous).await;
            }
            loop {
                if let Ok(client) = connect_existing().await {
                    return Ok(client);
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await
    }

    /// Exchange one request on this connection, exposing an upgrade refusal to the caller.
    pub async fn request_single(&mut self, action: DaemonAction) -> Result<DaemonReply> {
        self.request_once(action).await
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
        let mut stream = self.take_stream()?;
        let protocol_version = self.metadata.protocol_version;
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        write_frame(
            &mut stream,
            &RequestEnvelope {
                protocol_version,
                request_id,
                token: self.metadata.token.clone(),
                action,
            },
        )
        .await
        .map_err(transport_lost)?;
        self.finish_exchange(stream, request_id)
            .await?
            .result
            .map_err(|message| anyhow::Error::new(DaemonRefusal(message)))
    }

    // The exchange owns framing until a complete, checked response arrives.
    // Cancellation or an I/O error drops the stream instead of leaving it reusable.
    fn take_stream(&mut self) -> Result<TcpStream> {
        self.stream
            .take()
            .context("connection lost to an interrupted request")
            .context(DaemonTransportLost)
    }

    async fn finish_exchange(
        &mut self,
        mut stream: TcpStream,
        request_id: u64,
    ) -> Result<ResponseEnvelope> {
        let response = read_response(&mut stream).await.map_err(transport_lost)?;
        ensure!(
            response.protocol_version == self.metadata.protocol_version,
            "daemon changed protocol"
        );
        ensure!(
            response.request_id == request_id,
            "daemon crossed request IDs: expected {request_id}, received {}",
            response.request_id
        );
        self.stream = Some(stream);
        Ok(response)
    }

    pub async fn http(&mut self) -> Result<crate::http::HttpDaemonClient> {
        crate::http::HttpDaemonClient::from_daemon_client(self).await
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

    /// Whether the connected daemon can resolve paths from this client.
    pub fn shares_client_filesystem(&self, identity: &DaemonIdentity) -> bool {
        !self.connector().is_some_and(|connector| connector.is_ssh())
            && identity.placement.container() == mj_core::placement::daemon_container().as_deref()
    }

    pub async fn first_run_setup_due(&mut self) -> Result<bool> {
        match self.request(DaemonAction::FirstRunSetupDue).await? {
            DaemonReply::FirstRunSetupDue(due) => Ok(due),
            reply => bail!("unexpected first-run setup reply {reply:?}"),
        }
    }

    pub async fn run_first_run_setup(
        &mut self,
        project_directory: Option<PathBuf>,
    ) -> Result<Option<FirstRunSetupReport>> {
        let action = match project_directory {
            Some(project_directory) => DaemonAction::RunFirstRunSetupIn { project_directory },
            None => DaemonAction::RunFirstRunSetup,
        };
        match self.request(action).await? {
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

    /// Obtain the HTTP bearer credential from its daemon owner.
    pub async fn api_credential(&mut self) -> Result<String> {
        match self.request(DaemonAction::ApiCredential).await? {
            DaemonReply::ApiCredential(token) => Ok(token),
            reply => bail!("unexpected API credential reply: {reply:?}"),
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

    /// The capacity service's latest reading of every host and fleet. This
    /// reads what the daemon holds and never probes.
    pub async fn host_capacity(&mut self) -> Result<Vec<mj_core::targets::HostCapacityRow>> {
        match self.request(DaemonAction::HostCapacity).await? {
            DaemonReply::HostCapacity(rows) => Ok(rows),
            reply => bail!("unexpected host-capacity reply {reply:?}"),
        }
    }

    /// Ask the daemon to refresh host capacity. Readings arrive on the feed.
    pub async fn refresh_capacity(&mut self) -> Result<()> {
        match self.request(DaemonAction::RefreshCapacity).await? {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected refresh-capacity reply {reply:?}"),
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

    /// Ask the daemon to probe exactly these profiles once, even disabled
    /// ones. `forced` probes now, off each profile's schedule, even with a
    /// fresh report. Returns when the daemon has queued the probes, not when
    /// they finish; the reports arrive in the runtime feed.
    pub async fn probe_quota_profiles(
        &mut self,
        profile_ids: Vec<String>,
        forced: bool,
    ) -> Result<()> {
        match self
            .request(DaemonAction::ProbeQuotaProfiles {
                profile_ids,
                forced,
            })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected probe-quota reply {reply:?}"),
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

    /// Persist a prompt the user abandoned in the chat composer.
    pub async fn record_abandoned_prompt(
        &mut self,
        session_id: String,
        bundle_id: String,
        text: String,
    ) -> Result<()> {
        let request = AbandonedPromptRequest { bundle_id, text };
        self.http()
            .await?
            .record_abandoned_prompt(session_id, request)
            .await
    }

    /// Write the load-balancing preference order, most preferred first.
    pub async fn set_quota_preferences(&mut self, prefer: Vec<String>) -> Result<()> {
        match self
            .request(DaemonAction::SetQuotaPreferences { prefer })
            .await?
        {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected quota-preference save reply {reply:?}"),
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
        adoption: RecoveryAdoptionRequest,
        mut progress: impl FnMut(RecoveryAdoptionProgress),
    ) -> Result<()> {
        let mut stream = self.take_stream()?;
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        write_frame(
            &mut stream,
            &RequestEnvelope {
                protocol_version: self.metadata.protocol_version,
                request_id,
                token: self.metadata.token.clone(),
                action: DaemonAction::AdoptRecovery(adoption),
            },
        )
        .await
        .map_err(transport_lost)?;
        loop {
            let response = read_response(&mut stream).await.map_err(transport_lost)
                .context("adoption connection ended before completion; check mj sessions, then rerun recover adopt if needed")?;
            ensure!(
                response.protocol_version == self.metadata.protocol_version,
                "daemon changed protocol"
            );
            ensure!(
                response.request_id == request_id,
                "daemon crossed request IDs: expected {request_id}, received {}",
                response.request_id
            );
            match response.result {
                Ok(DaemonReply::AdoptionWaiting {
                    blockers,
                    destroy_command,
                }) => {
                    progress(RecoveryAdoptionProgress::Waiting {
                        blockers,
                        destroy_command,
                    });
                    continue;
                }
                Ok(DaemonReply::AdoptionInstalled { session_id }) => {
                    progress(RecoveryAdoptionProgress::Installed { session_id });
                    continue;
                }
                _ => {}
            }
            self.stream = Some(stream);
            return match response.result {
                Ok(DaemonReply::Done) => Ok(()),
                Ok(reply) => bail!("unexpected recovery-adoption reply {reply:?}"),
                Err(error) => bail!("{error}"),
            };
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

    pub async fn submit_session_command(
        &mut self,
        session_id: String,
        command_id: String,
        command: RelayCommand,
        inherited_draft: Option<String>,
    ) -> Result<u64> {
        let request = SessionCommandRequest {
            command_id,
            command,
            inherited_draft,
        };
        self.http()
            .await?
            .submit_session_command(session_id, request)
            .await
            .map_err(terminal_action_refusal)
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
        let request = StartupPromptRequest {
            command_id,
            text,
            prompt,
            encoded_draft,
            move_operation_id,
            inherited_draft,
        };
        self.http()
            .await?
            .queue_startup_prompt(session_id, request)
            .await
            .map_err(terminal_action_refusal)
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
        self.http()
            .await?
            .withdraw_startup_prompt(session_id, command_id)
            .await
    }

    /// Read only the requested durable terminal receipts (at most 128 IDs).
    pub async fn startup_prompt_outcomes(
        &mut self,
        session_id: String,
        command_ids: Vec<String>,
    ) -> Result<std::collections::BTreeMap<String, StartupPromptOutcome>> {
        let request = StartupPromptOutcomesRequest { command_ids };
        self.http()
            .await?
            .startup_prompt_outcomes(session_id, request)
            .await
    }

    pub async fn sync_session(&mut self, session_id: String) -> Result<()> {
        self.http().await?.sync_session(session_id).await
    }

    pub async fn respond_elicitation(
        &mut self,
        session_id: String,
        elicitation_id: String,
        response: ElicitationResponse,
    ) -> Result<()> {
        self.http()
            .await?
            .respond_elicitation(session_id, elicitation_id, response)
            .await
    }

    /// Submit once and retain the receipt; a lost reply does not permit replay.
    pub async fn start_session_lifecycle(
        &mut self,
        request: SessionLifecycleRequest,
    ) -> Result<LifecycleReceipt> {
        let session_id = request.session_id().to_owned();
        async { self.http().await?.start_session_lifecycle(request).await }
            .await
            .map_err(|error| {
                if let Some(failure) = error.downcast_ref::<crate::http::HttpApiError>() {
                    DaemonRefusal(failure.message.clone()).into()
                } else if error.is::<DaemonRefusal>() {
                    error
                } else {
                    anyhow::Error::new(LifecycleAdmissionUnconfirmed(session_id)).context(error)
                }
            })
    }

    /// Observe exactly this operation. Dropping the observer never cancels it.
    pub async fn wait_session_lifecycle(
        &mut self,
        receipt: LifecycleReceipt,
    ) -> Result<LifecycleCompletion> {
        let reply = async {
            self.http()
                .await?
                .wait_session_lifecycle(receipt.clone())
                .await
        }
        .await
        .map_err(|error| {
            anyhow::Error::new(LifecycleObservationUnavailable(receipt.clone())).context(error)
        })?;
        match reply {
            reply @ (LifecycleCompletion::Done | LifecycleCompletion::MoveOutcome(_)) => Ok(reply),
            LifecycleCompletion::LifecycleFailed(detail) => Err(DaemonRefusal(detail).into()),
            LifecycleCompletion::LifecycleObservationUnavailable(receipt) => {
                Err(LifecycleObservationUnavailable(receipt).into())
            }
        }
    }

    pub async fn suspend_preflight(
        &mut self,
        session_id: String,
        dry_run: bool,
        without_checkpoint: bool,
    ) -> Result<mj_core::suspend_consent::SuspendPreflight> {
        let request = SuspendPreflightRequest {
            dry_run,
            without_checkpoint,
        };
        self.http()
            .await?
            .suspend_preflight(session_id, request)
            .await
    }

    pub async fn suspend_session(&mut self, session_id: String) -> Result<()> {
        self.suspend_session_with_consent(
            session_id,
            mj_core::suspend_consent::SuspendConsent::default(),
        )
        .await
    }

    pub async fn suspend_session_with_consent(
        &mut self,
        session_id: String,
        consent: mj_core::suspend_consent::SuspendConsent,
    ) -> Result<()> {
        let request = consent;
        self.http()
            .await?
            .suspend_session_with_consent(session_id, request)
            .await
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
        let request = RestartSessionRequest {
            acknowledge_container_recreation,
        };
        self.http()
            .await?
            .restart_session_with_ack(session_id, request)
            .await
    }

    pub async fn start_create_session(
        &mut self,
        request: CreateSessionRequest,
    ) -> Result<RegisteredSession> {
        self.http().await?.start_create_session(request).await
    }

    /// Wait for a session's creation to end. Answered from durable state, so
    /// any daemon gives the same answer; the wait is read-only and repeatable.
    /// One attempt with no internal reconnect: a daemon that stops drops the
    /// connection, which carries [`DaemonTransportLost`], and the caller
    /// starts the next daemon and asks again.
    pub async fn wait_create_session(&mut self, session_id: String) -> Result<()> {
        self.http().await?.wait_create_session(session_id).await
    }

    pub async fn resume_session(&mut self, request: ResumeSessionRequest) -> Result<()> {
        self.http().await?.resume_session(request).await
    }

    pub async fn destroy_stopped_session(&mut self, session_id: String) -> Result<()> {
        self.http().await?.destroy_stopped_session(session_id).await
    }

    pub async fn force_destroy_session(&mut self, session_id: String) -> Result<()> {
        self.http().await?.force_destroy_session(session_id).await
    }

    pub async fn cancel_lifecycle(&mut self, session_id: String) -> Result<()> {
        self.http().await?.cancel_lifecycle(session_id).await
    }

    pub async fn stop(&mut self) -> Result<()> {
        match self.request(DaemonAction::Stop).await? {
            DaemonReply::Done => Ok(()),
            reply => bail!("unexpected stop reply {reply:?}"),
        }
    }
}

pub async fn connect_existing() -> Result<DaemonClient> {
    connector::DaemonConnector::shared()
        .await?
        .connect_existing()
        .await
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
// Clients obtain the HTTP API credential over control with ApiCredential.
// The dashboard's path, project and preflight work are daemon actions, and
// closing a cancellable action's connection cancels it.
// Version 91: unauthenticated nonce proof before SSH tunnel credentials.
// Authenticated chunked client uploads and daemon-owned attachment installation.
// Daemon-owned voice credential discovery and uploaded-audio transcription.
// Version 92: streamed uploads, client imports and daemon voice actions.
// Version 93: Antigravity harness identities in session and profile snapshots.
// Version 94: served unread attention, notification dismissal targets and
// unread frontiers.
// Version 95: authenticated daemon-owned login terminal streams.
// Version 95: milestone-4 setup discovery, availability, login status and machine actions.
// Version 97: the daemon decides which sessions are notifications.
// Version 98: container settings use the single resource allocation.
// Version 99: one-shot quota probes for profiles outside the enabled set.
// Version 100: a one-shot quota probe can be forced off the profile's
// schedule; the daemon answers one all-workspaces draft listing.
// Version 101: the daemon answers the capacity service's per-host readings.
// Version 102: review views carry the writing_brief phase and brief-writer role.
// Version 103: configuration carries the top-level `[images]` section (project
// images built by `mj image build`), which older clients' strict config reader
// rejects.
// Version 104: clients read configuration keys they have no field for, so a
// configuration key added later needs no protocol version of its own; only
// the configuration file refuses a key no field reads. `SubagentChoice` is
// the exception and still refuses unknown keys everywhere.
// Version 105: indexed single-item stored transcript reads for notification
// text use chunked replies; legacy count-mode stored projection tails remain
// unchunked.
/// The first protocol with daemon-owned notify-once alert events.
pub const ALERT_EVENTS_PROTOCOL: u32 = 109;
// Version 107: explicit SessionWiki sync generations and progress watches.
// Version 109: the daemon answers whether first-run setup is due, so a
// dashboard shows the first-run welcome only for an installation that needs it.
/// Explicit checkpoint actions were removed; legacy requests get a clear refusal.
pub const SUSPEND_ONLY_CHECKPOINT_PROTOCOL: u32 = 110;
/// Adoption streams cancellable quiescence waits before its atomic install.
pub const QUIESCENT_ADOPTION_PROTOCOL: u32 = SUSPEND_ONLY_CHECKPOINT_PROTOCOL + 1;
/// Tool permission forms name their transcript call, which older clients reject.
pub const TOOL_PERMISSION_CALL_PROTOCOL: u32 = QUIESCENT_ADOPTION_PROTOCOL + 1;
/// The runtime feed carries host capacity, and clients can refresh it.
pub const RUNTIME_HOST_CAPACITY_PROTOCOL: u32 = TOOL_PERMISSION_CALL_PROTOCOL + 1;
/// Availability checks are requested independently of their runtime feed results.
pub const RUNTIME_AVAILABILITY_PROTOCOL: u32 = RUNTIME_HOST_CAPACITY_PROTOCOL + 1;
pub const RUNTIME_SESSION_MATERIALIZED_PROTOCOL: u32 = RUNTIME_AVAILABILITY_PROTOCOL + 1;
pub const RUNTIME_SESSION_VERDICTS_PROTOCOL: u32 = RUNTIME_SESSION_MATERIALIZED_PROTOCOL + 1;
pub const RUNTIME_SHELL_INTERRUPT_PROTOCOL: u32 = RUNTIME_SESSION_VERDICTS_PROTOCOL + 1;
/// A review view names its review and the repositories it covers, and a
/// session can have several open. Older readers reject the new fields.
pub const RUNTIME_REVIEW_IDENTITY_PROTOCOL: u32 = RUNTIME_SHELL_INTERRUPT_PROTOCOL + 1;
/// Typed suspend consent, suspend preflight, and removal of publication consent and delete_branch.
pub const SUSPEND_CONSENT_PROTOCOL: u32 = RUNTIME_REVIEW_IDENTITY_PROTOCOL + 1;
pub const MOVE_ORIGIN_DELTA_PROTOCOL: u32 = SUSPEND_CONSENT_PROTOCOL + 1;
/// Supervisor delegation adds `supervisors` on session creation/move and runtime feed.
pub const SUPERVISORS_PROTOCOL: u32 = MOVE_ORIGIN_DELTA_PROTOCOL + 1;
/// Orchestrator opt-in is part of the delegation policy.
pub const ORCHESTRATORS_PROTOCOL: u32 = SUPERVISORS_PROTOCOL + 1;
const PRE_HTTP_SESSION_ACTIONS_PROTOCOL: u32 = ORCHESTRATORS_PROTOCOL + 1;
/// Dashboard reads, terminal presence, and session settings use HTTP.
pub const PROTOCOL_VERSION: u32 = PRE_HTTP_SESSION_ACTIONS_PROTOCOL + 4;
/// Guidance for clients sending the removed checkpoint action.
pub const REMOVED_CHECKPOINT_MESSAGE: &str =
    "The checkpoint action was removed; suspend saves the session.";
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
    pub async fn move_sources(
        &mut self,
        session_id: String,
        cleanup_operation_id: Option<String>,
    ) -> Result<Vec<mj_core::move_workspace::RetainedMoveSource>> {
        let request = MoveSourcesRequest {
            cleanup_operation_id,
        };
        self.http().await?.move_sources(session_id, request).await
    }

    pub async fn move_session(&mut self, request: MoveSessionRequest) -> Result<MoveOutcome> {
        self.http().await?.move_session(request).await
    }
}

fn terminal_action_refusal(error: anyhow::Error) -> anyhow::Error {
    if let Some(failure) = error.downcast_ref::<crate::http::HttpApiError>() {
        DaemonRefusal(failure.message.clone()).into()
    } else {
        error
    }
}

impl DaemonAction {
    /// Disposable machine work uses dedicated connections. Closing one or
    /// handing off the daemon cancels it; interrupted installs can be rerun.
    pub fn is_cancellable_machine_action(&self) -> bool {
        matches!(
            self,
            Self::InstallMbx { .. }
                | Self::PreviewBuildCache { .. }
                | Self::CheckpointArchiveSizes { .. }
                | Self::TestTarget { .. }
                | Self::ProfileLoginStatuses
                | Self::DiscoverSetup { .. }
                | Self::ValidateProjectDirectory { .. }
                | Self::CheckProjectDirectories { .. }
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

/// Domain-separated proof shared by the daemon and SSH transport.
pub fn identity_proof(token: &str, nonce: &str) -> hmac::Hmac<sha2::Sha256> {
    use hmac::Mac;
    let mut mac = <hmac::Hmac<sha2::Sha256> as hmac::KeyInit>::new_from_slice(token.as_bytes())
        .expect("HMAC accepts arbitrary key lengths");
    mac.update(b"mj-daemon-identity-v1");
    mac.update(nonce.as_bytes());
    mac
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executable::{BuildDescription, describe_daemon_and_client_builds};
    use std::path::Path;

    #[test]
    fn container_owner_tolerates_future_fields_while_legacy_metadata_stays_strict() {
        let directory = tempfile::tempdir().unwrap();
        let owner = directory.path().join("daemon-container.json");
        fs::write(
            &owner,
            r#"{
                "runtime": "podman",
                "container_id": "full-id",
                "container_name": "mj-daemon",
                "address": "127.0.0.1:8787",
                "token": "fixture-token",
                "started_at": "2026-11-23T00:00:00Z",
                "future_field": {"extra": true}
            }"#,
        )
        .unwrap();
        let record = read_container_owner_at(&owner).unwrap().unwrap();
        assert_eq!(record.schema, CONTAINER_OWNER_SCHEMA);
        assert_eq!(record.container_id, "full-id");
        let other_address = "127.0.0.1:8788".parse().unwrap();
        remove_container_owner_at(&owner, other_address, &record.token).unwrap();
        remove_container_owner_at(&owner, record.address, "replacement-token").unwrap();
        assert_eq!(
            read_container_owner_at(&owner).unwrap(),
            Some(record.clone())
        );
        remove_container_owner_at(&owner, record.address, &record.token).unwrap();
        assert_eq!(read_container_owner_at(&owner).unwrap(), None);

        let metadata = directory.path().join("daemon.json");
        let mut fixture = serde_json::json!({
            "protocol_version": PROTOCOL_VERSION,
            "pid": 0,
            "address": record.address,
            "token": record.token,
            "started_at": record.started_at,
            "build_version": "2.73.0"
        });
        fs::write(&metadata, fixture.to_string()).unwrap();
        assert!(record.matches(&read_metadata_at(&metadata).unwrap()));
        fixture["container_id"] = record.container_id.into();
        fs::write(&metadata, fixture.to_string()).unwrap();
        let error = read_metadata_at(&metadata).unwrap_err();
        assert!(format!("{error:#}").contains("unknown field `container_id`"));
    }

    #[test]
    fn api_endpoint_discovery_tolerates_future_fields() {
        let directory = tempfile::tempdir().unwrap();
        let path = api_endpoint_path_in(directory.path());
        fs::write(
            &path,
            serde_json::json!({
                "address": "127.0.0.1:8787",
                "token": "fixture-token",
                "pid": 42,
                "instance_id": "current-start",
                "protocol_version": PROTOCOL_VERSION,
                "future_field": {"extra": true}
            })
            .to_string(),
        )
        .unwrap();
        let endpoint = read_api_endpoint_from(&path).unwrap().unwrap();
        assert_eq!(endpoint.instance_id, "current-start");
    }

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

    // Hard-won: #1566: a cancelled request left its reply for the next request on a reused daemon connection.
    #[tokio::test]
    async fn cancelled_request_cannot_reuse_a_complete_or_partial_response() {
        // Cancel before the reply, within its length header, within its body,
        // and between fragments of a chunked response.
        for prefix in [0, 2, 8, usize::MAX] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let metadata = cancellation_metadata(listener.local_addr().unwrap());
            let (received, waiting) = tokio::sync::oneshot::channel();
            let (release, released) = tokio::sync::oneshot::channel();
            let server = mj_core::runtime::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let ping: RequestEnvelope = read_frame(&mut stream).await.unwrap();
                write_frame(
                    &mut stream,
                    &ResponseEnvelope {
                        protocol_version: ping.protocol_version,
                        request_id: ping.request_id,
                        result: Ok(DaemonReply::Done),
                    },
                )
                .await
                .unwrap();
                let request: RequestEnvelope = read_frame(&mut stream).await.unwrap();
                assert!(matches!(request.action, DaemonAction::Identity));
                let response = ResponseEnvelope {
                    protocol_version: request.protocol_version,
                    request_id: request.request_id,
                    result: Ok(DaemonReply::Done),
                };
                let body = serde_json::to_vec(&response).unwrap();
                let mut frame = (body.len() as u32).to_be_bytes().to_vec();
                frame.extend(body);
                let prefix = if prefix == usize::MAX {
                    write_frame(
                        &mut stream,
                        &ResponseEnvelope {
                            protocol_version: request.protocol_version,
                            request_id: request.request_id,
                            result: Ok(DaemonReply::ReplyChunk {
                                bytes: vec![b'{'],
                                finished: false,
                            }),
                        },
                    )
                    .await
                    .unwrap();
                    0
                } else {
                    stream.write_all(&frame[..prefix]).await.unwrap();
                    prefix
                };
                received.send(()).unwrap();
                released.await.unwrap();
                // Cancellation may already have closed the socket. Either result
                // is valid; the client must never consume this abandoned reply.
                let _late_reply = stream.write_all(&frame[prefix..]).await;
            });
            let mut client = DaemonClient::connect(metadata).await.unwrap();
            client.request(DaemonAction::Ping).await.unwrap();
            {
                let request = client.request_single(DaemonAction::Identity);
                tokio::pin!(request);
                tokio::select! {
                    biased;
                    result = &mut request => panic!("reply was not released: {result:?}"),
                    result = waiting => result.unwrap(),
                }
            }
            release.send(()).unwrap();
            server.await.unwrap();
            let error = client.request(DaemonAction::Ping).await.unwrap_err();
            assert!(format!("{error:#}").contains("connection lost to an interrupted request"));
            assert!(error.downcast_ref::<DaemonTransportLost>().is_some());
        }
    }

    #[tokio::test]
    async fn adoption_progress_keeps_response_identity_and_cancel_closes_its_connection() {
        for cancel in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let metadata = cancellation_metadata(listener.local_addr().unwrap());
            let (release, released) = tokio::sync::oneshot::channel();
            let server = mj_core::runtime::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request: RequestEnvelope = read_frame(&mut stream).await.unwrap();
                assert!(matches!(request.action, DaemonAction::AdoptRecovery(_)));
                write_frame(
                    &mut stream,
                    &ResponseEnvelope {
                        request_id: request.request_id,
                        protocol_version: request.protocol_version,
                        result: Ok(DaemonReply::AdoptionWaiting {
                            blockers: vec!["current turn".into()],
                            destroy_command: Some("mj recover destroy".into()),
                        }),
                    },
                )
                .await
                .unwrap();
                if cancel {
                    let mut byte = [0u8; 1];
                    assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
                } else {
                    released.await.unwrap();
                    write_frame(
                        &mut stream,
                        &ResponseEnvelope {
                            request_id: request.request_id,
                            protocol_version: request.protocol_version,
                            result: Ok(DaemonReply::AdoptionInstalled {
                                session_id: "orphan".into(),
                            }),
                        },
                    )
                    .await
                    .unwrap();
                    write_frame(
                        &mut stream,
                        &ResponseEnvelope {
                            request_id: request.request_id,
                            protocol_version: request.protocol_version,
                            result: Ok(DaemonReply::Done),
                        },
                    )
                    .await
                    .unwrap();
                }
            });
            let mut client = DaemonClient::connect(metadata).await.unwrap();
            let mut installed_notice = None;
            let (observed, mut observations) = tokio::sync::mpsc::unbounded_channel();
            {
                let work = client.adopt_recovery(
                    RecoveryAdoptionRequest {
                        session_id: "orphan".into(),
                        target_id: "podman".into(),
                        profile: None,
                        bundle: None,
                        all_instances: false,
                    },
                    |event| match event {
                        RecoveryAdoptionProgress::Waiting {
                            blockers,
                            destroy_command,
                        } => observed.send((blockers, destroy_command)).unwrap(),
                        RecoveryAdoptionProgress::Installed { session_id } => {
                            installed_notice = Some(session_id);
                        }
                    },
                );
                tokio::pin!(work);
                tokio::select! {
                    result = &mut work => panic!("adoption ended before its boundary: {result:?}"),
                    message = observations.recv() => assert_eq!(message.unwrap(), (vec!["current turn".to_owned()], Some("mj recover destroy".to_owned()))),
                }
                if !cancel {
                    release.send(()).unwrap();
                    work.await.unwrap();
                }
            }
            assert_eq!(installed_notice.as_deref(), (!cancel).then_some("orphan"));
            assert_eq!(client.stream.is_none(), cancel);
            server.await.unwrap();
        }
    }

    fn cancellation_metadata(address: std::net::SocketAddr) -> DaemonMetadata {
        DaemonMetadata {
            protocol_version: PROTOCOL_VERSION,
            pid: std::process::id(),
            address,
            token: "cancelled-exchange".into(),
            started_at: "test".into(),
            build_version: env!("CARGO_PKG_VERSION").into(),
            instance_id: String::new(),
            placement: DaemonPlacement::Host,
        }
    }

    /// Only a connection that dropped after the request was sent is marked as
    /// a lost transport; a daemon's answer, even an error, is not.
    #[tokio::test]
    async fn only_a_connection_dropped_after_the_request_is_marked_as_lost() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let metadata = DaemonMetadata {
            protocol_version: PROTOCOL_VERSION,
            pid: std::process::id(),
            address: listener.local_addr().unwrap(),
            token: "transport-lost-test".into(),
            started_at: "test".into(),
            build_version: env!("CARGO_PKG_VERSION").into(),
            instance_id: String::new(),
            placement: DaemonPlacement::Host,
        };
        let server = mj_core::runtime::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request: RequestEnvelope = read_frame(&mut stream).await.unwrap();
            write_frame(
                &mut stream,
                &ResponseEnvelope {
                    protocol_version: request.protocol_version,
                    request_id: request.request_id,
                    result: Err("create failed: clone refused".to_owned()),
                },
            )
            .await
            .unwrap();
            // The second request is read and then the connection is dropped.
            let _: RequestEnvelope = read_frame(&mut stream).await.unwrap();
        });
        let mut client = DaemonClient::connect(metadata).await.unwrap();
        let refused = client
            .wait_create_session("session-1".into())
            .await
            .unwrap_err();
        assert!(refused.downcast_ref::<DaemonRefusal>().is_some());
        assert!(refused.downcast_ref::<DaemonTransportLost>().is_none());
        let dropped = client
            .wait_create_session("session-1".into())
            .await
            .unwrap_err();
        assert!(dropped.downcast_ref::<DaemonTransportLost>().is_some());
        assert!(dropped.downcast_ref::<DaemonRefusal>().is_none());
        server.await.unwrap();
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
        let server = mj_core::runtime::spawn(async move {
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
        let server = mj_core::runtime::spawn(async move {
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
        let action = DaemonAction::SaveSpinnerStyle {
            style: Default::default(),
        };
        let expected = serde_json::to_value(&action).unwrap();
        let server = mj_core::runtime::spawn(async move {
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
}
