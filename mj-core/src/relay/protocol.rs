//! Wire protocol for the durable ACP relay: request/response envelopes,
//! error shapes, and newline-delimited JSON framing. This module is pure
//! serde plus byte-oriented framing; it has no filesystem or state-machine
//! concerns of its own.

use std::io::{BufRead, Write};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize, Serializer};

use crate::elicitation::ElicitationResponse;
use crate::project_memory::{ProjectMemorySnapshot, ReplicaReplaceOutcome, TreeVersion};

use super::snapshot::{RelayCommand, RelayEvent, RelayOperationalState};
use super::{
    MAX_FRAME_BYTES, RELAY_LEGACY_MAILBOX_PROTOCOL, RELAY_MIN_PROTOCOL_VERSION,
    RELAY_PROTOCOL_VERSION,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayVersionRange {
    pub min: u32,
    pub max: u32,
}

impl RelayVersionRange {
    pub const CURRENT: Self = Self {
        min: RELAY_MIN_PROTOCOL_VERSION,
        max: RELAY_PROTOCOL_VERSION,
    };

    pub const fn contains(self, version: u32) -> bool {
        self.min <= version && version <= self.max
    }

    pub fn negotiate(self, peer: Self) -> Option<u32> {
        let minimum = self.min.max(peer.min);
        let maximum = self.max.min(peer.max);
        (minimum <= maximum).then_some(maximum)
    }
}

/// Connection-only memory tree. Plain trees retain the pre-44 wire shape.
/// Compression and expansion must run on a blocking thread; expansion has no
/// size cap because both endpoints are trusted mj processes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum ProjectMemoryWireTree {
    Plain(ProjectMemorySnapshot),
    Gzip { gzip: String },
}

