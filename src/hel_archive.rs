//! Versioned, verified checkpoint archives for Hel sessions.
//!
//! This module deliberately accepts native harness artifacts one file at a
//! time.  Harness adapters must use a versioned allowlist; recursively copying
//! a profile home would risk archiving credentials and configuration.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail, ensure};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

use crate::hel_config::HarnessKind;
use crate::hel_transcript::is_false;

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
pub const ARCHIVE_FORMAT: &str = "hel-session";
pub const EVENT_FRONTIER_GENESIS_DIGEST: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

const MANIFEST_PATH: &str = "manifest.json";
const CANONICAL_SESSION_PATH: &str = "canonical/session.json";
const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PAYLOAD_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 32 * 1024 * 1024 * 1024;
/// Compressible payloads larger than this are split into parts of this size.
/// DEFLATE is sequential inside one stream, so parts are what let both the
/// writer and the reader use every core on a large payload.
const PAYLOAD_PART_BYTES: usize = 16 * 1024 * 1024;
const PAYLOAD_PART_SUFFIX: &str = ".helpart.";
/// Small ZIP metadata uses fast DEFLATE. Large archive payloads use Zstandard,
/// whose independent shards keep both compression and decompression parallel.
const DEFLATE_LEVEL: i64 = 1;
const ZSTD_LEVEL: i64 = 1;

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
    /// Informational provenance. Session deltas exclude every origin ref rather
    /// than a single base, so they record an empty string.
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
/// [`crate::hel_state::TerminalOutputRecord`].
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositorySnapshot {
    pub metadata: RepositoryMetadata,
    pub committed_bundle: Vec<u8>,
    pub staged_patch: Vec<u8>,
    pub unstaged_patch: Vec<u8>,
    pub untracked_tar: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveInput {
    pub session: SessionManifest,
    pub target: TargetManifest,
    pub bundle: BundleManifest,
    pub canonical_session: CanonicalSessionSnapshot,
    pub native_artifacts: Vec<NativeArtifact>,
    pub repositories: Vec<RepositorySnapshot>,
}

#[derive(Clone, Copy)]
struct ArchiveInputView<'a> {
    session: &'a SessionManifest,
    target: &'a TargetManifest,
    bundle: &'a BundleManifest,
    canonical_session: &'a CanonicalSessionSnapshot,
    native_artifacts: &'a [NativeArtifact],
    repositories: &'a [RepositorySnapshot],
}

