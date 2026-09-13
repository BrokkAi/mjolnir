//! Shared checkpoint archive formats and canonical session validation.

use crate::config::HarnessKind;
use crate::transcript::is_false;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Baseline schema. Every payload occupies exactly one ZIP entry, so any build
/// that understands schema 2 can read the archive.
pub const ARCHIVE_SCHEMA_VERSION: u32 = 2;
/// Schema 2 plus sharded payloads. A payload larger than
/// [`PAYLOAD_PART_BYTES`] is written as several `*.helpart.NNNNN` ZIP entries
/// so compression and verification can run in parallel. Archives declare this
/// schema only when at least one payload is sharded, which keeps small
/// sessions readable by builds that predate sharding and makes older builds
/// reject sharded archives with an explicit version error instead of
/// misreading part entries.
pub const ARCHIVE_SCHEMA_VERSION_SHARDED: u32 = 3;
/// Separate image blobs must be restored into the relay, not the harness home.
pub const ARCHIVE_SCHEMA_VERSION_ATTACHMENTS: u32 = 4;
pub const ARCHIVE_FORMAT: &str = "hel-session";
pub const EVENT_FRONTIER_GENESIS_DIGEST: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionManifest {
    pub id: String,
    pub title: String,
    pub harness_kind: HarnessKind,
    pub profile_id: String,
    pub native_session_id: String,
    pub created_at: String,
    pub checkpointed_at: String,
    pub hel_version: String,
    pub relay_version: String,
    pub adapter_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleManifest {
    pub id: String,
    pub primary_repository: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetManifest {
    pub template_id: String,
    pub target_kind: String,
    /// Informational provenance only. It must not contain credentials.
    pub details: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryMetadata {
    pub id: String,
    pub relative_destination: PathBuf,
    pub origin: String,
    /// Explicit push destinations configured for `origin`. An empty list
    /// means Git's normal push fallback (the fetch URL), and is omitted from
    /// older archive manifests for compatibility.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub push_urls: Vec<String>,
    /// Marks a repository cloned for a managed network workspace. The marker
    /// makes a nonempty `base_commit` unambiguous: raw DeltaFrom snapshots
    /// also carry a base, but must not inherit managed push configuration.
    #[serde(default, skip_serializing_if = "is_false")]
    pub remote_workspace: bool,
    /// Immutable launch base for managed network workspaces; raw DeltaFrom
    /// snapshots record their capture base. Legacy SessionDelta snapshots
    /// exclude origin refs instead and leave this empty.
    pub base_commit: String,
    pub head_commit: String,
    pub branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryManifest {
    #[serde(flatten)]
    pub metadata: RepositoryMetadata,
    pub committed_bundle_path: String,
    pub staged_patch_path: String,
    pub unstaged_patch_path: String,
    pub untracked_tar_path: String,
}

/// Controller-owned, materialized state captured at a checkpoint barrier.
///
/// These types deliberately do not depend on the live ACP or relay types. ACP
/// content blocks and evolving tool/plan details are stored as JSON values at
/// the stable archive boundary, while the identity, ordering, and timestamps
/// needed to rebuild controller state remain explicit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalSessionSnapshot {
    /// Highest relay event ordinal incorporated into this projection.
    pub event_frontier: u64,
    /// Relay-authored rolling digest of the exact event prefix at the frontier.
    pub event_frontier_digest: String,
    pub session: CanonicalSessionState,
    pub transcript: Vec<CanonicalTranscriptItem>,
    pub queued_prompts: Vec<CanonicalQueuedPrompt>,
}

impl CanonicalSessionSnapshot {
    pub fn validate(&self) -> Result<()> {
        validate_canonical_session(self)
    }

    /// Whether two snapshots carry the same session content.
    ///
    /// The event frontier and its digest are deliberately ignored: relay
    /// bookkeeping advances them without changing what the session contains.
    /// `last_activity_at_ms` is a watermark of the same kind.
    pub fn content_matches(&self, other: &Self) -> bool {
        self.transcript == other.transcript
            && self.queued_prompts == other.queued_prompts
            && self.session.without_activity_watermark()
                == other.session.without_activity_watermark()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalSessionState {
    pub execution: CanonicalExecutionState,
    /// Monotonic controller projection watermark derived from relay events.
    pub last_activity_at_ms: Option<i64>,
    pub session_title: Option<String>,
    pub configuration: BTreeMap<String, serde_json::Value>,
}

impl CanonicalSessionState {
    /// The same state with its volatile activity watermark cleared, so two
    /// states can be compared on content alone. Cloning keeps every other
    /// field in the comparison, including fields added later.
    fn without_activity_watermark(&self) -> Self {
        Self {
            last_activity_at_ms: None,
            ..self.clone()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum CanonicalExecutionState {
    Idle,
    Running { started_at_ms: i64 },
    Closing,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalTranscriptItem {
    pub stable_id: String,
    /// Ordinal of the event that created this logical transcript item.
    pub position: u64,
    /// Ordinal of the most recent content chunk for an agent message. This is
    /// `None` for every other logical item.
    pub latest_content_event_ordinal: Option<u64>,
    pub created_at_ms: i64,
    pub last_changed_at_ms: i64,
    pub body: CanonicalTranscriptBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CanonicalTranscriptBody {
    User {
        /// ACP content blocks in their JSON representation.
        content: Vec<serde_json::Value>,
    },
    Agent {
        /// Complete ACP `ContentChunk` values.
        chunks: Vec<serde_json::Value>,
        streaming: bool,
    },
    Thought {
        /// Complete ACP `ContentChunk` values.
        chunks: Vec<serde_json::Value>,
        streaming: bool,
    },
    Tool {
        /// Complete current ACP `ToolCall` value.
        call: serde_json::Value,
        /// Output of the terminals this call's content refers to.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        terminal_outputs: Vec<CanonicalTerminalOutput>,
        /// Every terminal this call has ever referred to, including references
        /// a later content update dropped.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        terminal_refs: Vec<String>,
        /// Cached compact label data. This is a sidecar rather than ACP
        /// metadata so legacy transcript conversions preserve their summary
        /// without changing the provider-owned tool call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        presentation: Option<crate::transcript::ToolCallPresentation>,
    },
    /// Terminal output no tool call refers to.
    TerminalOutput {
        record: CanonicalTerminalOutput,
    },
    Plan {
        /// Complete current ACP `Plan` value.
        plan: serde_json::Value,
    },
    /// A plan the harness asked the user to approve, kept verbatim.
    PlanProposal {
        proposal_id: String,
        plan: String,
    },
    System {
        text: String,
    },
}

/// Archived form of one client-run terminal's output. Mirrors
/// [`crate::state::TerminalOutputRecord`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalTerminalOutput {
    pub terminal_id: String,
    pub output: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
}

/// What a queued entry does when its turn comes. Archives written before
/// configuration changes could be queued carry no `kind`, so it defaults to a
/// prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalQueuedCommandKind {
    #[default]
    Prompt,
    SetConfig {
        key: String,
        value: String,
    },
}

impl CanonicalQueuedCommandKind {
    fn is_prompt(&self) -> bool {
        matches!(self, Self::Prompt)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalQueuedPrompt {
    pub command_id: String,
    #[serde(default, skip_serializing_if = "CanonicalQueuedCommandKind::is_prompt")]
    pub kind: CanonicalQueuedCommandKind,
    /// ACP content blocks in their JSON representation. A queued configuration
    /// change carries the composer text that produced it.
    pub content: Vec<serde_json::Value>,
    pub queued_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PayloadRole {
    CanonicalSession,
    NativeArtifact { relative_path: PathBuf },
    GitBundle { repository_id: String },
    GitStagedPatch { repository_id: String },
    GitUnstagedPatch { repository_id: String },
    GitUntrackedTar { repository_id: String },
}

/// One byte range of a sharded payload, stored as its own ZIP entry.
///
/// Parts are contiguous and ordered: part `i` covers the bytes right after
/// part `i - 1`, and concatenating every part in order reproduces the payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayloadPartDescriptor {
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayloadDescriptor {
    pub path: String,
    pub sha256: String,
    pub size: u64,
    pub mode: u32,
    pub role: PayloadRole,
    /// Empty for a whole payload stored in one ZIP entry. Otherwise the
    /// ordered parts the payload was split into; `path` then names no ZIP
    /// entry of its own and `sha256`/`size` describe the reassembled payload.
    /// The field is absent from schema-2 manifests, so builds that predate
    /// sharding also reject it through `deny_unknown_fields`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<PayloadPartDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveManifest {
    pub schema_version: u32,
    pub format: String,
    pub session: SessionManifest,
    pub target: TargetManifest,
    pub bundle: BundleManifest,
    pub repositories: Vec<RepositoryManifest>,
    pub payloads: Vec<PayloadDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeArtifact {
    pub relative_path: PathBuf,
    pub data: Vec<u8>,
    pub mode: u32,
}

fn validate_canonical_session(snapshot: &CanonicalSessionSnapshot) -> Result<()> {
    ensure!(
        is_lower_hex_sha256(&snapshot.event_frontier_digest),
        "canonical event frontier digest must be 64 lowercase hexadecimal characters"
    );
    ensure!(
        (snapshot.event_frontier == 0)
            == (snapshot.event_frontier_digest == EVENT_FRONTIER_GENESIS_DIGEST),
        "canonical event frontier and digest are inconsistent"
    );
    ensure!(
        (snapshot.event_frontier == 0) == snapshot.session.last_activity_at_ms.is_none(),
        "canonical event frontier and activity watermark are inconsistent"
    );
    ensure!(
        snapshot.session.execution == CanonicalExecutionState::Idle,
        "canonical session is not idle at the checkpoint barrier"
    );
    ensure!(
        snapshot
            .session
            .session_title
            .as_ref()
            .is_none_or(|title| !title.trim().is_empty()),
        "canonical session title is empty"
    );
    let mut item_ids = BTreeSet::new();
    let mut previous_position = 0_u64;
    for item in &snapshot.transcript {
        ensure!(
            !item.stable_id.trim().is_empty(),
            "canonical transcript item id is empty"
        );
        ensure!(
            item_ids.insert(item.stable_id.as_str()),
            "duplicate canonical transcript item id '{}'",
            item.stable_id
        );
        ensure!(
            item.position > 0,
            "canonical transcript item '{}' has zero position",
            item.stable_id
        );
        ensure!(
            item.position >= previous_position,
            "canonical transcript items are out of position order"
        );
        ensure!(
            item.position <= snapshot.event_frontier,
            "canonical transcript item '{}' is beyond event frontier {}",
            item.stable_id,
            snapshot.event_frontier
        );
        match (&item.body, item.latest_content_event_ordinal) {
            (CanonicalTranscriptBody::Agent { .. }, Some(ordinal)) => ensure!(
                ordinal >= item.position && ordinal <= snapshot.event_frontier,
                "canonical agent message '{}' has invalid latest content ordinal {ordinal}",
                item.stable_id
            ),
            (CanonicalTranscriptBody::Agent { .. }, None) => bail!(
                "canonical agent message '{}' has no latest content ordinal",
                item.stable_id
            ),
            (_, Some(ordinal)) => bail!(
                "canonical non-agent transcript item '{}' has latest content ordinal {ordinal}",
                item.stable_id
            ),
            (_, None) => {}
        }
        ensure!(
            item.last_changed_at_ms >= item.created_at_ms,
            "canonical transcript item '{}' changed before it was created",
            item.stable_id
        );
        ensure!(
            !matches!(
                &item.body,
                CanonicalTranscriptBody::Agent {
                    streaming: true,
                    ..
                } | CanonicalTranscriptBody::Thought {
                    streaming: true,
                    ..
                }
            ),
            "canonical transcript item '{}' is still streaming at the checkpoint barrier",
            item.stable_id
        );
        match &item.body {
            CanonicalTranscriptBody::User { content } => {
                for (index, block) in content.iter().enumerate() {
                    serde_json::from_value::<agent_client_protocol::schema::v1::ContentBlock>(
                        block.clone(),
                    )
                    .with_context(|| {
                        format!(
                            "canonical transcript item '{}' has invalid ACP content block {index}",
                            item.stable_id
                        )
                    })?;
                }
            }
            CanonicalTranscriptBody::Agent { chunks, .. }
            | CanonicalTranscriptBody::Thought { chunks, .. } => {
                for (index, chunk) in chunks.iter().enumerate() {
                    serde_json::from_value::<agent_client_protocol::schema::v1::ContentChunk>(
                        chunk.clone(),
                    )
                    .with_context(|| {
                        format!(
                            "canonical transcript item '{}' has invalid ACP content chunk {index}",
                            item.stable_id
                        )
                    })?;
                }
            }
            CanonicalTranscriptBody::Tool {
                call,
                terminal_outputs,
                terminal_refs,
                ..
            } => {
                serde_json::from_value::<agent_client_protocol::schema::v1::ToolCall>(call.clone())
                    .with_context(|| {
                        format!(
                            "canonical transcript item '{}' has invalid ACP tool call",
                            item.stable_id
                        )
                    })?;
                for output in terminal_outputs {
                    validate_canonical_terminal_output(output, &item.stable_id)?;
                }
                for terminal_id in terminal_refs {
                    ensure!(
                        !terminal_id.trim().is_empty(),
                        "canonical transcript item '{}' refers to a terminal with an empty id",
                        item.stable_id
                    );
                }
            }
            CanonicalTranscriptBody::TerminalOutput { record } => {
                validate_canonical_terminal_output(record, &item.stable_id)?;
            }
            CanonicalTranscriptBody::Plan { plan } => {
                serde_json::from_value::<agent_client_protocol::schema::v1::Plan>(plan.clone())
                    .with_context(|| {
                        format!(
                            "canonical transcript item '{}' has invalid ACP plan",
                            item.stable_id
                        )
                    })?;
            }
            CanonicalTranscriptBody::PlanProposal { proposal_id, .. } => {
                ensure!(
                    !proposal_id.trim().is_empty(),
                    "canonical transcript item '{}' has an empty plan proposal id",
                    item.stable_id
                );
            }
            CanonicalTranscriptBody::System { .. } => {}
        }
        previous_position = item.position;
    }

    let mut queue_ids = BTreeSet::new();
    for prompt in &snapshot.queued_prompts {
        ensure!(
            !prompt.command_id.trim().is_empty(),
            "canonical queued prompt id is empty"
        );
        ensure!(
            queue_ids.insert(prompt.command_id.as_str()),
            "duplicate canonical queued prompt id '{}'",
            prompt.command_id
        );
        ensure!(
            !prompt.content.is_empty(),
            "canonical queued prompt '{}' has no content",
            prompt.command_id
        );
        if let CanonicalQueuedCommandKind::SetConfig { key, value } = &prompt.kind {
            ensure!(
                !key.trim().is_empty() && !value.trim().is_empty(),
                "canonical queued configuration change '{}' is incomplete",
                prompt.command_id
            );
        }
        for (index, content) in prompt.content.iter().enumerate() {
            serde_json::from_value::<agent_client_protocol::schema::v1::ContentBlock>(
                content.clone(),
            )
            .with_context(|| {
                format!(
                    "canonical queued prompt '{}' has invalid ACP content block {index}",
                    prompt.command_id
                )
            })?;
        }
    }
    Ok(())
}

fn validate_canonical_terminal_output(
    output: &CanonicalTerminalOutput,
    stable_id: &str,
) -> Result<()> {
    ensure!(
        !output.terminal_id.trim().is_empty(),
        "canonical transcript item '{stable_id}' has terminal output with an empty terminal id"
    );
    Ok(())
}

pub fn is_lower_hex_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}