impl ProjectMemoryWireTree {
    pub fn encode(tree: ProjectMemorySnapshot, protocol: u32) -> Result<Self> {
        if protocol < super::RELAY_COMPRESSED_PROJECT_MEMORY_PROTOCOL {
            return Ok(Self::Plain(tree));
        }
        use base64::Engine;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        serde_json::to_writer(&mut encoder, &tree.files)
            .context("compress project memory files")?;
        let bytes = encoder.finish().context("finish project memory gzip")?;
        Ok(Self::Gzip {
            gzip: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
    }

    pub fn decode(self) -> Result<ProjectMemorySnapshot> {
        match self {
            Self::Plain(tree) => Ok(tree),
            Self::Gzip { gzip } => {
                use base64::Engine;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(gzip)
                    .context("decode project memory base64")?;
                let decoder = flate2::read::GzDecoder::new(bytes.as_slice());
                let files =
                    serde_json::from_reader(decoder).context("decompress project memory files")?;
                Ok(ProjectMemorySnapshot { files })
            }
        }
    }
}

/// A request on the new controller-to-relay boundary. ACP payloads remain ACP
/// payloads; only durability and queue-control operations are Hel-specific.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "method",
    content = "params",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RelayRequest {
    Hello {
        controller_version: String,
        supported: RelayVersionRange,
    },
    Attach {
        after_ordinal: u64,
        after_digest: String,
    },
    Acknowledge {
        through_ordinal: u64,
        through_digest: String,
    },
    Submit {
        command_id: String,
        command: RelayCommand,
    },
    Status,
    /// Atomically lease pending mailbox events to a harness tool hook.
    DrainMailbox {
        hook_event: String,
    },
    /// Confirm that a hook wrote and flushed the leased events to its output.
    AckMailbox {
        lease_id: String,
    },
    /// Atomically admit a checkpoint barrier only when no work would be lost.
    /// Uses the existing barrier journal and connection-disconnect cleanup.
    ReserveIdle {
        command_id: String,
    },
    AttachmentPresent {
        reference: crate::attachment::AttachmentRef,
    },
    InstallAttachment {
        reference: crate::attachment::AttachmentRef,
        data: String,
    },
    ReadAttachment {
        reference: crate::attachment::AttachmentRef,
    },
    /// Add hidden background context attached to the next real prompt.
    /// This mutates only the relay-private snapshot and is never projected as
    /// conversation history.
    InstallPromptContext {
        text: String,
    },
    /// Read the session-private memory replica and the baseline it was seeded
    /// from. Connection-only: memory content never enters the relay journal.
    ProjectMemorySnapshot,
    /// Install a controller-reconciled tree into both the replica and its
    /// baseline for the next three-way synchronization.
    InstallProjectMemorySnapshot {
        snapshot: ProjectMemoryWireTree,
    },
    /// Replace the complete session replica and baseline only if the replica
    /// still matches the tree the controller read.
    ReplaceProjectMemoryTree {
        expected_replica: TreeVersion,
        tree: ProjectMemoryWireTree,
    },
    /// Connection-only CPU measurement; never journaled.
    CpuUsage,
    /// Report non-secret metadata for this session's harness credentials.
    /// The runtime handles credential requests on the connection and never
    /// passes them through the durable relay.
    CredentialState,
    /// Read this session's harness credential file as base64. The payload is
    /// connection-only and must never enter relay state or observations.
    ReadCredentials,
    /// Install a base64-encoded credential file into this session's harness
    /// home. The destination path is fixed by the worker launch config.
    InstallCredentials {
        data: String,
    },
    /// Report non-secret metadata for this session's synced skills trees.
    /// Handled on the connection like credential requests; the durable relay
    /// never sees them.
    SkillsState,
    /// Replace this session's synced skills trees with a base64-encoded
    /// `skills` archive. The destination directories are fixed by the
    /// worker launch config and the harness skills whitelist.
    InstallSkills {
        data: String,
    },
    /// Report whether this worker has a synchronized GitHub CLI token and its
    /// non-secret fingerprint. This request is connection-only.
    GithubTokenState,
    /// Install the controller's current GitHub CLI token into worker-private
    /// runtime storage. The token never enters durable relay state.
    InstallGithubToken {
        data: String,
    },
    /// Remove the worker's synchronized GitHub CLI token.
    RemoveGithubToken,
    /// AWS credentials stay on the control connection, outside the journal.
    AwsCredentialsState,
    InstallAwsCredentials {
        data: String,
    },
    /// Resolve one in-flight form. Only its rendered, masked reply is journaled.
    RespondElicitation {
        elicitation_id: String,
        response: ElicitationResponse,
    },
    /// Stop one task from the current process-local background-work level.
    /// The opaque id must come from `RelayOperationalState.background_commands`.
    StopBackgroundTask {
        background_task_id: String,
    },
    /// Fetch controller work queued by the parent session's private MCP
    /// socket. Connection-only: request payloads do not enter chat history.
    SubagentRequests,
    /// Acknowledge the previous page and fetch bounded changes on this connection.
    PollSubagentQueue {
        acknowledged: Option<u64>,
    },
    /// Connection-only operator-away control; never journaled as a command.
    SetOperatorAway {
        away: bool,
        revision: u64,
    },
    /// Open or close worker-owned admission for requests that change child
    /// state. Closing is serialized with the private MCP queue's enqueue lock.
    SetSubagentAdmission {
        open: bool,
    },
    /// Connection-only history queries; never part of the durable transcript.
    HistoryQuery {
        query: crate::history::HistoryQuery,
    },
    HistoryRequests,
    CompleteHistoryRequest {
        result: crate::history::HistoryResult,
    },
    /// Acknowledge a completed MCP request and cache its bounded answer for
    /// subsequent tool calls and restart recovery.
    CompleteSubagentRequest {
        result: crate::subagent::SubagentToolResult,
    },
    /// Capture repository changes or advance their review baseline.
    Reviewer {
        /// Legacy routing field retained for older workers; capture uses no role.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<String>,
        request: ReviewerRequest,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureScope {
    #[default]
    TurnDelta,
    PushableCommits,
}

/// Worker-owned repository operations used by the review host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", content = "params", rename_all = "snake_case")]
pub enum ReviewerRequest {
    /// Report what changed in every workspace repository since `baselines`.
    ///
    /// A baseline is the `current_tree` id an earlier capture reported, keyed
    /// by repository root: a Git tag naming the captured tree and the HEAD it
    /// was taken at, or a bare tree id from an earlier worker. Either is a
    /// tree-ish. When that baseline is unavailable, the worker uses the
    /// baseline pinned before its primary harness started. If neither tree is
    /// available, coverage starts at this capture rather than presenting the
    /// whole repository as this turn's work. Capture never touches the
    /// repository's index or working tree.
    CaptureDelta {
        #[serde(default)]
        scope: CaptureScope,
        baselines: std::collections::BTreeMap<std::path::PathBuf, String>,
    },
    /// Record `trees`, the `current_tree` ids of a reviewed capture, as the new
    /// review baselines, pinning each so a later `git gc` cannot collect it.
    RecordReviewed {
        trees: std::collections::BTreeMap<std::path::PathBuf, (String, String)>,
    },
    AdvanceBaseline {
        trees: std::collections::BTreeMap<std::path::PathBuf, String>,
    },
}

/// What one repository contributed to a cumulative review delta.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoDelta {
    pub root: std::path::PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_kind: Option<String>,
    /// The tree-ish `patch` is diffed from: the baseline, with every change
    /// the turn did not write already in it (see `merged_in`). `None` when
    /// there is no usable baseline and coverage starts at this capture.
    pub baseline_tree: Option<String>,
    /// The capture, which a completed review records as the next baseline: a
    /// Git tag naming the captured tree and the HEAD it was taken at, or a
    /// bare tree id from an earlier worker. Either is a tree-ish.
    pub current_tree: String,
    /// Unified diff, bounded worker-side; an empty patch means this repository
    /// has nothing to review.
    pub patch: String,
    /// Human-readable file and line totals, computed from the untruncated
    /// patch so bounding cannot make a change look smaller than it is.
    pub diffstat: String,
    pub changed_lines: usize,
    /// Lines added and removed in each changed file, from `git diff --numstat`
    /// between the baseline and the captured tree. Like `diffstat` it is
    /// computed from the whole change, so it lists every file even when
    /// `patch` was cut short. Empty when there is no usable baseline, and from
    /// a worker that predates it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileLineChange>,
    /// Commits of other history the turn merged, pulled or switched to. Their
    /// changes are part of `baseline_tree`, so `patch` holds only the turn's
    /// own commits, how it resolved its merges, and its uncommitted edits.
    /// Empty from a worker that predates it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub merged_in: Vec<String>,
    /// Why the worker could not tell the turn's commits from other history,
    /// so `patch` may hold changes the turn pulled, merged or switched to.
    /// `None` when it could, and from a worker that predates it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unattributed: Option<String>,
}

/// One changed file's line counts in a review capture.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileLineChange {
    /// The file's path in the captured tree, relative to the repository root.
    pub path: String,
    /// The path it was renamed or copied from, when Git detected one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub insertions: usize,
    pub deletions: usize,
    /// Git reports no line counts for a binary file.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub binary: bool,
}

impl ReviewerRequest {
    pub const fn action_name(&self) -> &'static str {
        match self {
            Self::CaptureDelta { .. } => "reviewer_capture_delta",
            Self::AdvanceBaseline { .. } => "reviewer_advance_baseline",
            Self::RecordReviewed { .. } => "reviewer_record_reviewed",
        }
    }
}

impl RelayRequest {
    pub const fn method_name(&self) -> &'static str {
        match self {
            Self::Hello { .. } => "hello",
            Self::Attach { .. } => "attach",
            Self::Acknowledge { .. } => "acknowledge",
            Self::Submit { .. } => "submit",
            Self::Status => "status",
            Self::DrainMailbox { .. } => "drain_mailbox",
            Self::AckMailbox { .. } => "ack_mailbox",
            Self::ReserveIdle { .. } => "reserve_idle",
            Self::InstallPromptContext { .. } => "install_prompt_context",
            Self::ProjectMemorySnapshot => "project_memory_snapshot",
            Self::InstallProjectMemorySnapshot { .. } => "install_project_memory_snapshot",
            Self::ReplaceProjectMemoryTree { .. } => "replace_project_memory_tree",
            Self::AttachmentPresent { .. } => "attachment_present",
            Self::InstallAttachment { .. } => "install_attachment",
            Self::ReadAttachment { .. } => "read_attachment",
            Self::CpuUsage => "cpu_usage",
            Self::CredentialState => "credential_state",
            Self::ReadCredentials => "read_credentials",
            Self::InstallCredentials { .. } => "install_credentials",
            Self::SkillsState => "skills_state",
            Self::InstallSkills { .. } => "install_skills",
            Self::GithubTokenState => "github_token_state",
            Self::InstallGithubToken { .. } => "install_github_token",
            Self::RemoveGithubToken => "remove_github_token",
            Self::AwsCredentialsState => "aws_credentials_state",
            Self::InstallAwsCredentials { .. } => "install_aws_credentials",
            Self::RespondElicitation { .. } => "respond_elicitation",
            Self::StopBackgroundTask { .. } => "stop_background_task",
            Self::SubagentRequests => "subagent_requests",
            Self::PollSubagentQueue { .. } => "poll_subagent_queue",
            Self::SetSubagentAdmission { .. } => "set_subagent_admission",
            Self::SetOperatorAway { .. } => "set_operator_away",
            Self::HistoryQuery { .. } => "history_query",
            Self::HistoryRequests => "history_requests",
            Self::CompleteHistoryRequest { .. } => "complete_history_request",
            Self::CompleteSubagentRequest { .. } => "complete_subagent_request",
            Self::Reviewer { request, .. } => request.action_name(),
        }
    }