impl<'a> From<&'a ArchiveInput> for ArchiveInputView<'a> {
    fn from(input: &'a ArchiveInput) -> Self {
        Self {
            session: &input.session,
            target: &input.target,
            bundle: &input.bundle,
            canonical_session: &input.canonical_session,
            native_artifacts: &input.native_artifacts,
            repositories: &input.repositories,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedArchive {
    pub manifest: ArchiveManifest,
    /// Payload bytes keyed by the exact manifest path.
    pub payloads: BTreeMap<String, Vec<u8>>,
    pub archive_sha256: String,
}

impl VerifiedArchive {
    pub fn payload(&self, descriptor: &PayloadDescriptor) -> Result<&[u8]> {
        self.payloads
            .get(&descriptor.path)
            .map(Vec::as_slice)
            .ok_or_else(|| anyhow!("verified payload '{}' is missing", descriptor.path))
    }

    pub fn payload_by_role(&self, role: &PayloadRole) -> Result<&[u8]> {
        let descriptor = self
            .manifest
            .payloads
            .iter()
            .find(|descriptor| &descriptor.role == role)
            .ok_or_else(|| anyhow!("archive does not contain payload role {role:?}"))?;
        self.payload(descriptor)
    }

    pub fn canonical_session(&self) -> Result<CanonicalSessionSnapshot> {
        let snapshot: CanonicalSessionSnapshot =
            serde_json::from_slice(self.payload_by_role(&PayloadRole::CanonicalSession)?)
                .context("parse canonical session snapshot")?;
        snapshot.validate()?;
        Ok(snapshot)
    }
}

/// Fully verified archive metadata for callers that do not need to restore
/// payload bodies. Verification streams repository and native payloads instead
/// of retaining them, so memory is bounded by the manifest, canonical session,
/// and ZIP read buffers rather than the archive's expanded size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedArchiveMetadata {
    pub manifest: ArchiveManifest,
    pub canonical_session: CanonicalSessionSnapshot,
    pub archive_sha256: String,
}

/// Repository state retained by the resume source preflight. The rest of the
/// archive is still streamed, hashed, and validated without being held in
/// memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedRepositoryBundle {
    pub metadata: RepositoryMetadata,
    pub committed_bundle: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedRepositoryBundles {
    pub archive_sha256: String,
    pub repositories: Vec<VerifiedRepositoryBundle>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointRepositoryBundle {
    pub metadata: RepositoryMetadata,
    pub committed_bundle: Vec<u8>,
}

/// Read the source commits a checkpoint bundle requires without asking Git to
/// import its pack. Importing into an empty repository can make partial-clone
/// Git versions lazily contact the archived origin before Hel has installed
/// the configured source's credentials.
pub fn checkpoint_bundle_prerequisites(
    repository: &CheckpointRepositoryBundle,
) -> Result<Vec<String>> {
    let bundle = &repository.committed_bundle;
    if bundle.is_empty() {
        return Ok(vec![repository.metadata.head_commit.clone()]);
    }

    let header_end = bundle
        .windows(2)
        .position(|window| window == b"\n\n")
        .context("checkpoint Git bundle has no header terminator")?;
    ensure!(
        bundle[header_end + 2..].starts_with(b"PACK"),
        "checkpoint Git bundle has no pack payload"
    );
    let mut lines = bundle[..header_end].split(|byte| *byte == b'\n');
    let version = lines
        .next()
        .context("checkpoint Git bundle has no header")?;
    ensure!(
        matches!(version, b"# v2 git bundle" | b"# v3 git bundle"),
        "unsupported checkpoint Git bundle version"
    );

    let mut prerequisites = Vec::new();
    let mut advertised_head = None;
    for line in lines {
        if let Some(prerequisite) = line.strip_prefix(b"-") {
            prerequisites.push(bundle_header_object_id(prerequisite, "prerequisite")?);
            continue;
        }
        if line.starts_with(b"@") {
            ensure!(
                version == b"# v3 git bundle",
                "Git bundle v2 contains a capability"
            );
            continue;
        }
        let separator = line
            .iter()
            .position(u8::is_ascii_whitespace)
            .context("checkpoint Git bundle contains an invalid reference")?;
        let (object_id, reference) = line.split_at(separator);
        let object_id = bundle_header_object_id(object_id, "reference")?;
        if reference
            .iter()
            .copied()
            .skip_while(u8::is_ascii_whitespace)
            .eq(b"HEAD".iter().copied())
        {
            ensure!(
                advertised_head.replace(object_id).is_none(),
                "checkpoint Git bundle advertises HEAD more than once"
            );
        }
    }
    let advertised_head =
        advertised_head.context("checkpoint Git bundle does not advertise HEAD")?;
    ensure!(
        advertised_head.eq_ignore_ascii_case(&repository.metadata.head_commit),
        "checkpoint Git bundle HEAD does not match checkpoint metadata"
    );
    Ok(prerequisites)
}

fn bundle_header_object_id(line: &[u8], kind: &str) -> Result<String> {
    let object_id = line
        .split(|byte| byte.is_ascii_whitespace())
        .next()
        .context("checkpoint Git bundle contains an empty object ID")?;
    ensure!(
        matches!(object_id.len(), 40 | 64) && object_id.iter().all(u8::is_ascii_hexdigit),
        "checkpoint Git bundle contains an invalid {kind} object ID"
    );
    Ok(String::from_utf8(object_id.to_ascii_lowercase())
        .expect("ASCII hexadecimal object ID is UTF-8"))
}

#[derive(Debug)]
pub enum CloseVerification {
    Verified {
        archive_path: PathBuf,
        archive_sha256: String,
    },
    /// The target must remain live and retryable while this result is blocked.
    Blocked { error: anyhow::Error },
}

impl CloseVerification {
    pub fn teardown_allowed(&self) -> bool {
        matches!(self, Self::Verified { .. })
    }
}

#[derive(Debug)]
struct PendingPayload<'a> {
    descriptor: PayloadDescriptor,
    data: Cow<'a, [u8]>,
}

/// Writes, fsyncs, atomically replaces, reopens, and verifies an archive.
///
/// Success means it is safe for the caller's close state machine to tear down
/// the target. Failure leaves an existing destination untouched whenever the
/// failure occurs before the final same-directory rename.
pub fn write_archive_atomic(path: &Path, input: &ArchiveInput) -> Result<VerifiedArchiveMetadata> {
    write_archive_installed(path, input)?;
    verify_archive_streaming(path)
        .with_context(|| format!("verify newly written archive {}", path.display()))
}

/// Writes and installs an archive exactly as [`write_archive_atomic`] does, then
/// hashes it in one sequential pass instead of structurally re-reading it.
///
/// The checkpoint export path uses this: the target just wrote the ZIP from
/// validated input, and the controller checks the downloaded bytes against the
/// returned digest. Resume/import structurally verifies the archive when it is
/// read. Callers without either check must keep using [`write_archive_atomic`].
pub fn write_archive_hashed(path: &Path, input: &ArchiveInput) -> Result<String> {
    write_archive_hashed_view(path, input.into())
}

pub fn write_archive_hashed_borrowed(
    path: &Path,
    session: &SessionManifest,
    target: &TargetManifest,
    bundle: &BundleManifest,
    canonical_session: &CanonicalSessionSnapshot,
    native_artifacts: &[NativeArtifact],
    repositories: &[RepositorySnapshot],
) -> Result<String> {
    write_archive_hashed_view(
        path,
        ArchiveInputView {
            session,
            target,
            bundle,
            canonical_session,
            native_artifacts,
            repositories,
        },
    )
}

fn write_archive_hashed_view(path: &Path, input: ArchiveInputView<'_>) -> Result<String> {
    let installed_started = std::time::Instant::now();
    write_archive_installed_view(path, input, PAYLOAD_PART_BYTES)?;
    let installed_ms = installed_started.elapsed().as_millis();
    let hash_started = std::time::Instant::now();
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let digest = digest_reader(&mut file)
        .with_context(|| format!("hash newly written archive {}", path.display()))?;
    if std::env::var_os("MJ_CHECKPOINT_BENCH_PHASES").is_some() {
        eprintln!(
            "archive writer phases: installed_ms={installed_ms} final_hash_ms={}",
            hash_started.elapsed().as_millis()
        );
    }
    Ok(digest)
}

fn write_archive_installed(path: &Path, input: &ArchiveInput) -> Result<()> {
    write_archive_installed_with_part_size(path, input, PAYLOAD_PART_BYTES)
}

fn write_archive_installed_with_part_size(
    path: &Path,
    input: &ArchiveInput,
    part_bytes: usize,
) -> Result<()> {
    write_archive_installed_view(path, input.into(), part_bytes)
}

fn write_archive_installed_view(
    path: &Path,
    input: ArchiveInputView<'_>,
    part_bytes: usize,
) -> Result<()> {
    let prepare_started = std::time::Instant::now();
    let (manifest, payloads) = prepare_archive_view_with_part_size(input, part_bytes)?;
    let prepare_ms = prepare_started.elapsed().as_millis();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("create archive directory {}", parent.display()))?;

    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary archive in {}", parent.display()))?;
    restrict_archive_permissions(temporary.path())?;
    let zip_started = std::time::Instant::now();
    write_zip(temporary.as_file_mut(), &manifest, &payloads)?;
    let zip_ms = zip_started.elapsed().as_millis();
    let file_sync_started = std::time::Instant::now();
    temporary
        .as_file_mut()
        .sync_all()
        .with_context(|| format!("fsync temporary archive in {}", parent.display()))?;
    let file_sync_ms = file_sync_started.elapsed().as_millis();
    let persist_started = std::time::Instant::now();
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("atomically replace {}", path.display()))?;
    restrict_archive_permissions(path)?;
    let persist_ms = persist_started.elapsed().as_millis();
    let directory_sync_started = std::time::Instant::now();
    sync_directory(parent)?;
    if std::env::var_os("MJ_CHECKPOINT_BENCH_PHASES").is_some() {
        eprintln!(
            "archive install phases: prepare_ms={prepare_ms} zip_ms={zip_ms} file_sync_ms={file_sync_ms} persist_ms={persist_ms} directory_sync_ms={}",
            directory_sync_started.elapsed().as_millis()
        );
    }
    drop(payloads);
    drop(manifest);
    Ok(())
}

pub fn checkpoint_for_close(path: &Path, input: &ArchiveInput) -> CloseVerification {
    match write_archive_atomic(path, input) {
        Ok(verified) => CloseVerification::Verified {
            archive_path: path.to_path_buf(),
            archive_sha256: verified.archive_sha256,
        },
        Err(error) => CloseVerification::Blocked { error },
    }
}

pub fn read_archive_verified(path: &Path) -> Result<VerifiedArchive> {
    let contents = read_verified_zip(path, PayloadRetention::All)?;
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    Ok(VerifiedArchive {
        manifest: contents.manifest,
        payloads: contents.payloads,
        archive_sha256: digest_reader(&mut file)?,
    })
}

/// Verify an archive without materializing repository or native payloads.
/// Every ZIP entry is still fully read, hashed, and checked; Git untracked
/// payloads are parsed through the same path-safety validator while streaming.
///
/// A sharded untracked tar is the one exception to streaming: its parts are
/// held in memory long enough to reassemble and parse the tar, because tar
/// safety is a property of the whole payload rather than of one part.
pub fn verify_archive_streaming(path: &Path) -> Result<VerifiedArchiveMetadata> {
    let contents = read_verified_zip(path, PayloadRetention::CanonicalOnly)?;
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    Ok(VerifiedArchiveMetadata {
        manifest: contents.manifest,
        canonical_session: contents.canonical_session,
        archive_sha256: digest_reader(&mut file)?,
    })
}

