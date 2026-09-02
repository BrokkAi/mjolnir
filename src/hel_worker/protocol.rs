//! Wire protocol for the durable ACP relay: request/response envelopes,
//! error shapes, and newline-delimited JSON framing. This module is pure
//! serde plus byte-oriented framing; it has no filesystem or state-machine
//! concerns of its own.

use std::io::{BufRead, Write};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::hel_elicitation::ElicitationResponse;
use crate::hel_project_memory::ProjectMemorySnapshot;

use super::DurableRelay;
use super::journal::read_bounded_line;
use super::snapshot::{RelayCommand, RelayEvent, RelayOperationalState};
use super::{MAX_FRAME_BYTES, RELAY_MIN_PROTOCOL_VERSION, RELAY_PROTOCOL_VERSION};

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
        snapshot: ProjectMemorySnapshot,
    },
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
    /// `hel_skills` archive. The destination directories are fixed by the
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
    /// Run a prompt in a disposable ACP session and return its text. The
    /// runtime answers this on the connection: a scratch prompt is not session
    /// history, so it never reaches the durable relay, its journal, or its
    /// command ledger.
    Compact {
        prompt: String,
    },
    /// Resolve one in-flight form without journaling its answer.
    RespondElicitation {
        elicitation_id: String,
        response: ElicitationResponse,
    },
    /// Drive the second-opinion reviewer that runs beside this session.
    ///
    /// The reviewer is a sidecar, not a session: it shares this worker's
    /// target and working directory and owns nothing else. Its own durable
    /// relay answers the attach, acknowledge, submit and status requests
    /// nested here, so the reviewer's conversation is journaled and replayed
    /// the same way the primary's is.
    Reviewer {
        /// Which reviewing agent this is for. Absent means the default role,
        /// which is the one plan review uses; a turn review in the extended
        /// tier also names its supervisor, its intent analyst, and each
        /// specialist lane. An older controller sends no role, and an older
        /// worker ignores one, so the field is additive in both directions.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<String>,
        request: ReviewerRequest,
    },
}

/// What a controller asks of the second-opinion reviewer sidecar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", content = "params", rename_all = "snake_case")]
pub enum ReviewerRequest {
    /// Start the reviewer, or report the running one when `config` matches it.
    /// The reviewer's profile must already be staged under the worker root.
    Start {
        config: Box<crate::hel_worker_launch::ReviewerLaunchConfig>,
    },
    /// Replay the reviewer's journal from a cursor, as `Attach` does for the
    /// primary.
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
    /// Answer a form the reviewer's harness is waiting on.
    ///
    /// A reviewer that asks for permission and is never answered stalls the
    /// whole review, so its forms travel the same connection-only path the
    /// primary's do.
    RespondElicitation {
        elicitation_id: String,
        response: ElicitationResponse,
    },
    /// Cancel any turn in flight and stop the reviewer's process group,
    /// keeping its staged profile, native session and journal for next time.
    Pause,
    /// Report what changed in every workspace repository since `baselines`.
    ///
    /// A baseline is a Git tree id recorded by an earlier capture, keyed by
    /// repository root. A repository with no baseline -- or one whose baseline
    /// tree the repository no longer holds, as after a resume onto a fresh
    /// target -- reports no changes and takes the capture as its new baseline:
    /// coverage starts there rather than presenting the whole repository as
    /// this turn's work. Capture never touches the repository's index or
    /// working tree.
    CaptureDelta {
        baselines: std::collections::BTreeMap<std::path::PathBuf, String>,
    },
    /// Record `trees` as the new review baselines, pinning each so a later
    /// `git gc` cannot collect it.
    AdvanceBaseline {
        trees: std::collections::BTreeMap<std::path::PathBuf, String>,
    },
    /// Run Bifrost's one-shot semantic diff analysis over captured trees and
    /// return the changed-callable packet the review prompts embed.
    AnalyzeDelta {
        repositories: Vec<AnalyzeDeltaRepository>,
    },
    /// Collect the specialist lanes the review supervisor asked for through
    /// its MCP tool since the last time the controller asked. This request is
    /// answered by the sidecar itself rather than by any one role.
    TakeLaneDispatches,
}