    /// Oldest protocol that understands this method or command payload. Form
    /// answers landed in protocol 2, hidden context in 3, project-memory sync
    /// in 4, user shell commands in 5, the reviewer sidecar in 6, the
    /// non-steering turn cancellation in 7, and project-memory replacement in
    /// protocol 31.
    pub fn minimum_protocol(&self) -> u32 {
        match self {
            Self::AwsCredentialsState | Self::InstallAwsCredentials { .. } => {
                super::RELAY_AWS_CREDENTIALS_PROTOCOL
            }
            Self::CpuUsage => super::RELAY_CPU_USAGE_PROTOCOL,
            Self::DrainMailbox { .. } | Self::AckMailbox { .. } => 33,
            Self::ReserveIdle { .. } => 21,
            Self::HistoryQuery { .. }
            | Self::HistoryRequests
            | Self::CompleteHistoryRequest { .. } => 16,
            Self::AttachmentPresent { .. }
            | Self::InstallAttachment { .. }
            | Self::ReadAttachment { .. } => 8,
            Self::StopBackgroundTask { .. } => 9,
            Self::SubagentRequests | Self::CompleteSubagentRequest { .. } => 12,
            Self::PollSubagentQueue { .. } => super::RELAY_SUBAGENT_FEED_PROTOCOL,
            Self::SetSubagentAdmission { .. } => 32,
            Self::SetOperatorAway { .. } => super::RELAY_OPERATOR_AWAY_PROTOCOL,
            Self::RespondElicitation { .. } => 2,
            Self::InstallPromptContext { .. } => 3,
            Self::ProjectMemorySnapshot | Self::InstallProjectMemorySnapshot { .. } => 4,
            Self::ReplaceProjectMemoryTree { .. } => super::RELAY_PROJECT_MEMORY_REPLACE_PROTOCOL,
            Self::Submit { command, .. } => command.minimum_protocol(),
            Self::Reviewer {
                request:
                    ReviewerRequest::RecordReviewed { .. }
                    | ReviewerRequest::CaptureDelta {
                        scope: CaptureScope::PushableCommits,
                        ..
                    },
                ..
            } => super::RELAY_PUSH_REVIEW_PROTOCOL,

            Self::Reviewer { .. } => 6,
            _ => RELAY_MIN_PROTOCOL_VERSION,
        }
    }