/// Verify an archive while retaining only its Git bundle payloads.
///
/// Resume uses this before provisioning to prove that the configured sources
/// still provide every prerequisite of each thin bundle. Native artifacts and
/// patches can be very large, so they stay on the streaming path.
pub fn verify_repository_bundles_streaming(path: &Path) -> Result<VerifiedRepositoryBundles> {
    let contents = read_verified_zip(path, PayloadRetention::CanonicalAndGitBundles)?;
    let repositories = contents
        .manifest
        .repositories
        .iter()
        .map(|repository| {
            let role = PayloadRole::GitBundle {
                repository_id: repository.metadata.id.clone(),
            };
            let descriptor = contents
                .manifest
                .payloads
                .iter()
                .find(|descriptor| descriptor.role == role)
                .with_context(|| {
                    format!(
                        "repository {:?} has no committed bundle payload",
                        repository.metadata.id
                    )
                })?;
            let committed_bundle = contents
                .payloads
                .get(&descriptor.path)
                .cloned()
                .with_context(|| {
                    format!(
                        "verified committed bundle for repository {:?} was not retained",
                        repository.metadata.id
                    )
                })?;
            Ok(VerifiedRepositoryBundle {
                metadata: repository.metadata.clone(),
                committed_bundle,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    Ok(VerifiedRepositoryBundles {
        archive_sha256: digest_reader(&mut file)?,
        repositories,
    })
}

/// Read only the repository bundles from a checkpoint whose complete archive
/// digest was already persisted after full verification.
///
/// Resume uses this narrow reader for its source-availability preflight. The
/// restore path still performs full payload verification before changing any
/// target state. This deliberately avoids re-hashing the archive or bundle:
/// Git validates the pack during the preflight, and restore validates every
/// payload before using it.
pub fn read_checkpoint_repository_bundles(path: &Path) -> Result<Vec<CheckpointRepositoryBundle>> {
    let mut archive = open_archive(path)?;
    let manifest_bytes = {
        let mut entry = archive
            .by_name(MANIFEST_PATH)
            .with_context(|| format!("archive is missing {MANIFEST_PATH}"))?;
        ensure!(!entry.is_dir(), "archive manifest is a directory entry");
        ensure!(
            entry.size() <= MAX_MANIFEST_BYTES,
            "archive manifest is too large"
        );
        let mut bytes = Vec::with_capacity(entry.size().min(usize::MAX as u64) as usize);
        entry
            .read_to_end(&mut bytes)
            .context("read archive manifest")?;
        bytes
    };
    let manifest = parse_archive_manifest(&manifest_bytes)?;
    let repositories = manifest
        .repositories
        .iter()
        .map(|repository| {
            let role = PayloadRole::GitBundle {
                repository_id: repository.metadata.id.clone(),
            };
            let descriptor = manifest
                .payloads
                .iter()
                .find(|descriptor| descriptor.role == role)
                .with_context(|| {
                    format!(
                        "repository {:?} has no committed bundle payload",
                        repository.metadata.id
                    )
                })?;
            let committed_bundle = read_checkpoint_payload(&mut archive, descriptor)?;
            Ok(CheckpointRepositoryBundle {
                metadata: repository.metadata.clone(),
                committed_bundle,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(repositories)
}

fn read_checkpoint_payload(
    archive: &mut zip::ZipArchive<File>,
    descriptor: &PayloadDescriptor,
) -> Result<Vec<u8>> {
    if descriptor.parts.is_empty() {
        return read_checkpoint_entry(archive, &descriptor.path, descriptor.size, descriptor.mode);
    }
    let mut bytes = Vec::with_capacity(descriptor.size.min(usize::MAX as u64) as usize);
    for part in &descriptor.parts {
        bytes.extend_from_slice(&read_checkpoint_entry(
            archive,
            &part.path,
            part.size,
            descriptor.mode,
        )?);
    }
    ensure!(
        bytes.len() as u64 == descriptor.size,
        "size mismatch for payload '{}'",
        descriptor.path
    );
    ensure!(
        digest_bytes(&bytes) == descriptor.sha256,
        "SHA-256 mismatch for payload '{}'",
        descriptor.path
    );
    Ok(bytes)
}

fn read_checkpoint_entry(
    archive: &mut zip::ZipArchive<File>,
    path: &str,
    expected_size: u64,
    expected_mode: u32,
) -> Result<Vec<u8>> {
    let mut entry = archive
        .by_name(path)
        .with_context(|| format!("archive is missing payload '{path}'"))?;
    ensure!(!entry.is_dir(), "archive payload '{path}' is a directory");
    let enclosed = entry
        .enclosed_name()
        .ok_or_else(|| anyhow!("unsafe ZIP entry path '{}'", entry.name()))?;
    ensure!(
        slash_path(&enclosed)? == path,
        "archive payload path does not match '{path}'"
    );
    ensure!(
        entry.size() == expected_size,
        "size mismatch for payload '{path}'"
    );
    ensure!(
        entry.unix_mode().unwrap_or(0o600) & 0o7777 == expected_mode,
        "mode mismatch for payload '{path}'"
    );
    let mut bytes = Vec::with_capacity(expected_size.min(usize::MAX as u64) as usize);
    entry
        .read_to_end(&mut bytes)
        .with_context(|| format!("read ZIP entry '{path}'"))?;
    ensure!(
        bytes.len() as u64 == expected_size,
        "size mismatch for payload '{path}'"
    );
    Ok(bytes)
}

#[cfg(test)]
fn prepare_archive(input: &ArchiveInput) -> Result<(ArchiveManifest, Vec<PendingPayload<'_>>)> {
    prepare_archive_with_part_size(input, PAYLOAD_PART_BYTES)
}

#[cfg(test)]
fn prepare_archive_with_part_size(
    input: &ArchiveInput,
    part_bytes: usize,
) -> Result<(ArchiveManifest, Vec<PendingPayload<'_>>)> {
    prepare_archive_view_with_part_size(input.into(), part_bytes)
}

fn prepare_archive_view_with_part_size(
    input: ArchiveInputView<'_>,
    part_bytes: usize,
) -> Result<(ArchiveManifest, Vec<PendingPayload<'_>>)> {
    ensure!(part_bytes > 0, "archive payload part size is zero");
    ensure!(!input.session.id.trim().is_empty(), "session id is empty");
    ensure!(!input.bundle.id.trim().is_empty(), "bundle id is empty");
    validate_secret_free_map(&input.target.details)?;
    input.canonical_session.validate()?;

    let mut payloads = Vec::new();
    push_payload(
        &mut payloads,
        CANONICAL_SESSION_PATH.to_string(),
        Cow::Owned(
            serde_json::to_vec(input.canonical_session)
                .context("serialize canonical session snapshot")?,
        ),
        0o600,
        PayloadRole::CanonicalSession,
    )?;

    for artifact in input.native_artifacts {
        validate_archive_relative_path(&artifact.relative_path)?;
        ensure_not_secret_path(&artifact.relative_path)?;
        let archive_path = format!("native/{}", slash_path(&artifact.relative_path)?);
        push_payload(
            &mut payloads,
            archive_path,
            Cow::Borrowed(artifact.data.as_slice()),
            artifact.mode,
            PayloadRole::NativeArtifact {
                relative_path: artifact.relative_path.clone(),
            },
        )?;
    }

    let mut repository_ids = BTreeSet::new();
    let mut repositories = Vec::with_capacity(input.repositories.len());
    for repository in input.repositories {
        validate_component(&repository.metadata.id, "repository id")?;
        ensure!(
            repository_ids.insert(repository.metadata.id.clone()),
            "duplicate repository id '{}'",
            repository.metadata.id
        );
        validate_archive_relative_path(&repository.metadata.relative_destination)?;
        ensure!(
            !origin_contains_credentials(&repository.metadata.origin),
            "repository '{}' origin contains credentials",
            repository.metadata.id
        );
        validate_untracked_tar(&repository.untracked_tar)?;

        let root = format!("repositories/{}", repository.metadata.id);
        let committed_bundle_path = format!("{root}/committed.bundle");
        let staged_patch_path = format!("{root}/staged.patch");
        let unstaged_patch_path = format!("{root}/unstaged.patch");
        let untracked_tar_path = format!("{root}/untracked.tar");
        for (path, data, role) in [
            (
                &committed_bundle_path,
                &repository.committed_bundle,
                PayloadRole::GitBundle {
                    repository_id: repository.metadata.id.clone(),
                },
            ),
            (
                &staged_patch_path,
                &repository.staged_patch,
                PayloadRole::GitStagedPatch {
                    repository_id: repository.metadata.id.clone(),
                },
            ),
            (
                &unstaged_patch_path,
                &repository.unstaged_patch,
                PayloadRole::GitUnstagedPatch {
                    repository_id: repository.metadata.id.clone(),
                },
            ),
            (
                &untracked_tar_path,
                &repository.untracked_tar,
                PayloadRole::GitUntrackedTar {
                    repository_id: repository.metadata.id.clone(),
                },
            ),
        ] {
            push_payload(
                &mut payloads,
                path.clone(),
                Cow::Borrowed(data.as_slice()),
                0o600,
                role,
            )?;
        }
        repositories.push(RepositoryManifest {
            metadata: repository.metadata.clone(),
            committed_bundle_path,
            staged_patch_path,
            unstaged_patch_path,
            untracked_tar_path,
        });
    }

    payloads.par_iter_mut().for_each(|payload| {
        payload.descriptor.sha256 = digest_bytes(&payload.data);
        payload.descriptor.parts = plan_payload_parts(
            &payload.descriptor.path,
            &payload.data,
            payload_compression(&payload.descriptor.role),
            part_bytes,
        );
    });

    let mut paths = BTreeSet::new();
    for payload in &payloads {
        ensure!(
            paths.insert(payload.descriptor.path.clone()),
            "duplicate archive payload path '{}'",
            payload.descriptor.path
        );
    }
    let sharded = payloads
        .iter()
        .any(|payload| !payload.descriptor.parts.is_empty());
    let manifest = ArchiveManifest {
        schema_version: if sharded {
            ARCHIVE_SCHEMA_VERSION_SHARDED
        } else {
            ARCHIVE_SCHEMA_VERSION
        },
        format: ARCHIVE_FORMAT.to_string(),
        session: (*input.session).clone(),
        target: (*input.target).clone(),
        bundle: (*input.bundle).clone(),
        repositories,
        payloads: payloads
            .iter()
            .map(|payload| payload.descriptor.clone())
            .collect(),
    };
    validate_manifest(&manifest)?;
    Ok((manifest, payloads))
}

fn push_payload<'a>(
    payloads: &mut Vec<PendingPayload<'a>>,
    path: String,
    data: Cow<'a, [u8]>,
    mode: u32,
    role: PayloadRole,
) -> Result<()> {
    validate_archive_relative_path(Path::new(&path))?;
    ensure!(
        data.len() as u64 <= MAX_PAYLOAD_BYTES,
        "archive payload '{path}' is too large"
    );
    let descriptor = PayloadDescriptor {
        path,
        sha256: String::new(),
        size: data.len() as u64,
        mode: normalized_mode(mode)?,
        role,
        parts: Vec::new(),
    };
    payloads.push(PendingPayload { descriptor, data });
    Ok(())
}

/// Git bundles carry packfiles whose objects are already compressed, so they
/// are stored verbatim. Text and tar payloads compress well with Zstandard,
/// which is substantially faster than DEFLATE for checkpoint-sized history.
fn payload_compression(role: &PayloadRole) -> CompressionMethod {
    match role {
        PayloadRole::GitBundle { .. } => CompressionMethod::Stored,
        PayloadRole::CanonicalSession
        | PayloadRole::NativeArtifact { .. }
        | PayloadRole::GitStagedPatch { .. }
        | PayloadRole::GitUnstagedPatch { .. }
        | PayloadRole::GitUntrackedTar { .. } => CompressionMethod::Zstd,
    }
}

fn payload_part_path(path: &str, index: usize) -> String {
    format!("{path}{PAYLOAD_PART_SUFFIX}{index:05}")
}

/// Splits a compressible payload that is larger than `part_bytes` into ordered
/// parts. Stored payloads keep one entry: they cost no compression time, and
/// splitting them would only add entries a reader has to stitch back together.
fn plan_payload_parts(
    path: &str,
    data: &[u8],
    method: CompressionMethod,
    part_bytes: usize,
) -> Vec<PayloadPartDescriptor> {
    if method == CompressionMethod::Stored || data.len() <= part_bytes {
        return Vec::new();
    }
    data.par_chunks(part_bytes)
        .enumerate()
        .map(|(index, chunk)| PayloadPartDescriptor {
            path: payload_part_path(path, index),
            sha256: digest_bytes(chunk),
            size: chunk.len() as u64,
        })
        .collect()
}

/// One ZIP entry to write: either a whole payload, one part of a sharded
/// payload, or the manifest.
struct PlannedEntry<'a> {
    name: &'a str,
    mode: u32,
    method: CompressionMethod,
    data: &'a [u8],
}

fn write_zip(
    output: &mut File,
    manifest: &ArchiveManifest,
    payloads: &[PendingPayload<'_>],
) -> Result<()> {
    let manifest_bytes =
        serde_json::to_vec_pretty(manifest).context("serialize archive manifest")?;
    ensure!(
        manifest_bytes.len() as u64 <= MAX_MANIFEST_BYTES,
        "archive manifest is too large"
    );
    let mut entries = vec![PlannedEntry {
        name: MANIFEST_PATH,
        mode: 0o600,
        method: CompressionMethod::Deflated,
        data: &manifest_bytes,
    }];
    for payload in payloads {
        let descriptor = &payload.descriptor;
        let method = payload_compression(&descriptor.role);
        if descriptor.parts.is_empty() {
            entries.push(PlannedEntry {
                name: &descriptor.path,
                mode: descriptor.mode,
                method,
                data: &payload.data,
            });
            continue;
        }
        let mut offset = 0_usize;
        for part in &descriptor.parts {
            let end = usize::try_from(part.size)
                .ok()
                .and_then(|size| offset.checked_add(size))
                .filter(|end| *end <= payload.data.len())
                .ok_or_else(|| {
                    anyhow!("payload '{}' parts do not fit its body", descriptor.path)
                })?;
            entries.push(PlannedEntry {
                name: &part.path,
                mode: descriptor.mode,
                method,
                data: &payload.data[offset..end],
            });
            offset = end;
        }
        ensure!(
            offset == payload.data.len(),
            "payload '{}' parts do not cover its body",
            descriptor.path
        );
    }

    // Compression is the export freeze window, so every entry deflates on its
    // own core; the container is then assembled sequentially in plan order so
    // the archive layout stays deterministic.
    let compressed = entries
        .par_iter()
        .map(compress_entry)
        .collect::<Result<Vec<_>>>()?;

    let mut writer = zip::ZipWriter::new(output);
    for (entry, buffer) in entries.iter().zip(compressed) {
        let mut source = zip::ZipArchive::new(Cursor::new(buffer))
            .with_context(|| format!("reopen compressed ZIP entry '{}'", entry.name))?;
        let compressed_entry = source
            .by_index(0)
            .with_context(|| format!("read compressed ZIP entry '{}'", entry.name))?;
        writer
            .raw_copy_file(compressed_entry)
            .with_context(|| format!("write ZIP entry '{}'", entry.name))?;
    }
    writer.finish().context("finish Mjolnir archive ZIP")?;
    Ok(())
}

/// Compresses one entry into a single-entry ZIP so the assembly pass can copy
/// the finished deflate stream verbatim with [`zip::ZipWriter::raw_copy_file`].
fn compress_entry(entry: &PlannedEntry<'_>) -> Result<Vec<u8>> {
    let mut buffer = Cursor::new(Vec::with_capacity(entry.data.len() / 2 + 512));
    let mut writer = zip::ZipWriter::new(&mut buffer);
    writer
        .start_file(
            entry.name,
            SimpleFileOptions::default()
                .compression_method(entry.method)
                .compression_level(match entry.method {
                    CompressionMethod::Deflated => Some(DEFLATE_LEVEL),
                    CompressionMethod::Zstd => Some(ZSTD_LEVEL),
                    _ => None,
                })
                .unix_permissions(entry.mode)
                .large_file(entry.data.len() as u64 > zip::ZIP64_BYTES_THR),
        )
        .with_context(|| format!("start ZIP entry '{}'", entry.name))?;
    writer
        .write_all(entry.data)
        .with_context(|| format!("write ZIP entry '{}'", entry.name))?;
    writer
        .finish()
        .with_context(|| format!("compress ZIP entry '{}'", entry.name))?;
    Ok(buffer.into_inner())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PayloadRetention {
    All,
    CanonicalOnly,
    CanonicalAndGitBundles,
}

struct VerifiedZipContents {
    manifest: ArchiveManifest,
    canonical_session: CanonicalSessionSnapshot,
    payloads: BTreeMap<String, Vec<u8>>,
}

/// Structural facts about one ZIP entry, read from the central directory
/// before any body is decompressed.
struct ZipEntryMeta {
    index: usize,
    name: String,
    size: u64,
    mode: u32,
}

/// What the manifest says a ZIP entry must contain.
#[derive(Clone, Copy)]
enum EntryExpectation<'a> {
    Whole(&'a PayloadDescriptor),
    Part {
        payload: &'a PayloadDescriptor,
        part: &'a PayloadPartDescriptor,
    },
}

/// Each parallel reader owns one of these: ZIP entries can only be read one at
/// a time through a single handle, and the deflate decoder already buffers.
fn open_archive(path: &Path) -> Result<zip::ZipArchive<File>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    zip::ZipArchive::new(file).context("open Mjolnir archive ZIP")
}

/// Payload bodies the caller keeps.
fn retains_payload(retention: PayloadRetention, role: &PayloadRole) -> bool {
    retention == PayloadRetention::All
        || *role == PayloadRole::CanonicalSession
        || (retention == PayloadRetention::CanonicalAndGitBundles
            && matches!(role, PayloadRole::GitBundle { .. }))
}

/// Part bodies the reader has to hold until reassembly. Tar safety is a
/// property of the whole payload, so a sharded untracked tar is reassembled
/// even when the caller does not want the bytes.
fn retains_parts(retention: PayloadRetention, role: &PayloadRole) -> bool {
    retains_payload(retention, role) || matches!(role, PayloadRole::GitUntrackedTar { .. })
}

fn read_verified_zip(path: &Path, retention: PayloadRetention) -> Result<VerifiedZipContents> {
    let mut archive = open_archive(path)?;
    let mut entries = Vec::with_capacity(archive.len());
    let mut actual_paths = BTreeSet::<String>::new();
    let mut manifest_count = 0_usize;
    let mut total_size = 0_u64;
    for index in 0..archive.len() {
        let entry = archive
            .by_index_raw(index)
            .with_context(|| format!("read ZIP entry metadata {index}"))?;
        ensure!(
            !entry.is_dir(),
            "archive contains directory entry '{}'; only files are allowed",
            entry.name()
        );
        let enclosed = entry
            .enclosed_name()
            .ok_or_else(|| anyhow!("unsafe ZIP entry path '{}'", entry.name()))?;
        validate_archive_relative_path(&enclosed)?;
        ensure!(
            entry.size() <= MAX_PAYLOAD_BYTES,
            "ZIP entry '{}' is too large",
            entry.name()
        );
        total_size = total_size
            .checked_add(entry.size())
            .ok_or_else(|| anyhow!("archive expanded size overflow"))?;
        ensure!(
            total_size <= MAX_ARCHIVE_BYTES,
            "archive expanded size exceeds limit"
        );
        let name = slash_path(&enclosed)?;
        ensure!(
            actual_paths.insert(name.clone()),
            "duplicate ZIP entry '{name}'"
        );
        manifest_count += usize::from(name == MANIFEST_PATH);
        entries.push(ZipEntryMeta {
            index,
            name,
            size: entry.size(),
            mode: entry.unix_mode().unwrap_or(0o600) & 0o7777,
        });
    }
    ensure!(
        manifest_count == 1,
        "archive must contain exactly one {MANIFEST_PATH}"
    );
    let manifest_bytes = {
        let mut entry = archive
            .by_name(MANIFEST_PATH)
            .with_context(|| format!("archive is missing {MANIFEST_PATH}"))?;
        ensure!(!entry.is_dir(), "archive manifest is a directory entry");
        ensure!(
            entry.size() <= MAX_MANIFEST_BYTES,
            "archive manifest is too large"
        );
        let mut bytes = Vec::with_capacity(entry.size().min(usize::MAX as u64) as usize);
        entry
            .read_to_end(&mut bytes)
            .context("read archive manifest")?;
        bytes
    };
    drop(archive);
    let manifest = parse_archive_manifest(&manifest_bytes)?;

    let mut expectations = BTreeMap::<&str, EntryExpectation<'_>>::new();
    for descriptor in &manifest.payloads {
        if descriptor.parts.is_empty() {
            expectations.insert(
                descriptor.path.as_str(),
                EntryExpectation::Whole(descriptor),
            );
            continue;
        }
        for part in &descriptor.parts {
            expectations.insert(
                part.path.as_str(),
                EntryExpectation::Part {
                    payload: descriptor,
                    part,
                },
            );
        }
    }

    let mut payload_entries = Vec::with_capacity(entries.len());
    for meta in &entries {
        if meta.name == MANIFEST_PATH {
            ensure!(
                meta.size == manifest_bytes.len() as u64,
                "archive manifest size changed while it was read"
            );
            continue;
        }
        let expectation = expectations
            .get(meta.name.as_str())
            .ok_or_else(|| anyhow!("archive contains unlisted payload '{}'", meta.name))?;
        payload_entries.push((meta, *expectation));
    }
    // Entry names are unique and every one of them is listed, so equal counts
    // mean the manifest and the container describe the same entry set.
    ensure!(
        payload_entries.len() == expectations.len(),
        "archive payload list does not match manifest"
    );

    // DEFLATE cannot be inflated in parallel inside one stream, so read
    // parallelism comes from entries: each worker owns its own archive handle
    // and verifies whole entries end to end.
    let outcomes = payload_entries
        .par_iter()
        .map_init(
            || open_archive(path).map_err(|error| format!("{error:#}")),
            |archive, (meta, expectation)| {
                let archive = match archive {
                    Ok(archive) => archive,
                    Err(error) => bail!("{error}"),
                };
                read_verified_entry(archive, meta, *expectation, retention)
            },
        )
        .collect::<Vec<_>>();
    let mut bodies = BTreeMap::<&str, Vec<u8>>::new();
    for ((meta, _), outcome) in payload_entries.iter().zip(outcomes) {
        if let Some(bytes) = outcome? {
            bodies.insert(meta.name.as_str(), bytes);
        }
    }

    let mut payloads = BTreeMap::new();
    let mut canonical_session = None;
    for descriptor in &manifest.payloads {
        let bytes = reassemble_payload(descriptor, &mut bodies, retention)?;
        if matches!(descriptor.role, PayloadRole::GitUntrackedTar { .. })
            && let Some(bytes) = bytes.as_deref()
        {
            validate_untracked_tar(bytes)
                .with_context(|| format!("validate payload '{}'", descriptor.path))?;
        }
        if descriptor.role == PayloadRole::CanonicalSession {
            let bytes = bytes
                .as_deref()
                .expect("canonical session payload is always retained");
            let snapshot: CanonicalSessionSnapshot =
                serde_json::from_slice(bytes).context("parse canonical session snapshot")?;
            snapshot.validate()?;
            ensure!(
                canonical_session.replace(snapshot).is_none(),
                "archive contains duplicate canonical session payloads"
            );
        }
        if retention == PayloadRetention::All
            || (retention == PayloadRetention::CanonicalAndGitBundles
                && matches!(descriptor.role, PayloadRole::GitBundle { .. }))
        {
            payloads.insert(
                descriptor.path.clone(),
                bytes.expect("selected payloads are retained by the reader"),
            );
        }
    }
    Ok(VerifiedZipContents {
        manifest,
        canonical_session: canonical_session.context("archive canonical session is missing")?,
        payloads,
    })
}

/// Reads one ZIP entry, verifies it against the manifest, and returns its body
/// when the caller needs it.
fn read_verified_entry(
    archive: &mut zip::ZipArchive<File>,
    meta: &ZipEntryMeta,
    expectation: EntryExpectation<'_>,
    retention: PayloadRetention,
) -> Result<Option<Vec<u8>>> {
    let (payload, expected_size, expected_sha256, retain) = match expectation {
        EntryExpectation::Whole(payload) => (
            payload,
            payload.size,
            payload.sha256.as_str(),
            retains_payload(retention, &payload.role),
        ),
        EntryExpectation::Part { payload, part } => (
            payload,
            part.size,
            part.sha256.as_str(),
            retains_parts(retention, &payload.role),
        ),
    };
    let label = match expectation {
        EntryExpectation::Whole(_) => format!("payload '{}'", payload.path),
        EntryExpectation::Part { .. } => format!("payload part '{}'", meta.name),
    };
    ensure!(meta.size == expected_size, "size mismatch for {label}");
    ensure!(meta.mode == payload.mode, "mode mismatch for {label}");
    // Only a whole untracked tar can be parsed while it streams past; sharded
    // ones are reassembled after every part is verified.
    let stream_untracked_tar = !retain
        && matches!(expectation, EntryExpectation::Whole(_))
        && matches!(payload.role, PayloadRole::GitUntrackedTar { .. });

    let name = meta.name.as_str();
    let mut entry = archive
        .by_index(meta.index)
        .with_context(|| format!("read ZIP entry {}", meta.index))?;
    // Every worker opens the path again, so confirm this handle still sees the
    // entry the structural pass indexed instead of reporting a replaced file
    // as payload corruption.
    let enclosed = entry
        .enclosed_name()
        .ok_or_else(|| anyhow!("unsafe ZIP entry path '{}'", entry.name()))?;
    ensure!(
        slash_path(&enclosed)? == meta.name,
        "ZIP entry {} changed while the archive was read",
        meta.index
    );
    let mut bytes =
        retain.then(|| Vec::with_capacity(expected_size.min(usize::MAX as u64) as usize));
    let mut digesting = DigestingReader::new(&mut entry);
    if let Some(bytes) = bytes.as_mut() {
        digesting
            .read_to_end(bytes)
            .with_context(|| format!("read ZIP entry '{name}'"))?;
    } else if stream_untracked_tar {
        validate_untracked_tar_reader(&mut digesting)
            .with_context(|| format!("validate payload '{}'", payload.path))?;
        std::io::copy(&mut digesting, &mut std::io::sink())
            .with_context(|| format!("finish reading ZIP entry '{name}'"))?;
    } else {
        std::io::copy(&mut digesting, &mut std::io::sink())
            .with_context(|| format!("read ZIP entry '{name}'"))?;
    }
    let (actual_size, actual_digest) = digesting.finish();
    ensure!(actual_size == expected_size, "size mismatch for {label}");
    ensure!(
        actual_digest == expected_sha256,
        "SHA-256 mismatch for {label}"
    );
    Ok(bytes)
}

/// Turns verified entry bodies back into one payload body. Restore and import
/// therefore never see part entries, whatever the archive layout is.
fn reassemble_payload(
    descriptor: &PayloadDescriptor,
    bodies: &mut BTreeMap<&str, Vec<u8>>,
    retention: PayloadRetention,
) -> Result<Option<Vec<u8>>> {
    if descriptor.parts.is_empty() {
        return Ok(bodies.remove(descriptor.path.as_str()));
    }
    if !retains_parts(retention, &descriptor.role) {
        return Ok(None);
    }
    let mut assembled = Vec::with_capacity(descriptor.size.min(usize::MAX as u64) as usize);
    for part in &descriptor.parts {
        let chunk = bodies.remove(part.path.as_str()).ok_or_else(|| {
            anyhow!(
                "payload '{}' is missing part '{}'",
                descriptor.path,
                part.path
            )
        })?;
        assembled.extend_from_slice(&chunk);
    }
    ensure!(
        assembled.len() as u64 == descriptor.size,
        "size mismatch for payload '{}'",
        descriptor.path
    );
    ensure!(
        digest_bytes(&assembled) == descriptor.sha256,
        "SHA-256 mismatch for payload '{}'",
        descriptor.path
    );
    Ok(Some(assembled))
}

struct DigestingReader<R> {
    inner: R,
    digest: Sha256,
    bytes_read: u64,
}

impl<R> DigestingReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            digest: Sha256::new(),
            bytes_read: 0,
        }
    }

    fn finish(self) -> (u64, String) {
        (self.bytes_read, format!("{:x}", self.digest.finalize()))
    }
}

impl<R: Read> Read for DigestingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.bytes_read = self.bytes_read.saturating_add(read as u64);
        self.digest.update(&buffer[..read]);
        Ok(read)
    }
}

fn parse_archive_manifest(manifest_bytes: &[u8]) -> Result<ArchiveManifest> {
    #[derive(Deserialize)]
    struct ArchiveHeader {
        schema_version: u32,
        format: String,
    }
    let header: ArchiveHeader =
        serde_json::from_slice(manifest_bytes).context("parse archive manifest header")?;
    ensure!(
        header.format == ARCHIVE_FORMAT,
        "unsupported archive format '{}'",
        header.format
    );
    ensure!(
        header.schema_version == ARCHIVE_SCHEMA_VERSION
            || header.schema_version == ARCHIVE_SCHEMA_VERSION_SHARDED,
        "incompatible Mjolnir archive schema {}; this build requires schema {}",
        header.schema_version,
        ARCHIVE_SCHEMA_VERSION
    );
    let manifest: ArchiveManifest =
        serde_json::from_slice(manifest_bytes).context("parse Mjolnir archive manifest")?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

/// The schema an archive must declare for the payload layout it carries.
/// Sharded payloads are exactly what schema 3 adds, so declaring the wrong
/// version is a manifest error either way round.
fn expected_schema_version(payloads: &[PayloadDescriptor]) -> u32 {
    if payloads.iter().any(|payload| !payload.parts.is_empty()) {
        ARCHIVE_SCHEMA_VERSION_SHARDED
    } else {
        ARCHIVE_SCHEMA_VERSION
    }
}

fn validate_manifest(manifest: &ArchiveManifest) -> Result<()> {
    let expected_schema = expected_schema_version(&manifest.payloads);
    ensure!(
        manifest.schema_version == expected_schema,
        "incompatible Mjolnir archive schema {}; this build requires schema {}",
        manifest.schema_version,
        expected_schema
    );
    ensure!(
        manifest.format == ARCHIVE_FORMAT,
        "unsupported archive format '{}'",
        manifest.format
    );
    ensure!(
        !manifest.session.id.trim().is_empty(),
        "manifest session id is empty"
    );
    validate_secret_free_map(&manifest.target.details)?;
    let mut paths = BTreeSet::new();
    for descriptor in &manifest.payloads {
        validate_archive_relative_path(Path::new(&descriptor.path))?;
        ensure!(
            descriptor.path != MANIFEST_PATH,
            "manifest cannot describe itself as a payload"
        );
        ensure!(
            paths.insert(descriptor.path.as_str()),
            "duplicate manifest payload '{}'",
            descriptor.path
        );
        ensure!(
            descriptor.size <= MAX_PAYLOAD_BYTES,
            "payload '{}' exceeds size limit",
            descriptor.path
        );
        normalized_mode(descriptor.mode)?;
        ensure!(
            is_lower_hex_sha256(&descriptor.sha256),
            "invalid SHA-256 for payload '{}'",
            descriptor.path
        );
        validate_payload_parts(descriptor, &mut paths)?;
        if let PayloadRole::NativeArtifact { relative_path } = &descriptor.role {
            validate_archive_relative_path(relative_path)?;
            ensure_not_secret_path(relative_path)?;
            ensure!(
                descriptor.path == format!("native/{}", slash_path(relative_path)?),
                "native artifact path does not match its role"
            );
        }
    }
    let canonical_count = manifest
        .payloads
        .iter()
        .filter(|payload| payload.role == PayloadRole::CanonicalSession)
        .count();
    ensure!(
        canonical_count == 1,
        "archive must contain exactly one canonical session payload"
    );
    ensure!(
        manifest.payloads.iter().any(|payload| {
            payload.role == PayloadRole::CanonicalSession && payload.path == CANONICAL_SESSION_PATH
        }),
        "canonical session payload must be stored at {CANONICAL_SESSION_PATH}"
    );

    let mut repository_ids = BTreeSet::new();
    for repository in &manifest.repositories {
        let metadata = &repository.metadata;
        validate_component(&metadata.id, "repository id")?;
        ensure!(
            repository_ids.insert(metadata.id.as_str()),
            "duplicate repository id '{}'",
            metadata.id
        );
        validate_archive_relative_path(&metadata.relative_destination)?;
        ensure!(
            !origin_contains_credentials(&metadata.origin),
            "repository '{}' origin contains credentials",
            metadata.id
        );
        let expected = [
            (
                repository.committed_bundle_path.as_str(),
                PayloadRole::GitBundle {
                    repository_id: metadata.id.clone(),
                },
            ),
            (
                repository.staged_patch_path.as_str(),
                PayloadRole::GitStagedPatch {
                    repository_id: metadata.id.clone(),
                },
            ),
            (
                repository.unstaged_patch_path.as_str(),
                PayloadRole::GitUnstagedPatch {
                    repository_id: metadata.id.clone(),
                },
            ),
            (
                repository.untracked_tar_path.as_str(),
                PayloadRole::GitUntrackedTar {
                    repository_id: metadata.id.clone(),
                },
            ),
        ];
        for (path, role) in expected {
            ensure!(
                manifest
                    .payloads
                    .iter()
                    .any(|payload| payload.path == path && payload.role == role),
                "repository '{}' is missing its {:?} payload",
                metadata.id,
                role
            );
        }
    }
    for payload in &manifest.payloads {
        let repository_id = match &payload.role {
            PayloadRole::GitBundle { repository_id }
            | PayloadRole::GitStagedPatch { repository_id }
            | PayloadRole::GitUnstagedPatch { repository_id }
            | PayloadRole::GitUntrackedTar { repository_id } => Some(repository_id),
            PayloadRole::CanonicalSession | PayloadRole::NativeArtifact { .. } => None,
        };
        if let Some(repository_id) = repository_id {
            ensure!(
                repository_ids.contains(repository_id.as_str()),
                "payload '{}' refers to unknown repository '{}'",
                payload.path,
                repository_id
            );
        }
    }
    Ok(())
}

/// Checks that a sharded payload can be reassembled exactly, and that its part
/// entries occupy names nothing else in the archive claims. Every failure here
/// is loud: a missing, extra, renamed, or reordered part cannot be read as a
/// silently truncated or scrambled payload.
fn validate_payload_parts<'a>(
    descriptor: &'a PayloadDescriptor,
    paths: &mut BTreeSet<&'a str>,
) -> Result<()> {
    if descriptor.parts.is_empty() {
        return Ok(());
    }
    ensure!(
        descriptor.parts.len() > 1,
        "payload '{}' is sharded into a single part",
        descriptor.path
    );
    let mut covered = 0_u64;
    for (index, part) in descriptor.parts.iter().enumerate() {
        validate_archive_relative_path(Path::new(&part.path))?;
        ensure!(
            part.path == payload_part_path(&descriptor.path, index),
            "payload '{}' part {index} has unexpected path '{}'",
            descriptor.path,
            part.path
        );
        ensure!(
            paths.insert(part.path.as_str()),
            "duplicate manifest payload '{}'",
            part.path
        );
        ensure!(
            part.size > 0,
            "payload '{}' part {index} is empty",
            descriptor.path
        );
        ensure!(
            is_lower_hex_sha256(&part.sha256),
            "invalid SHA-256 for payload part '{}'",
            part.path
        );
        covered = covered
            .checked_add(part.size)
            .ok_or_else(|| anyhow!("payload '{}' part sizes overflow", descriptor.path))?;
    }
    ensure!(
        covered == descriptor.size,
        "payload '{}' parts cover {covered} bytes but the payload is {} bytes",
        descriptor.path,
        descriptor.size
    );
    Ok(())
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

fn normalized_mode(mode: u32) -> Result<u32> {
    ensure!(mode & !0o7777 == 0, "invalid payload mode {mode:o}");
    // Writable archive artifacts never need setuid/setgid/sticky bits.
    Ok(mode & 0o0777)
}

fn is_lower_hex_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn digest_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn digest_reader(reader: &mut impl Read) -> Result<String> {
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer).context("hash Mjolnir archive")?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

#[cfg(unix)]
fn restrict_archive_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("set 0600 permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_archive_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("fsync archive directory {}", path.display()))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

fn validate_archive_relative_path(path: &Path) -> Result<()> {
    ensure!(!path.as_os_str().is_empty(), "archive path is empty");
    ensure!(
        !path.is_absolute(),
        "archive path '{}' is absolute",
        path.display()
    );
    for component in path.components() {
        ensure!(
            matches!(component, Component::Normal(_)),
            "unsafe archive path '{}'",
            path.display()
        );
    }
    Ok(())
}

fn slash_path(path: &Path) -> Result<String> {
    validate_archive_relative_path(path)?;
    let parts = path
        .components()
        .map(|component| match component {
            Component::Normal(part) => part
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("archive path '{}' is not UTF-8", path.display())),
            _ => unreachable!("validated normal path component"),
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(parts.join("/"))
}

pub(crate) fn validate_component(value: &str, label: &str) -> Result<()> {
    ensure!(!value.is_empty(), "{label} is empty");
    ensure!(value != "." && value != "..", "invalid {label} '{value}'");
    ensure!(
        value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
        "invalid {label} '{value}'"
    );
    Ok(())
}

/// The single interpretation of "this name holds credentials or harness
/// configuration". Everything that decides whether a path may be archived,
/// collected, or restored asks this, so a name blocked in one place is blocked
/// everywhere.
fn is_secret_like_component(component: &str) -> bool {
    let component = component.to_ascii_lowercase();
    component == ".env"
        || component.starts_with(".env.")
        || matches!(
            component.as_str(),
            ".git-credentials"
                | "auth.json"
                | "auth.toml"
                | "config.json"
                | "config.toml"
                | "credentials"
                | "settings.json"
                | "token"
                | "token.json"
        )
        || is_credentials_json(&component)
}

/// `credentials.json` on its own, or carrying that name behind a separator:
/// `.credentials.json` (Claude's canonical credential file), and the
/// `<vendor>_credentials.json` / `<vendor>-credentials.json` conventions.
fn is_credentials_json(component: &str) -> bool {
    component
        .strip_suffix("credentials.json")
        .is_some_and(|prefix| prefix.is_empty() || prefix.ends_with(['.', '-', '_']))
}

/// A path is secret-like when any component is. A component that is not a
/// plain name (`..`, a root, a prefix) cannot be interpreted, so it counts as
/// secret-like and every caller refuses it.
pub(crate) fn is_secret_like_path(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(component) => is_secret_like_component(&component.to_string_lossy()),
        _ => true,
    })
}

fn ensure_not_secret_path(path: &Path) -> Result<()> {
    for component in path.components() {
        let Component::Normal(component) = component else {
            bail!("unsafe artifact path '{}'", path.display());
        };
        ensure!(
            !is_secret_like_component(&component.to_string_lossy()),
            "refusing to archive credential/config path '{}'",
            path.display()
        );
    }
    Ok(())
}

fn validate_secret_free_map(map: &BTreeMap<String, String>) -> Result<()> {
    for key in map.keys() {
        let key_lower = key.to_ascii_lowercase();
        ensure!(
            !["token", "secret", "password", "credential", "private_key"]
                .iter()
                .any(|needle| key_lower.contains(needle)),
            "target provenance key '{key}' may contain a secret"
        );
    }
    Ok(())
}

fn origin_contains_credentials(origin: &str) -> bool {
    let Ok(url) = url::Url::parse(origin) else {
        return false;
    };
    matches!(url.scheme(), "http" | "https")
        && (!url.username().is_empty() || url.password().is_some())
}

fn redact_origin_credentials(origin: &str) -> Result<String> {
    let Ok(mut url) = url::Url::parse(origin) else {
        return Ok(origin.to_string());
    };
    if !matches!(url.scheme(), "http" | "https") {
        return Ok(origin.to_string());
    }
    if url.username().is_empty() && url.password().is_none() {
        return Ok(origin.to_string());
    }
    url.set_username("")
        .map_err(|()| anyhow!("cannot redact username from Git origin"))?;
    url.set_password(None)
        .map_err(|()| anyhow!("cannot redact password from Git origin"))?;
    Ok(url.to_string())
}

mod git;

pub(crate) use git::ensure_no_symlink_ancestors;
pub use git::{
    GitCollectionSpec, GitCommand, GitCommandRunner, GitHistoryMode, GitOutput,
    GitSnapshotProgress, NON_INTERACTIVE_GIT_ENV, NON_INTERACTIVE_GIT_SSH_COMMAND,
    REVIEW_BASELINE_REF, REVIEW_CAPTURE_REF, SystemGit, capture_worktree_tree,
    collect_git_metadata_snapshot, collect_git_snapshot, collect_git_snapshot_with_progress,
    diff_between_trees, empty_tree_id, has_origin_refs, pin_review_tree, restore_git_snapshot,
};
#[cfg(test)]
use git::{build_untracked_tar, restore_untracked_tar};
use git::{validate_untracked_tar, validate_untracked_tar_reader};

#[cfg(test)]
mod tests;