/// One repository's captured endpoints for the Bifrost analysis pre-pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalyzeDeltaRepository {
    pub root: std::path::PathBuf,
    /// Absent for a repository with no recorded baseline, which the worker
    /// resolves to that repository's empty tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_tree: Option<String>,
    pub current_tree: String,
}

/// What one repository contributed to a cumulative review delta.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoDelta {
    pub root: std::path::PathBuf,
    pub baseline_tree: Option<String>,
    pub current_tree: String,
    /// Unified diff, bounded worker-side; an empty patch means this repository
    /// has nothing to review.
    pub patch: String,
    /// Human-readable file and line totals, computed from the untruncated
    /// patch so bounding cannot make a change look smaller than it is.
    pub diffstat: String,
    pub changed_lines: usize,
}

impl ReviewerRequest {
    pub const fn action_name(&self) -> &'static str {
        match self {
            Self::Start { .. } => "reviewer_start",
            Self::Attach { .. } => "reviewer_attach",
            Self::Acknowledge { .. } => "reviewer_acknowledge",
            Self::Submit { .. } => "reviewer_submit",
            Self::Status => "reviewer_status",
            Self::RespondElicitation { .. } => "reviewer_respond_elicitation",
            Self::Pause => "reviewer_pause",
            Self::CaptureDelta { .. } => "reviewer_capture_delta",
            Self::AdvanceBaseline { .. } => "reviewer_advance_baseline",
            Self::AnalyzeDelta { .. } => "reviewer_analyze_delta",
            Self::TakeLaneDispatches => "reviewer_take_lane_dispatches",
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
            Self::InstallPromptContext { .. } => "install_prompt_context",
            Self::ProjectMemorySnapshot => "project_memory_snapshot",
            Self::InstallProjectMemorySnapshot { .. } => "install_project_memory_snapshot",
            Self::CredentialState => "credential_state",
            Self::ReadCredentials => "read_credentials",
            Self::InstallCredentials { .. } => "install_credentials",
            Self::SkillsState => "skills_state",
            Self::InstallSkills { .. } => "install_skills",
            Self::GithubTokenState => "github_token_state",
            Self::InstallGithubToken { .. } => "install_github_token",
            Self::RemoveGithubToken => "remove_github_token",
            Self::Compact { .. } => "compact",
            Self::RespondElicitation { .. } => "respond_elicitation",
            Self::Reviewer { request, .. } => request.action_name(),
        }
    }

    /// Oldest protocol that understands this method or command payload. Form
    /// answers landed in protocol 2, hidden context in 3, project-memory sync
    /// in 4, user shell commands in 5, and the reviewer sidecar in 6.
    pub const fn minimum_protocol(&self) -> u32 {
        match self {
            Self::RespondElicitation { .. } => 2,
            Self::InstallPromptContext { .. } => 3,
            Self::ProjectMemorySnapshot | Self::InstallProjectMemorySnapshot { .. } => 4,
            Self::Submit { command, .. } => command.minimum_protocol(),
            Self::Reviewer { .. } => 6,
            _ => RELAY_MIN_PROTOCOL_VERSION,
        }
    }

    pub const fn supported_at(&self, protocol_version: u32) -> bool {
        RelayVersionRange::CURRENT.contains(protocol_version)
            && protocol_version >= self.minimum_protocol()
    }
}

pub(crate) fn incompatible_request_protocol(protocol_version: u32) -> RelayResponseBody {
    relay_error(
        RelayErrorCode::IncompatibleProtocol,
        format!(
            "request uses protocol {protocol_version}, relay supports protocol {}-{}",
            RELAY_MIN_PROTOCOL_VERSION, RELAY_PROTOCOL_VERSION
        ),
        false,
        None,
    )
}