    pub fn supported_at(&self, protocol_version: u32) -> bool {
        RelayVersionRange::CURRENT.contains(protocol_version)
            && protocol_version >= self.minimum_protocol()
    }
}

/// The worker-side protocol rule, applied once where requests arrive: a relay
/// serves only [`RELAY_PROTOCOL_VERSION`]. `Hello` is exempt because it is how
/// the two sides discover each other's versions.
pub fn relay_protocol_rejection(envelope: &RelayRequestEnvelope) -> Option<RelayResponseBody> {
    (!matches!(envelope.request, RelayRequest::Hello { .. })
        && envelope.protocol_version != RELAY_PROTOCOL_VERSION)
        .then(|| incompatible_request_protocol(envelope.protocol_version))
}

pub fn incompatible_request_protocol(protocol_version: u32) -> RelayResponseBody {
    relay_error(
        RelayErrorCode::IncompatibleProtocol,
        format!(
            "request uses protocol {protocol_version}, relay supports protocol {RELAY_PROTOCOL_VERSION}"
        ),
        false,
        None,
    )
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayRequestEnvelope {
    pub request_id: String,
    pub protocol_version: u32,
    pub request: RelayRequest,
}

impl Serialize for RelayRequestEnvelope {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut request = serde_json::to_value(&self.request).map_err(serde::ser::Error::custom)?;
        if self.protocol_version == RELAY_LEGACY_MAILBOX_PROTOCOL {
            rewrite_protocol_33_mailbox_commands(&mut request)
                .map_err(serde::ser::Error::custom)?;
        }
        #[derive(Serialize)]
        struct Envelope<'a> {
            request_id: &'a str,
            protocol_version: u32,
            request: serde_json::Value,
        }
        Envelope {
            request_id: &self.request_id,
            protocol_version: self.protocol_version,
            request,
        }
        .serialize(serializer)
    }
}

