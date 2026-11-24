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
/// the archive writer’s part-size limit is written as several `*.helpart.NNNNN` ZIP entries
/// so compression and verification can run in parallel. Archives declare this
/// schema only when at least one payload is sharded, which keeps small
/// sessions readable by builds that predate sharding and makes older builds
/// reject sharded archives with an explicit version error instead of
/// misreading part entries.
pub const ARCHIVE_SCHEMA_VERSION_SHARDED: u32 = 3;
/// Separate image blobs must be restored into the relay, not the harness home.
pub const ARCHIVE_SCHEMA_VERSION_ATTACHMENTS: u32 = 4;
/// A durable transcript identity; text alone must never reset model history.
pub const CONTEXT_BOUNDARY_PREFIX: &str = "context-cleared:";
pub fn is_context_boundary(stable_id: &str) -> bool {
    stable_id.starts_with(CONTEXT_BOUNDARY_PREFIX)
}
pub const ARCHIVE_SCHEMA_VERSION_CONTEXT: u32 = 5;
/// Git refs and stash entries are preserved in independently owned clones.
pub const ARCHIVE_SCHEMA_VERSION_CLONE_REFS: u32 = 6;
/// Sub-agent report files, restored beside the repositories rather than into
/// the harness home. It supersedes every earlier schema: an archive that
/// carries reports declares it whatever else it carries.
pub const ARCHIVE_SCHEMA_VERSION_AGENT_REPORTS: u32 = 7;
/// A repository captured from a subdirectory of its checkout, whose untracked
/// files are named from the checkout's top level. An older build would unpack
/// them inside the subdirectory. It supersedes every earlier schema.
pub const ARCHIVE_SCHEMA_VERSION_CHECKOUT_SUBDIRECTORY: u32 = 8;
/// Typed received-message history that older canonical readers cannot decode.
pub const ARCHIVE_SCHEMA_VERSION_MESSAGES: u32 = 9;
/// Durable private Git refs, including non-commit review baselines.
pub const ARCHIVE_SCHEMA_VERSION_REVIEW_REFS: u32 = 10;
/// Linked Git worktrees inside a captured checkout, restored beside it. An
/// older build would drop them silently, so an archive that carries any
/// declares this schema whatever else it carries.
pub const ARCHIVE_SCHEMA_VERSION_LINKED_WORKTREES: u32 = 11;
/// Older readers reject the new canonical notification record.
pub const ARCHIVE_SCHEMA_VERSION_NOTIFICATION_STACK: u32 = 12;
/// Mail the worker accepted and had not delivered at the cut. Older readers
/// reject the canonical field, so an archive that carries it declares this
/// schema whatever else it carries.
pub const ARCHIVE_SCHEMA_VERSION_MAILBOX: u32 = 13;
/// An origin-relative Git pack and workspace snapshot metadata. It can carry
/// mail as well, so it comes after the mailbox schema.
pub const ARCHIVE_SCHEMA_VERSION_ORIGIN_DELTA: u32 = 14;
/// Worker-owned proof of whether the archived native session was used. Older
/// readers reject the canonical field, so an archive that carries it declares
/// this schema whatever else it carries (including origin-delta and mailbox).
pub const ARCHIVE_SCHEMA_VERSION_NATIVE_USE: u32 = 15;
/// Mjolnir's private Git refs: review baselines, review points and reviewed
/// trees. They are bookkeeping, never pushed, and some name a bare tree.
pub const MJOLNIR_REF_NAMESPACE: &str = "refs/mj/";