pub fn incompatible_request_protocol_response(
    request_id: String,
    protocol_version: u32,
) -> RelayResponseEnvelope {
    RelayResponseEnvelope {
        request_id,
        protocol_version,
        body: incompatible_request_protocol(protocol_version),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayRequestEnvelope {
    pub request_id: String,
    pub protocol_version: u32,
    pub request: RelayRequest,
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
    Hello {
        negotiated: u32,
        relay_version: String,
        session_id: String,
        /// Content address of the worker executable that answered. The crate
        /// version cannot tell two builds apart, so this is what a controller
        /// compares against the binary it would install. Absent from a worker
        /// built before the field existed, which counts as outdated.
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
    PromptContextInstalled,
    ProjectMemorySnapshot {
        baseline: ProjectMemorySnapshot,
        replica: ProjectMemorySnapshot,
    },
    ProjectMemorySnapshotInstalled,
    /// Fingerprint and freshness of a session's harness credentials. Neither
    /// value is secret.
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
    /// Agent text from a disposable ACP compaction session.
    Compacted {
        text: String,
    },
    ElicitationResolved {
        elicitation_id: String,
    },
    /// The reviewer sidecar is running under the requested configuration.
    ReviewerStarted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        native_session_id: Option<String>,
        /// What the reviewer's harness advertises right now, which is what the
        /// waterfall offers the user.
        config_options: Vec<agent_client_protocol::schema::v1::SessionConfigOption>,
        /// Whether this call reused an already-running reviewer.
        reused: bool,
        state: Box<RelayOperationalState>,
    },
    /// The reviewer's process group has been stopped; its files remain.
    ReviewerPaused,
    /// What every workspace repository changed since the stored baselines.
    ReviewDelta {
        repositories: Vec<RepoDelta>,
    },
    /// The review baselines now name the trees the controller sent.
    ReviewBaselineAdvanced,
    /// Bifrost's changed-callable packet for the captured trees.
    ReviewChangedFunctions {
        packet: String,
    },
    /// Specialist lanes the review supervisor asked for.
    LaneDispatches {
        requests: Vec<crate::hel_review::lanes::ReviewSubagentRequest>,
    },
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

pub(crate) fn relay_protocol_error(
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

pub(crate) fn relay_error(
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

pub fn serve_relay_json_lines(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    relay: &mut DurableRelay,
) -> Result<()> {
    while let Some(request) = read_relay_frame(reader)? {
        let response = relay.handle(request);
        write_relay_frame(writer, &response)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hel_worker::test_support::*;

    #[test]
    fn relay_has_a_hard_protocol_v1_floor() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let response = relay.handle(RelayRequestEnvelope {
            request_id: "hello-old".into(),
            protocol_version: 0,
            request: RelayRequest::Hello {
                controller_version: "old".into(),
                supported: RelayVersionRange { min: 0, max: 0 },
            },
        });
        assert!(matches!(
            response.body,
            RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::IncompatibleProtocol,
                    retryable: false,
                    ..
                }
            }
        ));
    }

    #[test]
    fn current_range_overlaps_protocol_v1() {
        let v1 = RelayVersionRange { min: 1, max: 1 };
        let v2 = RelayVersionRange { min: 2, max: 2 };
        assert_eq!(RelayVersionRange::CURRENT.negotiate(v1), Some(1));
        assert_eq!(v1.negotiate(RelayVersionRange::CURRENT), Some(1));
        assert_eq!(
            RelayVersionRange::CURRENT.negotiate(RelayVersionRange::CURRENT),
            Some(RELAY_PROTOCOL_VERSION)
        );
        assert_eq!(v1.negotiate(v2), None);
        assert!(RelayVersionRange::CURRENT.contains(1));
        assert!(RelayVersionRange::CURRENT.contains(2));
        assert!(RelayVersionRange::CURRENT.contains(3));
        assert!(RelayVersionRange::CURRENT.contains(4));
        assert!(!RelayVersionRange::CURRENT.contains(0));
        assert!(!RelayVersionRange::CURRENT.contains(RELAY_PROTOCOL_VERSION + 1));
        assert!(RelayRequest::Status.supported_at(1));
        assert!(
            !RelayRequest::RespondElicitation {
                elicitation_id: String::new(),
                response: crate::hel_elicitation::ElicitationResponse::Cancel,
            }
            .supported_at(1)
        );
    }

    #[test]
    fn hello_from_protocol_v1_controller_negotiates_v1() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let response = relay.handle(RelayRequestEnvelope {
            request_id: "hello-v1".into(),
            protocol_version: 1,
            request: RelayRequest::Hello {
                controller_version: "old".into(),
                supported: RelayVersionRange { min: 1, max: 1 },
            },
        });
        assert_eq!(response.protocol_version, 1);
        match response.body {
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::Hello { negotiated, .. },
            } => assert_eq!(negotiated, 1),
            other => panic!("expected a v1 hello, got {other:?}"),
        }
    }

    /// The controller decides whether to replace a worker from what hello
    /// reports, so hello has to carry the build and a worker that was never
    /// told one has to say so rather than guess.
    #[test]
    fn hello_reports_the_worker_build_when_the_worker_knows_it() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let hello = |relay: &mut DurableRelay| {
            let response = relay.handle(RelayRequestEnvelope {
                request_id: "hello-build".into(),
                protocol_version: RELAY_PROTOCOL_VERSION,
                request: RelayRequest::Hello {
                    controller_version: "current".into(),
                    supported: RelayVersionRange::CURRENT,
                },
            });
            match response.body {
                RelayResponseBody::Ok {
                    payload: RelayResponsePayload::Hello { worker_build, .. },
                } => worker_build,
                other => panic!("expected a hello, got {other:?}"),
            }
        };
        assert_eq!(hello(&mut relay), None);

        relay.set_worker_build(Some("a".repeat(64)));
        assert_eq!(hello(&mut relay), Some("a".repeat(64)));
    }

    /// A worker built before the field existed answers hello without it. That
    /// hello must still parse, reporting no build.
    #[test]
    fn a_hello_without_a_worker_build_parses() {
        let payload: RelayResponsePayload = serde_json::from_value(serde_json::json!({
            "type": "hello",
            "data": {
                "negotiated": 1,
                "relay_version": "2.0.0",
                "session_id": SESSION,
            },
        }))
        .expect("an old worker's hello must still parse");
        assert!(matches!(
            payload,
            RelayResponsePayload::Hello {
                worker_build: None,
                ..
            }
        ));
    }

    #[test]
    fn protocol_v1_status_is_accepted() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let response = relay.handle(RelayRequestEnvelope {
            request_id: "status-v1".into(),
            protocol_version: 1,
            request: RelayRequest::Status,
        });
        assert_eq!(response.protocol_version, 1);
        assert!(matches!(
            response.body,
            RelayResponseBody::Ok {
                payload: RelayResponsePayload::Status(_)
            }
        ));
    }

    #[test]
    fn protocol_v1_cannot_respond_to_elicitation_on_the_durable_relay() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), SESSION, "1.0.0").unwrap();
        let response = relay.handle(RelayRequestEnvelope {
            request_id: "elicit-v1".into(),
            protocol_version: 1,
            request: RelayRequest::RespondElicitation {
                elicitation_id: "form-1".into(),
                response: crate::hel_elicitation::ElicitationResponse::Cancel,
            },
        });
        assert!(matches!(
            response.body,
            RelayResponseBody::Error {
                error: RelayProtocolError {
                    code: RelayErrorCode::IncompatibleProtocol,
                    retryable: false,
                    ..
                }
            }
        ));
    }
}