fn rewrite_protocol_33_mailbox_commands(request: &mut serde_json::Value) -> serde_json::Result<()> {
    let command = match request["method"].as_str() {
        Some("submit") => Some(&mut request["params"]["command"]),
        Some("reviewer") if request["params"]["request"]["action"] == "submit" => {
            Some(&mut request["params"]["request"]["params"]["command"])
        }
        _ => None,
    };
    if let Some(command) = command {
        let command_type = command["type"].as_str().map(str::to_owned);
        match command_type.as_deref() {
            Some("deliver_mailbox_event") => {
                rewrite_protocol_33_event(&mut command["data"]["event"])?;
            }
            Some("mailbox_wake") => {
                if let Some(events) = command["data"]["events"].as_array_mut() {
                    for event in events {
                        rewrite_protocol_33_event(event)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn rewrite_protocol_33_event(value: &mut serde_json::Value) -> serde_json::Result<()> {
    let event: crate::mailbox::MailboxEvent = serde_json::from_value(value.clone())?;
    *value = serde_json::to_value(event.legacy_representation())?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelayResponseEnvelope {
    pub request_id: String,
    pub protocol_version: u32,
    #[serde(flatten)]
    pub body: RelayResponseBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
// This is a short-lived wire DTO. Boxing every successful response would add
// an allocation without reducing retained relay state.
#[allow(clippy::large_enum_variant)]
pub enum RelayResponseBody {
    Ok { payload: RelayResponsePayload },
    Error { error: RelayProtocolError },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RelayResponsePayload {
    /// `None` means work won the race; no barrier or other mutation occurred.
    IdleReservation {
        ordinal: Option<u64>,
    },
    Hello {
        negotiated: u32,
        relay_version: String,
        session_id: String,
        /// Worker-input compatibility identity of the worker that answered.
        /// Absent from a worker built before the field existed, which counts
        /// as outdated.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worker_build: Option<String>,
    },
    Attached {
        state: RelayOperationalState,
        events: Vec<RelayEvent>,
        through_ordinal: u64,
        through_digest: String,
    },
    Acknowledged {
        through_ordinal: u64,
        through_digest: String,
    },
    Accepted {
        command_id: String,
        ordinal: u64,
    },
    Status(RelayOperationalState),
    MailboxDrained {
        /// Present when mailbox events or a Claude boundary steer are leased.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lease_id: Option<String>,
        text: Option<String>,
        /// Number of context items leased to the hook.
        count: usize,
    },
    MailboxAcknowledged {
        acknowledged: bool,
    },
    AttachmentPresent {
        present: bool,
    },
    AttachmentInstalled,
    AttachmentData {
        data: String,
    },
    PromptContextInstalled,
    ProjectMemorySnapshot {
        baseline: ProjectMemoryWireTree,
        replica: ProjectMemoryWireTree,
    },
    ProjectMemorySnapshotInstalled,
    ProjectMemoryTreeReplaced {
        outcome: ReplicaReplaceOutcome,
    },
    /// Fingerprint and freshness of a session's harness credentials. Neither
    /// value is secret.
    CpuUsage {
        usage: Option<crate::cpu_usage::SessionCpuUsage>,
    },
    CredentialState {
        present: bool,
        fingerprint: String,
        freshness_epoch_ms: Option<i64>,
    },
    /// Base64 of a session's credential file. Sent only on the connection
    /// socket, never recorded.
    Credentials {
        data: String,
    },
    /// Fingerprint of a session's synced skills trees. Not secret.
    SkillsState {
        present: bool,
        fingerprint: String,
    },
    /// Presence and fingerprint of the worker-private GitHub CLI token.
    GithubTokenState {
        present: bool,
        fingerprint: String,
    },
    ElicitationResolved {
        elicitation_id: String,
    },
    BackgroundTaskStopRequested {
        background_task_id: String,
    },
    SubagentRequests {
        requests: Vec<crate::subagent::SubagentToolRequest>,
        results: Vec<crate::subagent::SubagentToolResult>,
    },
    SubagentQueue {
        page: crate::subagent::SubagentQueuePage,
    },
    SubagentAdmissionChanged {
        open: bool,
    },
    SubagentRequestCompleted,
    /// Newer peers report whether the result reached a live tool caller. The
    /// original unit response remains valid and is treated as delivered.
    SubagentRequestCompletedWithDelivery {
        delivered_to_waiter: bool,
    },
    HistoryRequests {
        requests: Vec<crate::history::HistoryRequest>,
    },
    HistoryResult {
        result: crate::history::HistoryResult,
    },
    HistoryRequestCompleted,
    /// What every workspace repository changed since the stored baselines.
    ReviewDelta {
        repositories: Vec<RepoDelta>,
    },
    /// The review baselines now name the trees the controller sent.
    ReviewBaselineAdvanced,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayProtocolError {
    pub code: RelayErrorCode,
    pub message: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<RelayErrorDetail>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayErrorCode {
    IncompatibleProtocol,
    InvalidRequest,
    InvalidState,
    Desynchronized,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RelayErrorDetail {
    Desynchronized {
        requested_after: u64,
        requested_digest: String,
        earliest_available: u64,
        earliest_digest: String,
        latest: u64,
        latest_digest: String,
    },
}

pub fn relay_protocol_error(
    code: RelayErrorCode,
    message: impl Into<String>,
    retryable: bool,
    detail: Option<RelayErrorDetail>,
) -> RelayProtocolError {
    RelayProtocolError {
        code,
        message: message.into(),
        retryable,
        detail,
    }
}

pub fn relay_error(
    code: RelayErrorCode,
    message: impl Into<String>,
    retryable: bool,
    detail: Option<RelayErrorDetail>,
) -> RelayResponseBody {
    RelayResponseBody::Error {
        error: relay_protocol_error(code, message, retryable, detail),
    }
}

pub fn unsupported_relay_method_response(
    request_id: String,
    protocol_version: u32,
    method: String,
) -> RelayResponseEnvelope {
    RelayResponseEnvelope {
        request_id,
        protocol_version,
        body: relay_error(
            RelayErrorCode::InvalidRequest,
            format!("relay does not support method {method:?}"),
            false,
            None,
        ),
    }
}

pub fn invalid_relay_request_response(
    request_id: String,
    protocol_version: u32,
    message: String,
) -> RelayResponseEnvelope {
    RelayResponseEnvelope {
        request_id,
        protocol_version,
        body: relay_error(RelayErrorCode::InvalidRequest, message, false, None),
    }
}

pub fn read_relay_frame(reader: &mut impl BufRead) -> Result<Option<RelayRequestEnvelope>> {
    let mut bytes = Vec::new();
    let (read, _) = read_bounded_line(reader, &mut bytes, MAX_FRAME_BYTES)
        .context("read relay protocol frame")?;
    if read == 0 {
        return Ok(None);
    }
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    if bytes.is_empty() {
        bail!("empty relay protocol frame");
    }
    serde_json::from_slice(&bytes)
        .context("parse relay protocol request")
        .map(Some)
}

pub fn write_relay_frame(writer: &mut impl Write, response: &RelayResponseEnvelope) -> Result<()> {
    serde_json::to_writer(&mut *writer, response)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

pub fn read_bounded_line(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
    maximum_bytes: usize,
) -> Result<(usize, bool)> {
    line.clear();
    let mut consumed_total = 0_usize;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok((consumed_total, false));
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let content_bytes = newline.unwrap_or(available.len());
        let next_len = line
            .len()
            .checked_add(content_bytes)
            .ok_or_else(|| anyhow!("relay journal line length overflow"))?;
        super::snapshot::ensure_byte_budget(next_len, maximum_bytes, "relay journal event")?;
        line.extend_from_slice(&available[..content_bytes]);
        let consumed = content_bytes + usize::from(newline.is_some());
        reader.consume(consumed);
        consumed_total = consumed_total
            .checked_add(consumed)
            .ok_or_else(|| anyhow!("relay journal length overflow"))?;
        if newline.is_some() {
            return Ok((consumed_total, true));
        }
    }
}

#[cfg(test)]
mod project_memory_wire_tests {
    use super::*;

    #[test]
    fn memory_wire_preserves_legacy_bytes_and_rejects_corrupt_compression() {
        let legacy = r#"{"files":{"/MEMORY.md":"notes\n"}}"#;
        let plain: ProjectMemoryWireTree = serde_json::from_str(legacy).unwrap();
        assert_eq!(serde_json::to_string(&plain).unwrap(), legacy);
        let tree = plain.decode().unwrap();
        let compressed = ProjectMemoryWireTree::encode(tree.clone(), 44).unwrap();
        let encoded = serde_json::to_vec(&compressed).unwrap();
        let decoded: ProjectMemoryWireTree = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.decode().unwrap(), tree);
        assert!(
            ProjectMemoryWireTree::Gzip {
                gzip: "invalid base64".into()
            }
            .decode()
            .is_err()
        );
        assert!(
            ProjectMemoryWireTree::Gzip {
                gzip: "e30=".into()
            }
            .decode()
            .is_err()
        );
        let ProjectMemoryWireTree::Gzip { mut gzip } = compressed else {
            panic!("expected gzip")
        };
        gzip.truncate(gzip.len() / 2);
        assert!(ProjectMemoryWireTree::Gzip { gzip }.decode().is_err());
    }
}