/// A review baseline pin: `refs/mj/review-baseline` for a main worktree, or
/// `refs/mj/worktrees/<name>/review-baseline` for a linked one. Releases that
/// reviewed each turn wrote them into the user's repositories. Nothing reads
/// their value any more, so captures and restores neither carry nor write them.
pub fn is_review_baseline_pin(name: &str) -> bool {
    name == "refs/mj/review-baseline"
        || (name.starts_with("refs/mj/worktrees/") && name.ends_with("/review-baseline"))
}
/// Shared diagnostic identity across the target export's stderr boundary.
pub const NESTED_REPOSITORY_CHECKPOINT_ERROR: &str =
    "cannot checkpoint nested Git repository directory at ";
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SnapshotHead {
    Branch { name: String, commit: String },
    Detached(String),
}
impl SnapshotHead {
    pub fn commit(&self) -> &str {
        match self {
            Self::Branch { commit, .. } | Self::Detached(commit) => commit,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SnapshotStashEntry {
    pub commit: String,
    pub message: String,
}
mod snapshot_path_serde {
    use super::*;
    use serde::{Deserializer, Serializer};
    use std::path::Path;
    pub fn serialize<S: Serializer>(
        path: &Path,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer
            .serialize_str(&super::archive_path_text(path).map_err(serde::ser::Error::custom)?)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<PathBuf, D::Error> {
        let text = String::deserialize(deserializer)?;
        let path = PathBuf::from(&text);
        let normalized = super::archive_path_text(&path).map_err(serde::de::Error::custom)?;
        if normalized != text {
            return Err(serde::de::Error::custom(
                "snapshot path is not slash-separated",
            ));
        }
        Ok(path)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SnapshotSubmodule {
    #[serde(with = "snapshot_path_serde")]
    pub path: PathBuf,
    pub commit: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SnapshotGitlink {
    #[serde(with = "snapshot_path_serde")]
    pub path: PathBuf,
    pub commit: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SnapshotWorktree {
    pub has_uncommitted_state: bool,
    #[serde(with = "snapshot_path_serde")]
    pub relative_path: PathBuf,
    pub head: SnapshotHead,
    pub working_state: Option<String>,
    pub working_state_skipped: bool,
    pub initialized_submodules: Vec<SnapshotSubmodule>,
    /// Only this checkout's index; module indexes do not travel.
    pub index_gitlinks: Vec<SnapshotGitlink>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BranchTracking {
    pub remote: Option<String>,
    pub merge: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSnapshotMetadata {
    /// Configured origin URL, redacted. None is an in-place-only raw snapshot.
    pub origin_url: Option<String>,
    pub object_format: String,
    pub notices: Vec<String>,
    pub head: SnapshotHead,
    pub refs: BTreeMap<String, String>,
    pub branch_tracking: BTreeMap<String, BranchTracking>,
    pub stash: Vec<SnapshotStashEntry>,
    pub working_state: Option<String>,
    pub working_state_skipped: bool,
    pub has_uncommitted_state: bool,
    pub worktrees: Vec<SnapshotWorktree>,
    pub initialized_submodules: Vec<SnapshotSubmodule>,
    /// Only this checkout's index; module indexes do not travel.
    pub index_gitlinks: Vec<SnapshotGitlink>,
    pub remote_refs_stale: bool,
    /// Boundary commits and saved tips omitted from the origin-relative pack.
    pub prerequisites: Vec<String>,
    /// True when captured relative to explicit history-cutoff commits rather than an origin remote.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub history_cutoff: bool,
}

pub fn archive_path_text(path: &std::path::Path) -> Result<String> {
    use std::path::Component;
    ensure!(!path.as_os_str().is_empty(), "archive path is empty");
    ensure!(
        !path.is_absolute(),
        "archive path '{}' is absolute",
        path.display()
    );
    let parts = path
        .components()
        .map(|component| {
            let Component::Normal(part) = component else {
                bail!("unsafe archive path '{}'", path.display());
            };
            part.to_str()
                .with_context(|| format!("archive path '{}' is not UTF-8", path.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(parts.join("/"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryMetadata {
    pub id: String,
    pub relative_destination: PathBuf,
    /// Where the directory at `relative_destination` sits inside its Git
    /// checkout, when it is not the checkout's top level. The snapshot still
    /// covers the whole checkout: its untracked files are named from the top
    /// level, and a restore works from the directory this path leads up to.
    /// Absent for a top-level directory, and in archives written before a
    /// subdirectory was captured this way; those carry only the
    /// subdirectory's untracked files, named from the subdirectory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_subdirectory: Option<PathBuf>,
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
    /// The directory has no committed Git checkout. A bare local session may
    /// run in a plain folder of the user's own or a repository without its
    /// first commit; nothing about such a directory is
    /// captured, and a restore must leave it alone. The field is absent from
    /// every archive written before plain folders were accepted.
    #[serde(default, skip_serializing_if = "is_false")]
    pub plain_directory: bool,
    /// Immutable launch base for managed network workspaces; raw DeltaFrom
    /// snapshots record their capture base. Legacy SessionDelta snapshots
    /// exclude origin refs instead and leave this empty. Origin-delta
    /// snapshots of a managed network workspace record its launch base so a
    /// resume keeps it; other origin-delta snapshots, and archives written
    /// before this was recorded, leave it empty.
    pub base_commit: String,
    pub head_commit: String,
    pub branch: Option<String>,
    /// Only ref names and object IDs were captured. The raw checkout owns
    /// the objects and working files; absent objects do not prevent resume.
    #[serde(default, skip_serializing_if = "is_false")]
    pub review_refs_metadata_only: bool,
    /// The source checkout's session-start scope name, used to relocate that
    /// checkout's session-start refs. Archives written by earlier releases also
    /// carried the review baseline pin under this name; restore ignores that
    /// pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_baseline_ref: Option<String>,
    /// Saved local branches, tags, notes, and durable Mjolnir refs.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub saved_refs: BTreeMap<String, String>,
    /// Newest to oldest, including stash entries unreachable from refs/stash.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stash_stack: Vec<SavedStashEntry>,
    /// The Git operation an independent clone was in the middle of: a merge,
    /// cherry-pick, revert, rebase or bisect, or unmerged index entries that
    /// any command left. Absent when there is none, so every other archive is
    /// written exactly as before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_operation: Option<SavedGitOperation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_delta: Option<Box<WorkspaceSnapshotMetadata>>,
}

impl RepositoryMetadata {
    /// Saved refs that hold the session's own Git work: everything except
    /// Mjolnir's private refs, which are never published and so must not
    /// decide whether the work is.
    pub fn work_refs(&self) -> impl Iterator<Item = (&String, &String)> {
        self.saved_refs
            .iter()
            .filter(|(name, _)| !name.starts_with(MJOLNIR_REF_NAMESPACE))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedStashEntry {
    pub commit: String,
    pub message: String,
}

/// A linked worktree of the captured repository that lives inside the
/// captured checkout, for example one made with `git worktree add .mj/lanes/x`.
/// Its commits live in the repository's own bundle; this record and its
/// payloads carry the rest of what the worktree holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkedWorktreeMetadata {
    /// The worktree's directory below the checkout's top level, written as
    /// `/`-separated text on every OS.
    #[serde(with = "slash_path")]
    pub path: PathBuf,
    pub head_commit: String,
    /// The branch checked out in the worktree; `None` when HEAD is detached.
    pub branch: Option<String>,
    /// The reason the worktree was locked with, empty for a lock without one.
    /// `None` when it is not locked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_reason: Option<String>,
    /// The operation the worktree was in the middle of, as for a clone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_operation: Option<SavedGitOperation>,
}

mod slash_path {
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _, ser::Error as _};
    use std::path::{Component, Path, PathBuf};

    pub fn serialize<S: Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
        let mut parts = Vec::new();
        for component in path.components() {
            let Component::Normal(part) = component else {
                return Err(S::Error::custom(
                    "worktree path is not a plain relative path",
                ));
            };
            parts.push(
                part.to_str()
                    .ok_or_else(|| S::Error::custom("worktree path is not UTF-8"))?,
            );
        }
        serializer.serialize_str(&parts.join("/"))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
        let text = String::deserialize(deserializer)?;
        if text.is_empty() {
            return Err(D::Error::custom("worktree path is empty"));
        }
        Ok(text.split('/').collect())
    }
}

/// A linked worktree's manifest entry: its record and where its payloads are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkedWorktreeManifest {
    #[serde(flatten)]
    pub metadata: LinkedWorktreeMetadata,
    pub staged_patch_path: String,
    pub unstaged_patch_path: String,
    pub untracked_tar_path: String,
    /// Present exactly when `metadata.git_operation` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflicted_worktree_tar_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_state_tar_path: Option<String>,
}

/// The index side of an unfinished Git operation, and the objects it needs.
/// The operation's working-tree files and its state files in the Git
/// directory are the repository's two operation payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedGitOperation {
    /// The index's unmerged entries, in index order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmerged: Vec<SavedUnmergedEntry>,
    /// Unmerged paths that had no file in the working tree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_worktree_paths: Vec<PathBuf>,
    /// Every object the unmerged entries and the state files name. The
    /// committed bundle carries each one the restored clone could lack, and a
    /// restore checks that all of them exist before it writes the operation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub objects: Vec<String>,
}

/// One stage of an unmerged path, as `git ls-files --unmerged` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedUnmergedEntry {
    pub path: PathBuf,
    /// 1 for the common ancestor, 2 for ours, 3 for theirs.
    pub stage: u8,
    /// The octal mode as Git writes it, such as `100644`.
    pub mode: String,
    pub object: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryManifest {
    #[serde(flatten)]
    pub metadata: RepositoryMetadata,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub committed_bundle_path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub staged_patch_path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unstaged_patch_path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub untracked_tar_path: String,
    /// The working-tree files of the unmerged paths. Present exactly when
    /// `metadata.git_operation` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflicted_worktree_tar_path: Option<String>,
    /// The operation's state files from the Git directory. Present exactly
    /// when `metadata.git_operation` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_state_tar_path: Option<String>,
    /// Linked worktrees inside the checkout, parents before children. An
    /// older reader would ignore this field, which is why an archive that has
    /// any declares [`ARCHIVE_SCHEMA_VERSION_LINKED_WORKTREES`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub linked_worktrees: Vec<LinkedWorktreeManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_delta_pack_path: Option<String>,
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
    /// Ignored. Archives written by protocol 26 workers carried a command
    /// receipt ledger here; it is accepted so those archives still restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_ledger: Option<serde_json::Value>,
    /// Versioned worker assessment state, validated by assessment::Checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assessment_state: Option<serde_json::Value>,
    /// Worker-owned state outside the bounded assessment record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_stack: Option<crate::attention::NotificationStack>,
    /// Highest relay event ordinal incorporated into this projection.
    pub event_frontier: u64,
    /// Relay-authored rolling digest of the exact event prefix at the frontier.
    pub event_frontier_digest: String,
    pub session: CanonicalSessionState,
    pub transcript: Vec<CanonicalTranscriptItem>,
    pub queued_prompts: Vec<CanonicalQueuedPrompt>,
    /// The worker's mail at the cut, copied from its relay. Absent when it
    /// had none, and in archives from workers that could not report it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mailbox: Option<CanonicalMailbox>,
    /// Worker-owned proof of whether the archived native session was used.
    /// Absent in legacy archives (schema <= 14) and from workers older than
    /// relay protocol 80.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_use_evidence: Option<crate::relay::NativeSessionUseEvidence>,
}

/// Mail a worker accepted, as its relay held it at a checkpoint cut. The
/// daemon marked each event accepted when the worker took it and will not send
/// it again, so restore must reinstate this for the mail to arrive at all.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalMailbox {
    /// Accepted events not yet handed to the agent, in delivery order. Events
    /// a hook had taken without confirming come first, where returning its
    /// lease puts them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_events: Vec<crate::mailbox::MailboxEvent>,
    /// Recently delivered event keys and the journal ordinal of their
    /// delivery. A producer that retries one is acknowledged without a second
    /// delivery; this is the worker's only deduplication record for mail.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub delivered_event_keys: BTreeMap<String, u64>,
    /// Mail already rendered into the hidden context of a queued prompt, keyed
    /// by that prompt's command ID. A worker restart moves an interrupted
    /// prompt's claim to its recovery prompt, which carries it from then on.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub prompt_contexts: BTreeMap<String, String>,
}

impl CanonicalMailbox {
    pub fn is_empty(&self) -> bool {
        self.pending_events.is_empty()
            && self.delivered_event_keys.is_empty()
            && self.prompt_contexts.is_empty()
    }

    /// The shape the worker's own admission guarantees, checked again before
    /// an archive or seed can become relay state.
    pub fn validate(&self) -> Result<()> {
        let mut pending = BTreeSet::new();
        for event in &self.pending_events {
            ensure!(
                !event.key.trim().is_empty()
                    && !event.source.trim().is_empty()
                    && event.has_renderable_content(),
                "archived mailbox event '{}' has no key, source, or content",
                event.key
            );
            ensure!(
                pending.insert(event.key.as_str()),
                "archived mailbox event '{}' is pending more than once",
                event.key
            );
            ensure!(
                !self.delivered_event_keys.contains_key(&event.key),
                "archived mailbox event '{}' is both pending and delivered",
                event.key
            );
        }
        ensure!(
            self.delivered_event_keys.len() <= crate::relay::RELAY_RETRY_ID_RETENTION,
            "archived mailbox retains more delivered keys than a worker keeps"
        );
        for (command_id, context) in &self.prompt_contexts {
            ensure!(
                !command_id.trim().is_empty() && !context.trim().is_empty(),
                "archived mailbox prompt context for '{command_id}' is empty"
            );
        }
        Ok(())
    }
}

impl CanonicalSessionSnapshot {
    pub fn current_context_start(&self) -> u64 {
        self.transcript
            .iter()
            .filter(|item| is_context_boundary(&item.stable_id))
            .map(|item| item.position)
            .max()
            .unwrap_or(0)
    }

    pub fn validate(&self) -> Result<()> {
        validate_canonical_session(self)
    }

    /// Whether two snapshots carry the same session content.
    ///
    /// The event frontier and its digest are deliberately ignored: relay
    /// bookkeeping advances them without changing what the session contains.
    /// `last_activity_at_ms` is a watermark of the same kind.
    pub fn content_matches(&self, other: &Self) -> bool {
        self.command_ledger == other.command_ledger
            && self.assessment_state == other.assessment_state
            && self.notification_stack == other.notification_stack
            && self.transcript == other.transcript
            && self.queued_prompts == other.queued_prompts
            && self.mailbox == other.mailbox
            && self.native_use_evidence == other.native_use_evidence
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
    pub configuration: crate::state::SessionConfiguration,
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
    Message {
        event: Box<crate::mailbox::MailboxEvent>,
    },
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_line_count: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        first_error_line: Option<String>,
        #[serde(default, skip_serializing_if = "crate::transcript::is_false")]
        detail_trimmed: bool,
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
    NativeArtifact {
        relative_path: PathBuf,
    },
    GitBundle {
        repository_id: String,
    },
    GitOriginDeltaPack {
        repository_id: String,
    },
    GitStagedPatch {
        repository_id: String,
    },
    GitUnstagedPatch {
        repository_id: String,
    },
    GitUntrackedTar {
        repository_id: String,
    },
    /// The working-tree files of a repository's unmerged paths, conflict
    /// markers included. A build that predates it rejects the manifest as a
    /// whole, so it cannot restore the clone without its operation.
    GitConflictedWorktreeTar {
        repository_id: String,
    },
    /// The state files of a repository's unfinished Git operation, named as
    /// `git rev-parse --git-path` names them.
    GitOperationStateTar {
        repository_id: String,
    },
    /// The payloads of the repository's `worktree`th linked worktree, which
    /// are those of the checkout itself for that worktree's directory.
    GitLinkedWorktreeStagedPatch {
        repository_id: String,
        worktree: u32,
    },
    GitLinkedWorktreeUnstagedPatch {
        repository_id: String,
        worktree: u32,
    },
    GitLinkedWorktreeUntrackedTar {
        repository_id: String,
        worktree: u32,
    },
    GitLinkedWorktreeConflictedWorktreeTar {
        repository_id: String,
        worktree: u32,
    },
    GitLinkedWorktreeOperationStateTar {
        repository_id: String,
        worktree: u32,
    },
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
    pub data: NativeArtifactData,
    pub mode: u32,
}

/// One owner for native bytes: generated runtime state or a disk source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeArtifactData {
    Bytes(Vec<u8>),
    File(PathBuf),
}

impl From<Vec<u8>> for NativeArtifactData {
    fn from(bytes: Vec<u8>) -> Self {
        Self::Bytes(bytes)
    }
}

impl NativeArtifactData {
    pub fn open_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(path)?;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::other(
                "native artifact is not a regular file",
            ));
        }
        Ok(file)
    }

    /// Materialize a small provider record when its format requires rewriting.
    /// Checkpoint capture and archive writing must use `NativeArtifact::reader`.
    pub fn bytes(&self) -> std::io::Result<std::borrow::Cow<'_, [u8]>> {
        match self {
            Self::Bytes(bytes) => Ok(std::borrow::Cow::Borrowed(bytes)),
            Self::File(path) => Ok(std::borrow::Cow::Owned(std::fs::read(path)?)),
        }
    }
}

impl NativeArtifact {
    pub fn size(&self) -> std::io::Result<u64> {
        match &self.data {
            NativeArtifactData::File(path) => {
                Ok(NativeArtifactData::open_file(path)?.metadata()?.len())
            }
            NativeArtifactData::Bytes(bytes) => Ok(bytes.len() as u64),
        }
    }

    pub fn reader(&self) -> std::io::Result<Box<dyn std::io::Read + '_>> {
        match &self.data {
            NativeArtifactData::File(path) => Ok(Box::new(NativeArtifactData::open_file(path)?)),
            NativeArtifactData::Bytes(bytes) => Ok(Box::new(std::io::Cursor::new(bytes))),
        }
    }
}

pub fn validate_native_use_evidence(
    evidence: &crate::relay::NativeSessionUseEvidence,
    event_frontier: u64,
) -> Result<()> {
    ensure!(
        evidence.version == 1,
        "unsupported native-session-use evidence version {}",
        evidence.version
    );
    ensure!(
        !evidence.native_session_id.trim().is_empty(),
        "native-session-use evidence has an empty native session ID"
    );
    if let Some(ordinal) = evidence.opened_ordinal {
        ensure!(
            ordinal <= event_frontier,
            "native-session-use evidence opened_ordinal {} is beyond event frontier {}",
            ordinal,
            event_frontier
        );
    }
    Ok(())
}

fn validate_canonical_session(snapshot: &CanonicalSessionSnapshot) -> Result<()> {
    if let Some(stack) = &snapshot.notification_stack {
        stack.validate_at(snapshot.event_frontier)?;
    }
    if let Some(value) = &snapshot.assessment_state {
        crate::assessment::Checkpoint::decode(value)?;
    }
    if let Some(evidence) = &snapshot.native_use_evidence {
        validate_native_use_evidence(evidence, snapshot.event_frontier)?;
    }
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
    let outputs = crate::transcript::TerminalOutputs::of_canonical(snapshot.transcript.iter());
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
            CanonicalTranscriptBody::Message { event } => {
                ensure!(
                    !event.key.trim().is_empty()
                        && event.message_text().is_some()
                        && item.stable_id == crate::transcript::message_item_id(&event.key),
                    "canonical message '{}' has invalid message data",
                    item.stable_id
                );
            }
            CanonicalTranscriptBody::User { content } => {
                for (index, block) in content.iter().enumerate() {
                    serde_json::from_value::<agent_client_protocol::v1::schema::ContentBlock>(
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
                    serde_json::from_value::<agent_client_protocol::v1::schema::ContentChunk>(
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
                terminal_refs,
                ..
            } => {
                serde_json::from_value::<agent_client_protocol::v1::schema::ToolCall>(call.clone())
                    .with_context(|| {
                        format!(
                            "canonical transcript item '{}' has invalid ACP tool call",
                            item.stable_id
                        )
                    })?;
                for output in outputs.embedded_for_tool(&item.stable_id) {
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
            CanonicalTranscriptBody::TerminalOutput { record, .. } => {
                validate_canonical_terminal_output(record, &item.stable_id)?;
            }
            CanonicalTranscriptBody::Plan { plan } => {
                serde_json::from_value::<agent_client_protocol::v1::schema::Plan>(plan.clone())
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
            serde_json::from_value::<agent_client_protocol::v1::schema::ContentBlock>(
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
    if let Some(mailbox) = &snapshot.mailbox {
        mailbox.validate()?;
        for command_id in mailbox.prompt_contexts.keys() {
            ensure!(
                snapshot
                    .queued_prompts
                    .iter()
                    .any(|prompt| { prompt.command_id == *command_id && prompt.kind.is_prompt() }),
                "canonical mailbox context belongs to '{command_id}', which is not a queued prompt"
            );
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
