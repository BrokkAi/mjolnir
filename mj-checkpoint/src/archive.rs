//! Versioned, verified checkpoint archives for Hel sessions.
//!
//! This module deliberately accepts native harness artifacts one file at a
//! time. Harness adapters select the session's artifacts; the archive preserves
//! their contents and filenames without guessing which names belong.

use mj_core::hex::lower_hex;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail, ensure};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

pub use mj_core::archive::*;
#[cfg(test)]
use mj_core::config::HarnessKind;

pub(super) const MANIFEST_PATH: &str = "manifest.json";
const CANONICAL_SESSION_PATH: &str = "canonical/session.json";
const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;
/// Compressible payloads larger than this are split into parts of this size.
/// DEFLATE is sequential inside one stream, so parts are what let both the
/// writer and the reader use every core on a large payload.
const PAYLOAD_PART_BYTES: usize = 16 * 1024 * 1024;
const PAYLOAD_PART_SUFFIX: &str = ".helpart.";
/// Small ZIP metadata uses fast DEFLATE. Large archive payloads use Zstandard,
/// whose independent shards keep both compression and decompression parallel.
const DEFLATE_LEVEL: i64 = 1;
const ZSTD_LEVEL: i64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositorySnapshot<T = Vec<u8>> {
    pub origin_delta_pack: Option<PathBuf>,
    pub metadata: RepositoryMetadata,
    pub committed_bundle: T,
    pub staged_patch: T,
    pub unstaged_patch: T,
    pub untracked_tar: T,
    /// Present exactly when `metadata.git_operation` is.
    pub operation_payloads: Option<GitOperationPayloads<T>>,
    /// Linked worktrees inside the checkout, parents before children.
    pub linked_worktrees: Vec<LinkedWorktreeSnapshot<T>>,
}

/// What a linked worktree holds beyond the commits in the repository's bundle:
/// the same dirty state the checkout itself captures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedWorktreeSnapshot<T = Vec<u8>> {
    pub metadata: mj_core::archive::LinkedWorktreeMetadata,
    pub staged_patch: T,
    pub unstaged_patch: T,
    pub untracked_tar: T,
    /// Present exactly when `metadata.git_operation` is.
    pub operation_payloads: Option<GitOperationPayloads<T>>,
}

/// The files of an unfinished Git operation that are not index entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitOperationPayloads<T = Vec<u8>> {
    /// The working-tree files of the unmerged paths.
    pub conflicted_worktree_tar: T,
    /// The operation's state files, named as `git rev-parse --git-path`
    /// names them.
    pub state_tar: T,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveInput<T = Vec<u8>> {
    pub session: SessionManifest,
    pub target: TargetManifest,
    pub bundle: BundleManifest,
    pub canonical_session: CanonicalSessionSnapshot,
    pub native_artifacts: Vec<NativeArtifact>,
    pub repositories: Vec<RepositorySnapshot<T>>,
}

#[derive(Clone, Copy)]
struct ArchiveInputView<'a, T = Vec<u8>> {
    session: &'a SessionManifest,
    target: &'a TargetManifest,
    bundle: &'a BundleManifest,
    canonical_session: &'a CanonicalSessionSnapshot,
    native_artifacts: &'a [NativeArtifact],
    repositories: &'a [RepositorySnapshot<T>],
}

impl<'a, T> From<&'a ArchiveInput<T>> for ArchiveInputView<'a, T> {
    fn from(input: &'a ArchiveInput<T>) -> Self {
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
    source: Option<&'a Path>,
}

/// Writes, fsyncs, atomically replaces, reopens, and verifies an archive.
///
/// Success means it is safe for the caller's close state machine to tear down
/// the target. Failure leaves an existing destination untouched whenever the
/// failure occurs before the final same-directory rename.
pub fn write_archive_atomic(path: &Path, input: &ArchiveInput) -> Result<VerifiedArchiveMetadata> {
    write_archive_atomic_with_payloads(path, input)
}

pub(crate) fn write_archive_atomic_with_payloads<T: RepositoryPayload>(
    path: &Path,
    input: &ArchiveInput<T>,
) -> Result<VerifiedArchiveMetadata> {
    write_archive_installed_view(path, input.into(), PAYLOAD_PART_BYTES)?;
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

pub fn write_archive_hashed_with_manifest(
    path: &Path,
    input: &ArchiveInput,
) -> Result<(String, ArchiveManifest)> {
    write_archive_hashed_view_with_manifest(path, input.into())
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
    Ok(write_archive_hashed_borrowed_with_manifest(
        path,
        session,
        target,
        bundle,
        canonical_session,
        native_artifacts,
        repositories,
    )?
    .0)
}

pub fn write_archive_hashed_borrowed_with_manifest(
    path: &Path,
    session: &SessionManifest,
    target: &TargetManifest,
    bundle: &BundleManifest,
    canonical_session: &CanonicalSessionSnapshot,
    native_artifacts: &[NativeArtifact],
    repositories: &[RepositorySnapshot],
) -> Result<(String, ArchiveManifest)> {
    write_archive_hashed_repository_payloads(
        path,
        session,
        target,
        bundle,
        canonical_session,
        native_artifacts,
        repositories,
    )
}

pub(crate) fn write_archive_hashed_repository_payloads<T: RepositoryPayload>(
    path: &Path,
    session: &SessionManifest,
    target: &TargetManifest,
    bundle: &BundleManifest,
    canonical_session: &CanonicalSessionSnapshot,
    native_artifacts: &[NativeArtifact],
    repositories: &[RepositorySnapshot<T>],
) -> Result<(String, ArchiveManifest)> {
    write_archive_hashed_view_with_manifest(
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
    Ok(write_archive_hashed_view_with_manifest(path, input)?.0)
}

fn write_archive_hashed_view_with_manifest<T: RepositoryPayload>(
    path: &Path,
    input: ArchiveInputView<'_, T>,
) -> Result<(String, ArchiveManifest)> {
    let manifest = write_archive_installed_view(path, input, PAYLOAD_PART_BYTES)?;
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let sha256 = digest_reader(&mut file)
        .with_context(|| format!("hash newly written archive {}", path.display()))?;
    Ok((sha256, manifest))
}

#[cfg(test)]
fn write_archive_installed_with_part_size(
    path: &Path,
    input: &ArchiveInput,
    part_bytes: usize,
) -> Result<()> {
    write_archive_installed_view(path, input.into(), part_bytes)?;
    Ok(())
}

fn write_archive_installed_view<T: RepositoryPayload>(
    path: &Path,
    input: ArchiveInputView<'_, T>,
    part_bytes: usize,
) -> Result<ArchiveManifest> {
    let (manifest, payloads) = prepare_archive_view_with_part_size(input, part_bytes)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("create archive directory {}", parent.display()))?;

    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary archive in {}", parent.display()))?;
    restrict_archive_permissions(temporary.path())?;
    write_zip(temporary.as_file_mut(), parent, &manifest, &payloads)?;
    temporary
        .as_file_mut()
        .sync_all()
        .with_context(|| format!("fsync temporary archive in {}", parent.display()))?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("atomically replace {}", path.display()))?;
    restrict_archive_permissions(path)?;
    mj_core::config::sync_directory(parent)?;
    drop(payloads);
    Ok(manifest)
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

/// Retain control and repository state while native files remain in the ZIP.
pub(crate) fn read_archive_for_restore(path: &Path) -> Result<VerifiedArchive> {
    let contents = read_verified_zip(path, PayloadRetention::Restore)?;
    Ok(VerifiedArchive {
        manifest: contents.manifest,
        payloads: contents.payloads,
        archive_sha256: digest_reader(&mut File::open(path)?)?,
    })
}

pub(crate) fn read_archive_repositories(path: &Path) -> Result<VerifiedArchive> {
    read_archive_repositories_unless(path, None)
}

/// [`read_archive_repositories`] that stops on any physical read once `probe`
/// returns true.
pub(crate) fn read_archive_repositories_unless(
    path: &Path,
    probe: Option<&ArchivePurgeCancel>,
) -> Result<VerifiedArchive> {
    let contents = read_verified_zip_cancellable(
        path,
        PayloadRetention::Repositories,
        None,
        probe.map(std::sync::Arc::clone),
    )?;
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = ArchiveFile {
        len: file.metadata()?.len(),
        file: std::sync::Arc::new(file),
        position: 0,
        start: 0,
        cancelled: None,
        probe: probe.map(std::sync::Arc::clone),
    };
    Ok(VerifiedArchive {
        manifest: contents.manifest,
        payloads: contents.payloads,
        archive_sha256: digest_reader(&mut reader)?,
    })
}

/// An open checkpoint directory shared by all streamed payload copies.
/// Each copy checks the descriptor's whole size and digest as it writes.
pub struct CheckpointPayloadReader {
    archive: Archive,
}

impl CheckpointPayloadReader {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            archive: open_archive(path)?,
        })
    }

    pub fn copy_payload(
        &mut self,
        descriptor: &PayloadDescriptor,
        writer: &mut dyn Write,
    ) -> Result<()> {
        copy_payload_from_archive(&mut self.archive, descriptor, writer, None)
    }

    /// Like [`Self::copy_payload`], stopping with [`ArchiveReadCancelled`]
    /// once `cancelled` returns true. The probe runs before every write, so
    /// a large payload stops promptly.
    pub fn copy_payload_unless(
        &mut self,
        descriptor: &PayloadDescriptor,
        writer: &mut dyn Write,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<()> {
        copy_payload_from_archive(&mut self.archive, descriptor, writer, Some(cancelled))
    }
}

fn copy_payload_from_archive(
    archive: &mut Archive,
    descriptor: &PayloadDescriptor,
    writer: &mut dyn Write,
    cancelled: Option<&dyn Fn() -> bool>,
) -> Result<()> {
    struct Destination<'a> {
        writer: &'a mut dyn Write,
        cancelled: Option<&'a dyn Fn() -> bool>,
        digest: Sha256,
        size: u64,
    }
    impl Write for Destination<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.cancelled.is_some_and(|cancelled| cancelled()) {
                return Err(std::io::Error::other(ArchiveReadCancelled));
            }
            let count = self.writer.write(bytes)?;
            self.digest.update(&bytes[..count]);
            self.size += count as u64;
            Ok(count)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.writer.flush()
        }
    }
    let mut output = Destination {
        writer,
        cancelled,
        digest: Sha256::new(),
        size: 0,
    };
    if descriptor.parts.is_empty() {
        std::io::copy(&mut archive.by_name(&descriptor.path)?, &mut output)?;
    } else {
        for part in &descriptor.parts {
            std::io::copy(&mut archive.by_name(&part.path)?, &mut output)?;
        }
    }
    ensure!(
        output.size == descriptor.size,
        "native payload size changed during restore"
    );
    ensure!(
        lower_hex(output.digest.finalize()) == descriptor.sha256,
        "SHA-256 mismatch for payload '{}'",
        descriptor.path
    );
    Ok(())
}

/// Shared probe for a legacy untracked purge.
///
/// Returning true stops verification and rewriting. The daemon's probe is
/// true when the HTTP client has disconnected or the upgrade gate has closed.
pub type ArchivePurgeCancel = std::sync::Arc<dyn Fn() -> bool + Send + Sync>;

/// A read stopped because [`ArchivePurgeCancel`] asked it to.
#[derive(Debug)]
pub struct ArchiveReadCancelled;

impl std::fmt::Display for ArchiveReadCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("archive verification cancelled")
    }
}

impl std::error::Error for ArchiveReadCancelled {}

/// Whether `error` is a read that [`ArchiveReadCancelled`] stopped.
pub fn archive_verification_cancelled(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<ArchiveReadCancelled>().is_some()
            || cause
                .downcast_ref::<std::io::Error>()
                .and_then(|io| io.get_ref())
                .is_some_and(|inner| inner.downcast_ref::<ArchiveReadCancelled>().is_some())
    })
}

/// Verify an archive without materializing repository or native payloads.
/// Every ZIP entry is still fully read, hashed, and checked; the Git tar
/// payloads (untracked files, and an unfinished operation's files) are parsed
/// through the same path-safety validators while streaming.
///
/// A sharded tar is the one exception to streaming: its parts are held in
/// memory long enough to reassemble and parse the tar, because tar safety is
/// a property of the whole payload rather than of one part.
pub fn verify_archive_streaming(path: &Path) -> Result<VerifiedArchiveMetadata> {
    verify_archive_streaming_cancellable(
        path,
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
}

/// Cancellation is checked on every physical archive read, including parallel payload readers.
pub fn verify_archive_streaming_cancellable(
    path: &Path,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<VerifiedArchiveMetadata> {
    verify_archive_streaming_probed(path, Some(cancelled), None)
}

/// Like [`verify_archive_streaming`], stopping when `cancelled` returns true.
///
/// The probe runs on every physical archive read, including parallel payload
/// readers.
pub fn verify_archive_streaming_unless(
    path: &Path,
    cancelled: &ArchivePurgeCancel,
) -> Result<VerifiedArchiveMetadata> {
    verify_archive_streaming_probed(path, None, Some(std::sync::Arc::clone(cancelled)))
}

fn verify_archive_streaming_probed(
    path: &Path,
    cancelled: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    probe: Option<ArchivePurgeCancel>,
) -> Result<VerifiedArchiveMetadata> {
    let contents = read_verified_zip_cancellable(
        path,
        PayloadRetention::CanonicalOnly,
        cancelled.clone(),
        probe.clone(),
    )?;
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = ArchiveFile {
        len: file.metadata()?.len(),
        file: std::sync::Arc::new(file),
        position: 0,
        start: 0,
        cancelled,
        probe,
    };
    Ok(VerifiedArchiveMetadata {
        manifest: contents.manifest,
        canonical_session: contents.canonical_session,
        archive_sha256: digest_reader(&mut reader)?,
    })
}

/// Read the manifest and only the legacy Git bundle payloads from a checkpoint
/// whose complete archive digest was persisted after full verification.
///
/// Resume uses this narrow reader for its source-availability preflight. The
/// restore path still performs full payload verification before changing any
/// target state. This deliberately avoids re-hashing the archive or bundle:
/// Git validates the pack during the preflight, and restore validates every
/// payload before using it.
pub fn read_checkpoint_manifest_and_repository_bundles(
    path: &Path,
) -> Result<(ArchiveManifest, Vec<CheckpointRepositoryBundle>)> {
    let mut archive = open_archive(path)?;
    let manifest = read_checkpoint_manifest_from(&mut archive)?;
    let repositories = manifest
        .repositories
        .iter()
        .filter(|repository| repository.metadata.origin_delta.is_none())
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
    Ok((manifest, repositories))
}

pub fn read_checkpoint_repository_bundles(path: &Path) -> Result<Vec<CheckpointRepositoryBundle>> {
    let (_, bundles) = read_checkpoint_manifest_and_repository_bundles(path)?;
    Ok(bundles)
}

/// Read the manifest of an archive that the caller has already verified.
pub fn read_checkpoint_manifest(path: &Path) -> Result<ArchiveManifest> {
    let mut archive = open_archive(path)?;
    read_checkpoint_manifest_from(&mut archive)
}

/// Whether the archive manifest records any `OriginDelta` repository snapshot.
pub fn checkpoint_archive_uses_origin_delta(path: &Path) -> Result<bool> {
    let manifest = read_checkpoint_manifest(path)?;
    Ok(manifest_uses_origin_delta(&manifest))
}

pub fn manifest_uses_origin_delta(manifest: &ArchiveManifest) -> bool {
    carries_origin_delta(&manifest.repositories)
}

/// Read one payload from an archive whose manifest was already verified.
///
/// The limit is checked before allocating or reading payload data. The entry
/// sizes and the complete payload digest are checked again against the
/// descriptor so a changed archive cannot rely on the earlier verification.
pub fn read_verified_checkpoint_payload(
    path: &Path,
    descriptor: &PayloadDescriptor,
    max_bytes: u64,
) -> Result<Vec<u8>> {
    ensure!(
        descriptor.size <= max_bytes,
        "payload '{}' exceeds read limit of {max_bytes} bytes",
        descriptor.path
    );
    if !descriptor.parts.is_empty() {
        let mut covered = 0u64;
        for part in &descriptor.parts {
            ensure!(
                part.size <= max_bytes,
                "payload part '{}' exceeds read limit of {max_bytes} bytes",
                part.path
            );
            covered = covered
                .checked_add(part.size)
                .context("payload part sizes overflow")?;
            ensure!(
                covered <= descriptor.size,
                "payload parts exceed declared size"
            );
        }
        ensure!(
            covered == descriptor.size,
            "payload parts do not cover declared size"
        );
    }

    let mut archive = open_archive(path)?;
    let bytes = read_checkpoint_payload(&mut archive, descriptor)?;
    ensure!(
        digest_bytes(&bytes) == descriptor.sha256,
        "SHA-256 mismatch for payload '{}'",
        descriptor.path
    );
    Ok(bytes)
}

fn read_checkpoint_manifest_from(archive: &mut Archive) -> Result<ArchiveManifest> {
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
    parse_archive_manifest(&manifest_bytes)
}

fn read_checkpoint_payload(
    archive: &mut Archive,
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
    archive: &mut Archive,
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

fn prepare_archive_view_with_part_size<'a, T: RepositoryPayload>(
    input: ArchiveInputView<'a, T>,
    part_bytes: usize,
) -> Result<(ArchiveManifest, Vec<PendingPayload<'a>>)> {
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
        let archive_path = format!("native/{}", slash_path(&artifact.relative_path)?);
        push_payload(
            &mut payloads,
            archive_path,
            Cow::Borrowed(match &artifact.data {
                NativeArtifactData::Bytes(bytes) => bytes.as_slice(),
                NativeArtifactData::File(_) => &[],
            }),
            artifact.mode,
            PayloadRole::NativeArtifact {
                relative_path: artifact.relative_path.clone(),
            },
        )?;
        if let NativeArtifactData::File(source) = &artifact.data {
            let payload = payloads.last_mut().expect("just inserted native payload");
            payload.source = Some(source);
            payload.descriptor.size = artifact.size()?;
        }
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
        if let Some(subdirectory) = &repository.metadata.checkout_subdirectory {
            validate_archive_relative_path(subdirectory)?;
        }
        ensure!(
            !origin_contains_credentials(&repository.metadata.origin),
            "repository '{}' origin contains credentials",
            repository.metadata.id
        );
        for push_url in &repository.metadata.push_urls {
            ensure!(
                !origin_contains_credentials(push_url),
                "repository '{}' push URL contains credentials",
                repository.metadata.id
            );
        }
        if repository.metadata.remote_workspace {
            ensure!(
                !repository.metadata.base_commit.is_empty(),
                "repository '{}' managed workspace has no base commit",
                repository.metadata.id
            );
        }
        if repository.metadata.plain_directory {
            // Nothing about a plain folder is captured, so any Git state here
            // would be state a restore could not honestly reproduce.
            ensure!(
                !repository.metadata.remote_workspace
                    && repository.metadata.checkout_subdirectory.is_none()
                    && repository.metadata.origin.is_empty()
                    && repository.metadata.push_urls.is_empty()
                    && repository.metadata.base_commit.is_empty()
                    && repository.metadata.head_commit.is_empty()
                    && repository.metadata.branch.is_none()
                    && !repository.metadata.review_refs_metadata_only
                    && repository.metadata.review_baseline_ref.is_none()
                    && repository.metadata.saved_refs.is_empty()
                    && repository.metadata.stash_stack.is_empty()
                    && repository.metadata.git_operation.is_none()
                    && repository.committed_bundle.size()? == 0
                    && repository.staged_patch.size()? == 0
                    && repository.unstaged_patch.size()? == 0
                    && repository.untracked_tar.size()? == 0
                    && repository.operation_payloads.is_none(),
                "repository '{}' is a plain directory but carries Git state",
                repository.metadata.id
            );
        }
        let root = format!("repositories/{}", repository.metadata.id);
        if repository.metadata.origin_delta.is_some() {
            ensure!(
                repository.committed_bundle.size()? == 0
                    && repository.staged_patch.size()? == 0
                    && repository.unstaged_patch.size()? == 0
                    && repository.untracked_tar.size()? == 0
                    && repository.operation_payloads.is_none()
                    && repository.linked_worktrees.is_empty(),
                "origin delta repository has legacy Git payloads"
            );
            let origin_delta_pack_path = repository
                .origin_delta_pack
                .as_deref()
                .map(|pack| -> Result<String> {
                    let path = format!("{root}/origin.pack");
                    push_payload(
                        &mut payloads,
                        path.clone(),
                        Cow::Borrowed(&[]),
                        0o600,
                        PayloadRole::GitOriginDeltaPack {
                            repository_id: repository.metadata.id.clone(),
                        },
                    )?;
                    let payload = payloads.last_mut().expect("just inserted pack payload");
                    payload.source = Some(pack);
                    payload.descriptor.size = fs::metadata(pack)?.len();
                    Ok(path)
                })
                .transpose()?;
            repositories.push(RepositoryManifest {
                metadata: repository.metadata.clone(),
                committed_bundle_path: String::new(),
                staged_patch_path: String::new(),
                unstaged_patch_path: String::new(),
                untracked_tar_path: String::new(),
                conflicted_worktree_tar_path: None,
                operation_state_tar_path: None,
                linked_worktrees: Vec::new(),
                origin_delta_pack_path,
            });
            continue;
        }
        ensure!(
            repository.origin_delta_pack.is_none(),
            "legacy repository has an origin delta pack"
        );
        let operation = match (
            &repository.metadata.git_operation,
            &repository.operation_payloads,
        ) {
            (None, None) => None,
            (Some(operation), Some(operation_payloads)) => {
                validate_saved_git_operation(operation)?;
                Some(operation_payloads)
            }
            _ => bail!(
                "repository '{}' Git operation metadata and payloads disagree",
                repository.metadata.id
            ),
        };

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
            push_repository_payload(&mut payloads, path.clone(), data, role)?;
        }
        let (conflicted_worktree_tar_path, operation_state_tar_path) = match operation {
            None => (None, None),
            Some(operation) => {
                let worktree_path = format!("{root}/conflicted-worktree.tar");
                let state_path = format!("{root}/operation-state.tar");
                push_repository_payload(
                    &mut payloads,
                    worktree_path.clone(),
                    &operation.conflicted_worktree_tar,
                    PayloadRole::GitConflictedWorktreeTar {
                        repository_id: repository.metadata.id.clone(),
                    },
                )?;
                push_repository_payload(
                    &mut payloads,
                    state_path.clone(),
                    &operation.state_tar,
                    PayloadRole::GitOperationStateTar {
                        repository_id: repository.metadata.id.clone(),
                    },
                )?;
                (Some(worktree_path), Some(state_path))
            }
        };
        let mut linked_worktrees = Vec::with_capacity(repository.linked_worktrees.len());
        let mut worktree_paths = BTreeSet::new();
        for (index, worktree) in repository.linked_worktrees.iter().enumerate() {
            validate_linked_worktree_metadata(&worktree.metadata)
                .with_context(|| format!("repository '{}'", repository.metadata.id))?;
            ensure!(
                worktree_paths.insert(worktree.metadata.path.clone()),
                "repository '{}' lists linked worktree '{}' twice",
                repository.metadata.id,
                worktree.metadata.path.display()
            );
            let worktree_root = format!("{root}/worktrees/{index:08}");
            let id = repository.metadata.id.clone();
            let worktree_index = index as u32;
            let staged_patch_path = format!("{worktree_root}/staged.patch");
            let unstaged_patch_path = format!("{worktree_root}/unstaged.patch");
            let untracked_tar_path = format!("{worktree_root}/untracked.tar");
            for (path, data, role) in [
                (
                    &staged_patch_path,
                    &worktree.staged_patch,
                    PayloadRole::GitLinkedWorktreeStagedPatch {
                        repository_id: id.clone(),
                        worktree: worktree_index,
                    },
                ),
                (
                    &unstaged_patch_path,
                    &worktree.unstaged_patch,
                    PayloadRole::GitLinkedWorktreeUnstagedPatch {
                        repository_id: id.clone(),
                        worktree: worktree_index,
                    },
                ),
                (
                    &untracked_tar_path,
                    &worktree.untracked_tar,
                    PayloadRole::GitLinkedWorktreeUntrackedTar {
                        repository_id: id.clone(),
                        worktree: worktree_index,
                    },
                ),
            ] {
                push_repository_payload(&mut payloads, path.clone(), data, role)?;
            }
            let (conflicted_worktree_tar_path, operation_state_tar_path) = match (
                &worktree.metadata.git_operation,
                &worktree.operation_payloads,
            ) {
                (None, None) => (None, None),
                (Some(_), Some(operation)) => {
                    let worktree_path = format!("{worktree_root}/conflicted-worktree.tar");
                    let state_path = format!("{worktree_root}/operation-state.tar");
                    push_repository_payload(
                        &mut payloads,
                        worktree_path.clone(),
                        &operation.conflicted_worktree_tar,
                        PayloadRole::GitLinkedWorktreeConflictedWorktreeTar {
                            repository_id: id.clone(),
                            worktree: worktree_index,
                        },
                    )?;
                    push_repository_payload(
                        &mut payloads,
                        state_path.clone(),
                        &operation.state_tar,
                        PayloadRole::GitLinkedWorktreeOperationStateTar {
                            repository_id: id.clone(),
                            worktree: worktree_index,
                        },
                    )?;
                    (Some(worktree_path), Some(state_path))
                }
                _ => bail!(
                    "repository '{}' linked worktree '{}' Git operation metadata and payloads disagree",
                    repository.metadata.id,
                    worktree.metadata.path.display()
                ),
            };
            linked_worktrees.push(mj_core::archive::LinkedWorktreeManifest {
                metadata: worktree.metadata.clone(),
                staged_patch_path,
                unstaged_patch_path,
                untracked_tar_path,
                conflicted_worktree_tar_path,
                operation_state_tar_path,
            });
        }
        repositories.push(RepositoryManifest {
            origin_delta_pack_path: None,
            metadata: repository.metadata.clone(),
            committed_bundle_path,
            staged_patch_path,
            unstaged_patch_path,
            untracked_tar_path,
            conflicted_worktree_tar_path,
            operation_state_tar_path,
            linked_worktrees,
        });
    }

    payloads
        .par_iter_mut()
        .try_for_each(|payload| -> Result<()> {
            if payload.source.is_some() {
                hash_file_payload(payload, part_bytes)?;
            } else {
                payload.descriptor.sha256 = digest_bytes(&payload.data);
                payload.descriptor.parts = plan_payload_parts(
                    &payload.descriptor.path,
                    &payload.data,
                    payload_compression(&payload.descriptor.role),
                    part_bytes,
                );
            }
            Ok(())
        })?;

    let mut paths = BTreeSet::new();
    for payload in &payloads {
        ensure!(
            paths.insert(payload.descriptor.path.clone()),
            "duplicate archive payload path '{}'",
            payload.descriptor.path
        );
    }
    let descriptors: Vec<_> = payloads
        .iter()
        .map(|payload| payload.descriptor.clone())
        .collect();
    let manifest = ArchiveManifest {
        schema_version: if input.canonical_session.pending_prompt_context.is_some() {
            ARCHIVE_SCHEMA_VERSION_PROMPT_CONTEXT
        } else if input.canonical_session.native_use_evidence.is_some() {
            ARCHIVE_SCHEMA_VERSION_NATIVE_USE
        } else if carries_origin_delta(&repositories) {
            ARCHIVE_SCHEMA_VERSION_ORIGIN_DELTA
        } else if input.canonical_session.mailbox.is_some() {
            ARCHIVE_SCHEMA_VERSION_MAILBOX
        } else if input.canonical_session.notification_stack.is_some() {
            ARCHIVE_SCHEMA_VERSION_NOTIFICATION_STACK
        } else if carries_linked_worktrees(&repositories) {
            ARCHIVE_SCHEMA_VERSION_LINKED_WORKTREES
        } else if carries_review_refs(&repositories) {
            ARCHIVE_SCHEMA_VERSION_REVIEW_REFS
        } else if input
            .canonical_session
            .transcript
            .iter()
            .any(|item| matches!(item.body, CanonicalTranscriptBody::Message { .. }))
        {
            ARCHIVE_SCHEMA_VERSION_MESSAGES
        } else if carries_checkout_subdirectory(&repositories) {
            ARCHIVE_SCHEMA_VERSION_CHECKOUT_SUBDIRECTORY
        } else if carries_agent_reports(&descriptors) {
            ARCHIVE_SCHEMA_VERSION_AGENT_REPORTS
        } else if repositories.iter().any(|repo| {
            !repo.metadata.saved_refs.is_empty() || !repo.metadata.stash_stack.is_empty()
        }) {
            ARCHIVE_SCHEMA_VERSION_CLONE_REFS
        } else if input.canonical_session.current_context_start() > 0 {
            ARCHIVE_SCHEMA_VERSION_CONTEXT
        } else {
            expected_schema_version(&descriptors)
        },
        format: ARCHIVE_FORMAT.to_string(),
        session: (*input.session).clone(),
        target: (*input.target).clone(),
        bundle: (*input.bundle).clone(),
        repositories,
        payloads: descriptors,
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
    let descriptor = PayloadDescriptor {
        path,
        sha256: String::new(),
        size: data.len() as u64,
        mode: normalized_mode(mode)?,
        role,
        parts: Vec::new(),
    };
    payloads.push(PendingPayload {
        descriptor,
        data,
        source: None,
    });
    Ok(())
}

#[derive(Debug)]
pub(crate) enum StagedPayload {
    Inline(Vec<u8>),
    File(PathBuf),
}

impl Default for StagedPayload {
    fn default() -> Self {
        Self::Inline(Vec::new())
    }
}

pub(crate) trait RepositoryPayload {
    fn source(&self) -> (&[u8], Option<&Path>);

    fn reader(&self) -> Result<Box<dyn Read + '_>> {
        let (bytes, path) = self.source();
        match path {
            Some(path) => Ok(Box::new(NativeArtifactData::open_file(path)?)),
            None => Ok(Box::new(Cursor::new(bytes))),
        }
    }

    fn size(&self) -> Result<u64> {
        let (bytes, path) = self.source();
        match path {
            Some(path) => Ok(NativeArtifactData::open_file(path)?.metadata()?.len()),
            None => Ok(bytes.len() as u64),
        }
    }
}

impl RepositoryPayload for Vec<u8> {
    fn source(&self) -> (&[u8], Option<&Path>) {
        (self, None)
    }
}

impl RepositoryPayload for StagedPayload {
    fn source(&self) -> (&[u8], Option<&Path>) {
        match self {
            Self::Inline(bytes) => (bytes, None),
            Self::File(path) => (&[], Some(path)),
        }
    }
}

fn hash_file_payload(payload: &mut PendingPayload<'_>, part_bytes: usize) -> Result<()> {
    let split = payload_compression(&payload.descriptor.role) != CompressionMethod::Stored
        && payload.descriptor.size > part_bytes as u64;
    let mut whole = Sha256::new();
    let mut remaining = payload.descriptor.size;
    let mut parts = Vec::new();
    {
        let mut source = payload.reader_at(0)?;
        let mut buffer = [0; 64 * 1024];
        while remaining > 0 {
            let size = if split {
                remaining.min(part_bytes as u64)
            } else {
                remaining
            };
            let mut part = Sha256::new();
            let mut unread = size;
            while unread > 0 {
                let count = unread.min(buffer.len() as u64) as usize;
                source
                    .read_exact(&mut buffer[..count])
                    .with_context(|| format!("hash payload '{}'", payload.descriptor.path))?;
                whole.update(&buffer[..count]);
                if split {
                    part.update(&buffer[..count]);
                }
                unread -= count as u64;
            }
            if split {
                parts.push(PayloadPartDescriptor {
                    path: payload_part_path(&payload.descriptor.path, parts.len()),
                    sha256: lower_hex(part.finalize()),
                    size,
                });
            }
            remaining -= size;
        }
        ensure!(
            source.read(&mut [0])? == 0,
            "payload source '{}' grew during export",
            payload.descriptor.path
        );
    }
    payload.descriptor.parts = parts;
    payload.descriptor.sha256 = lower_hex(whole.finalize());
    Ok(())
}

fn push_repository_payload<'a>(
    payloads: &mut Vec<PendingPayload<'a>>,
    path: String,
    data: &'a impl RepositoryPayload,
    role: PayloadRole,
) -> Result<()> {
    let (bytes, source) = data.source();
    push_payload(payloads, path, Cow::Borrowed(bytes), 0o600, role)?;
    let payload = payloads
        .last_mut()
        .expect("just inserted repository payload");
    payload.source = source;
    payload.descriptor.size = data.size()?;
    if let Some(validate) = tar_payload_validator(&payload.descriptor.role) {
        validate(&mut payload.reader_at(0)?)?;
    }
    Ok(())
}

/// Git bundles carry packfiles whose objects are already compressed, so they
/// are stored verbatim. Text and tar payloads compress well with Zstandard,
/// which is substantially faster than DEFLATE for checkpoint-sized history.
fn payload_compression(role: &PayloadRole) -> CompressionMethod {
    match role {
        PayloadRole::GitBundle { .. } | PayloadRole::GitOriginDeltaPack { .. } => {
            CompressionMethod::Stored
        }
        PayloadRole::CanonicalSession
        | PayloadRole::NativeArtifact { .. }
        | PayloadRole::GitStagedPatch { .. }
        | PayloadRole::GitUnstagedPatch { .. }
        | PayloadRole::GitUntrackedTar { .. }
        | PayloadRole::GitConflictedWorktreeTar { .. }
        | PayloadRole::GitOperationStateTar { .. }
        | PayloadRole::GitLinkedWorktreeStagedPatch { .. }
        | PayloadRole::GitLinkedWorktreeUnstagedPatch { .. }
        | PayloadRole::GitLinkedWorktreeUntrackedTar { .. }
        | PayloadRole::GitLinkedWorktreeConflictedWorktreeTar { .. }
        | PayloadRole::GitLinkedWorktreeOperationStateTar { .. } => CompressionMethod::Zstd,
    }
}

/// The tar payloads whose entries are checked while an archive is verified,
/// with the checker for each.
fn tar_payload_validator(role: &PayloadRole) -> Option<fn(&mut dyn Read) -> Result<()>> {
    match role {
        PayloadRole::GitUntrackedTar { .. } | PayloadRole::GitLinkedWorktreeUntrackedTar { .. } => {
            Some(|reader| validate_untracked_tar_reader(reader))
        }
        PayloadRole::GitConflictedWorktreeTar { .. }
        | PayloadRole::GitLinkedWorktreeConflictedWorktreeTar { .. } => {
            Some(|reader| validate_conflicted_worktree_tar_reader(reader))
        }
        PayloadRole::GitOperationStateTar { .. }
        | PayloadRole::GitLinkedWorktreeOperationStateTar { .. } => {
            Some(|reader| validate_operation_state_tar_reader(reader))
        }
        PayloadRole::CanonicalSession
        | PayloadRole::NativeArtifact { .. }
        | PayloadRole::GitBundle { .. }
        | PayloadRole::GitOriginDeltaPack { .. }
        | PayloadRole::GitStagedPatch { .. }
        | PayloadRole::GitUnstagedPatch { .. }
        | PayloadRole::GitLinkedWorktreeStagedPatch { .. }
        | PayloadRole::GitLinkedWorktreeUnstagedPatch { .. } => None,
    }
}

pub(super) fn payload_part_path(path: &str, index: usize) -> String {
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

fn write_zip(
    output: &mut File,
    spool_parent: &Path,
    manifest: &ArchiveManifest,
    payloads: &[PendingPayload<'_>],
) -> Result<()> {
    let manifest_bytes =
        serde_json::to_vec_pretty(manifest).context("serialize archive manifest")?;
    ensure!(
        manifest_bytes.len() as u64 <= MAX_MANIFEST_BYTES,
        "archive manifest is too large"
    );
    let mut writer = zip::ZipWriter::new(output);
    write_stream_entry(
        &mut writer,
        MANIFEST_PATH,
        0o600,
        CompressionMethod::Deflated,
        manifest_bytes.len() as u64,
        &mut Cursor::new(&manifest_bytes),
        &digest_bytes(&manifest_bytes),
    )?;
    let spool_builder = tempfile::Builder::new();
    #[cfg(unix)]
    let spool_builder = {
        use std::os::unix::fs::PermissionsExt;
        let mut builder = spool_builder;
        builder.permissions(fs::Permissions::from_mode(0o700));
        builder
    };
    let spool = spool_builder
        .tempdir_in(spool_parent)
        .context("create ZIP compression spool")?;
    let mut entries = Vec::new();
    for payload in payloads {
        let descriptor = &payload.descriptor;
        if descriptor.parts.is_empty() {
            entries.push(StreamEntry {
                payload,
                name: &descriptor.path,
                size: descriptor.size,
                digest: &descriptor.sha256,
                offset: 0,
            });
        } else {
            let mut offset = 0_u64;
            for part in &descriptor.parts {
                entries.push(StreamEntry {
                    payload,
                    name: &part.path,
                    size: part.size,
                    digest: &part.sha256,
                    offset,
                });
                offset = offset
                    .checked_add(part.size)
                    .context("payload parts overflow")?;
            }
            ensure!(
                offset == descriptor.size,
                "payload parts do not cover its body"
            );
        }
    }
    // Reuse Rayon's compression parallelism. One spool per active worker bounds
    // scratch disk to a worker-sized window instead of the entire archive.
    for window in entries.chunks(rayon::current_num_threads()) {
        let compressed = window
            .par_iter()
            .map(|entry| {
                // Stored Git bundles are already compressed and unsharded;
                // spooling them would duplicate a potentially large bundle.
                if payload_compression(&entry.payload.descriptor.role) == CompressionMethod::Stored
                {
                    Ok(None)
                } else {
                    compress_stream_entry(entry, spool.path())
                        .map(Some)
                        .with_context(|| format!("compress ZIP entry '{}'", entry.name))
                }
            })
            .collect::<Result<Vec<_>>>()?;
        append_stream_entries(&mut writer, window, compressed)?;
    }

    writer.finish().context("finish Mjolnir archive ZIP")?;
    Ok(())
}

/// Assembly owns payload completion: workers may finish parts out of order,
/// so only this ordered pass can check EOF after every part has completed.
fn append_stream_entries(
    writer: &mut zip::ZipWriter<&mut File>,
    entries: &[StreamEntry<'_>],
    compressed: Vec<Option<tempfile::NamedTempFile>>,
) -> Result<()> {
    for (entry, temporary) in entries.iter().zip(compressed) {
        if let Some(temporary) = temporary {
            let mut source = zip::ZipArchive::new(temporary.reopen()?)
                .with_context(|| format!("reopen compressed ZIP entry '{}'", entry.name))?;
            let compressed_entry = source
                .by_index(0)
                .with_context(|| format!("read compressed ZIP entry '{}'", entry.name))?;
            writer
                .raw_copy_file(compressed_entry)
                .with_context(|| format!("write ZIP entry '{}'", entry.name))?;
        } else {
            write_payload_entry(writer, entry)
                .with_context(|| format!("write ZIP entry '{}'", entry.name))?;
        }
        if entry.offset + entry.size == entry.payload.descriptor.size {
            let mut source = entry
                .payload
                .reader_at(entry.payload.descriptor.size)
                .with_context(|| {
                    format!(
                        "reopen completed payload '{}'",
                        entry.payload.descriptor.path
                    )
                })?;
            ensure!(
                source.read(&mut [0u8; 1])? == 0,
                "payload source grew during export"
            );
        }
    }
    Ok(())
}

struct StreamEntry<'a> {
    payload: &'a PendingPayload<'a>,
    name: &'a str,
    size: u64,
    digest: &'a str,
    offset: u64,
}

/// A private single-entry ZIP lets assembly reuse the codec output and CRC
/// through raw_copy_file without retaining compressed payloads in memory.
fn compress_stream_entry(
    entry: &StreamEntry<'_>,
    directory: &Path,
) -> Result<tempfile::NamedTempFile> {
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    let mut writer = zip::ZipWriter::new(temporary.as_file_mut());
    write_payload_entry(&mut writer, entry)?;
    writer.finish().context("finish compressed ZIP entry")?;
    Ok(temporary)
}

impl PendingPayload<'_> {
    fn reader_at(&self, offset: u64) -> Result<Box<dyn Read + '_>> {
        Ok(match self.source {
            Some(path) => {
                let mut file = NativeArtifactData::open_file(path)?;
                file.seek(SeekFrom::Start(offset))?;
                Box::new(file)
            }
            None => {
                let mut cursor = Cursor::new(self.data.as_ref());
                cursor.set_position(offset);
                Box::new(cursor)
            }
        })
    }
}

fn write_payload_entry(
    writer: &mut zip::ZipWriter<&mut File>,
    entry: &StreamEntry<'_>,
) -> Result<()> {
    let mut source = entry.payload.reader_at(entry.offset)?;
    write_stream_entry(
        writer,
        entry.name,
        entry.payload.descriptor.mode,
        payload_compression(&entry.payload.descriptor.role),
        entry.size,
        &mut source.by_ref().take(entry.size),
        entry.digest,
    )?;
    Ok(())
}

pub(super) fn write_stream_entry(
    writer: &mut zip::ZipWriter<&mut File>,
    name: &str,
    mode: u32,
    method: CompressionMethod,
    size: u64,
    reader: &mut dyn Read,
    expected_digest: &str,
) -> Result<()> {
    writer.start_file(
        name,
        SimpleFileOptions::default()
            .compression_method(method)
            .compression_level(match method {
                CompressionMethod::Deflated => Some(DEFLATE_LEVEL),
                CompressionMethod::Zstd => Some(ZSTD_LEVEL),
                _ => None,
            })
            .unix_permissions(mode)
            // ZIP's documented 32-bit size threshold requires ZIP64 beyond this value.
            .large_file(size > zip::ZIP64_BYTES_THR),
    )?;
    let mut checked = DigestingReader::new(reader);
    ensure!(
        std::io::copy(&mut checked, writer)? == size,
        "payload source size changed"
    );
    ensure!(
        checked.finish().1 == expected_digest,
        "payload source changed during export"
    );
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PayloadRetention {
    All,
    Restore,
    Repositories,
    CanonicalOnly,
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

type Archive = zip::ZipArchive<ArchiveFile>;

/// Opening parses the ZIP's whole central directory. A parallel reader needs
/// its own read position, not its own parse: clone the opened [`Archive`],
/// which shares the parsed directory and this file.
fn open_archive(path: &Path) -> Result<Archive> {
    open_archive_cancellable(path, None, None)
}

fn open_archive_probed(path: &Path, probe: &ArchivePurgeCancel) -> Result<Archive> {
    open_archive_cancellable(path, None, Some(std::sync::Arc::clone(probe)))
}

fn open_archive_cancellable(
    path: &Path,
    cancelled: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    probe: Option<ArchivePurgeCancel>,
) -> Result<Archive> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let len = file
        .metadata()
        .with_context(|| format!("read the size of {}", path.display()))?
        .len();
    zip::ZipArchive::new(ArchiveFile {
        file: std::sync::Arc::new(file),
        len,
        position: 0,
        start: 0,
        cancelled,
        probe,
    })
    .context("open Mjolnir archive ZIP")
}

/// A file read at explicit offsets, so clones share one open file and each
/// keeps its own position. A duplicated handle would share the position.
#[derive(Clone)]
struct ArchiveFile {
    cancelled: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    probe: Option<ArchivePurgeCancel>,
    file: std::sync::Arc<File>,
    len: u64,
    position: u64,
    start: u64,
}

impl Read for ArchiveFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self
            .cancelled
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire))
            || self.probe.as_ref().is_some_and(|probe| probe())
        {
            return Err(std::io::Error::other(ArchiveReadCancelled));
        }
        let count = usize::try_from(self.len.saturating_sub(self.position))
            .unwrap_or(usize::MAX)
            .min(buf.len());
        let offset = self
            .start
            .checked_add(self.position)
            .ok_or_else(|| std::io::Error::other("file read offset overflow"))?;
        #[cfg(unix)]
        let read = std::os::unix::fs::FileExt::read_at(&*self.file, &mut buf[..count], offset)?;
        #[cfg(windows)]
        let read =
            std::os::windows::fs::FileExt::seek_read(&*self.file, &mut buf[..count], offset)?;
        self.position += read as u64;
        Ok(read)
    }
}

impl std::io::Seek for ArchiveFile {
    fn seek(&mut self, to: std::io::SeekFrom) -> std::io::Result<u64> {
        let (base, offset) = match to {
            std::io::SeekFrom::Start(position) => {
                self.position = position;
                return Ok(position);
            }
            std::io::SeekFrom::End(offset) => (self.len, offset),
            std::io::SeekFrom::Current(offset) => (self.position, offset),
        };
        self.position = base.checked_add_signed(offset).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before the start of the archive",
            )
        })?;
        Ok(self.position)
    }
}

/// Payload bodies the caller keeps.
fn retains_payload(retention: PayloadRetention, role: &PayloadRole) -> bool {
    retention == PayloadRetention::All
        || (retention == PayloadRetention::Repositories
            && !matches!(
                role,
                PayloadRole::NativeArtifact { .. } | PayloadRole::GitOriginDeltaPack { .. }
            ))
        || (retention == PayloadRetention::Restore
            && match role {
                PayloadRole::NativeArtifact { relative_path } => {
                    relative_path.starts_with(mj_core::attachment::ARCHIVE_ATTACHMENT_DIR)
                        || relative_path.starts_with(mj_core::subagent::ARCHIVE_REPORT_DIR)
                        || relative_path.starts_with(mj_core::goal::NATIVE_ARTIFACT_ROOT)
                }
                PayloadRole::GitOriginDeltaPack { .. } => false,
                _ => true,
            })
        || *role == PayloadRole::CanonicalSession
}

/// Part bodies the reader has to hold until reassembly. Tar safety is a
/// property of the whole payload, so a sharded tar is reassembled even when
/// the caller does not want the bytes.
fn retains_parts(retention: PayloadRetention, role: &PayloadRole) -> bool {
    retains_payload(retention, role) || tar_payload_validator(role).is_some()
}

fn read_verified_zip(path: &Path, retention: PayloadRetention) -> Result<VerifiedZipContents> {
    read_verified_zip_cancellable(path, retention, None, None)
}

fn read_verified_zip_cancellable(
    path: &Path,
    retention: PayloadRetention,
    cancelled: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    probe: Option<ArchivePurgeCancel>,
) -> Result<VerifiedZipContents> {
    let mut archive = open_archive_cancellable(path, cancelled, probe)?;
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
        total_size = total_size
            .checked_add(entry.size())
            .ok_or_else(|| anyhow!("archive expanded size overflow"))?;
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
    // parallelism comes from entries: each worker reads through its own clone
    // of the archive, sharing the directory parsed above, and verifies whole
    // entries end to end.
    let outcomes = payload_entries
        .par_iter()
        .map_init(
            || archive.clone(),
            |archive, (meta, expectation)| {
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
        // Per-part hashes do not prove the whole-payload digest. Verify omitted
        // sharded bodies in order before returning any trusted archive state.
        if !descriptor.parts.is_empty() && !retains_parts(retention, &descriptor.role) {
            copy_payload_from_archive(&mut archive, descriptor, &mut std::io::sink(), None)?;
        }
        let bytes = reassemble_payload(descriptor, &mut bodies, retention)?;
        if let Some(validate) = tar_payload_validator(&descriptor.role)
            && let Some(bytes) = bytes.as_deref()
        {
            validate(&mut Cursor::new(bytes))
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
                manifest.schema_version >= ARCHIVE_SCHEMA_VERSION_MESSAGES
                    || !snapshot
                        .transcript
                        .iter()
                        .any(|item| matches!(item.body, CanonicalTranscriptBody::Message { .. })),
                "message transcript entries require archive schema {}",
                ARCHIVE_SCHEMA_VERSION_MESSAGES
            );
            ensure!(
                manifest.schema_version >= ARCHIVE_SCHEMA_VERSION_NOTIFICATION_STACK
                    || snapshot.notification_stack.is_none(),
                "notification stack requires archive schema {}",
                ARCHIVE_SCHEMA_VERSION_NOTIFICATION_STACK
            );
            ensure!(
                manifest.schema_version >= ARCHIVE_SCHEMA_VERSION_MAILBOX
                    || snapshot.mailbox.is_none(),
                "archived mailbox requires archive schema {}",
                ARCHIVE_SCHEMA_VERSION_MAILBOX
            );
            ensure!(
                manifest.schema_version >= ARCHIVE_SCHEMA_VERSION_NATIVE_USE
                    || snapshot.native_use_evidence.is_none(),
                "native-session-use evidence requires archive schema {}",
                ARCHIVE_SCHEMA_VERSION_NATIVE_USE
            );
            ensure!(
                manifest.schema_version != ARCHIVE_SCHEMA_VERSION_NATIVE_USE
                    || snapshot.native_use_evidence.is_some(),
                "archive schema {} requires native-session-use evidence",
                ARCHIVE_SCHEMA_VERSION_NATIVE_USE
            );
            ensure!(
                manifest.schema_version >= ARCHIVE_SCHEMA_VERSION_PROMPT_CONTEXT
                    || snapshot.pending_prompt_context.is_none(),
                "pending prompt context requires archive schema {}",
                ARCHIVE_SCHEMA_VERSION_PROMPT_CONTEXT
            );
            ensure!(
                manifest.schema_version != ARCHIVE_SCHEMA_VERSION_PROMPT_CONTEXT
                    || snapshot.pending_prompt_context.is_some(),
                "archive schema {} requires pending prompt context",
                ARCHIVE_SCHEMA_VERSION_PROMPT_CONTEXT
            );
            ensure!(
                canonical_session.replace(snapshot).is_none(),
                "archive contains duplicate canonical session payloads"
            );
        }
        if retains_payload(retention, &descriptor.role) {
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
    archive: &mut Archive,
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
    // Only a whole tar can be parsed while it streams past; sharded ones are
    // reassembled after every part is verified.
    let stream_tar = tar_payload_validator(&payload.role)
        .filter(|_| !retain && matches!(expectation, EntryExpectation::Whole(_)));

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
    } else if let Some(validate) = stream_tar {
        validate(&mut digesting).with_context(|| format!("validate payload '{}'", payload.path))?;
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
        (self.bytes_read, lower_hex(self.digest.finalize()))
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
            || header.schema_version == ARCHIVE_SCHEMA_VERSION_SHARDED
            || header.schema_version == ARCHIVE_SCHEMA_VERSION_ATTACHMENTS
            || header.schema_version == ARCHIVE_SCHEMA_VERSION_CONTEXT
            || header.schema_version == ARCHIVE_SCHEMA_VERSION_CLONE_REFS
            || header.schema_version == ARCHIVE_SCHEMA_VERSION_AGENT_REPORTS
            || header.schema_version == ARCHIVE_SCHEMA_VERSION_CHECKOUT_SUBDIRECTORY
            || header.schema_version == ARCHIVE_SCHEMA_VERSION_MESSAGES
            || header.schema_version == ARCHIVE_SCHEMA_VERSION_REVIEW_REFS
            || header.schema_version == ARCHIVE_SCHEMA_VERSION_LINKED_WORKTREES
            || header.schema_version == ARCHIVE_SCHEMA_VERSION_NOTIFICATION_STACK
            || header.schema_version == ARCHIVE_SCHEMA_VERSION_MAILBOX
            || header.schema_version == ARCHIVE_SCHEMA_VERSION_ORIGIN_DELTA
            || header.schema_version == ARCHIVE_SCHEMA_VERSION_NATIVE_USE
            || header.schema_version == ARCHIVE_SCHEMA_VERSION_PROMPT_CONTEXT,
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
/// Schema 3 adds sharding; schema 4 adds relay-owned attachments. Older
/// readers must reject either extension instead of silently restoring it wrong.
fn expected_schema_version(payloads: &[PayloadDescriptor]) -> u32 {
    if payloads.iter().any(|payload| matches!(&payload.role, PayloadRole::NativeArtifact { relative_path } if relative_path.starts_with(mj_core::attachment::ARCHIVE_ATTACHMENT_DIR))) {
        ARCHIVE_SCHEMA_VERSION_ATTACHMENTS
    } else if payloads.iter().any(|payload| !payload.parts.is_empty()) {
        ARCHIVE_SCHEMA_VERSION_SHARDED
    } else {
        ARCHIVE_SCHEMA_VERSION
    }
}

/// Whether an archive carries sub-agent report files, which only a build that
/// knows to restore them beside the repositories may read.
fn carries_agent_reports(payloads: &[PayloadDescriptor]) -> bool {
    payloads.iter().any(|payload| {
        matches!(&payload.role, PayloadRole::NativeArtifact { relative_path }
            if relative_path.starts_with(mj_core::subagent::ARCHIVE_REPORT_DIR))
    })
}

/// Whether an archive holds a repository captured from a subdirectory of its
/// checkout, which only a build that restores from the top level may read.
fn carries_checkout_subdirectory(repositories: &[RepositoryManifest]) -> bool {
    repositories
        .iter()
        .any(|repository| repository.metadata.checkout_subdirectory.is_some())
}

/// Whether an archive holds a linked worktree, which only a build that
/// restores them may read.
fn carries_linked_worktrees(repositories: &[RepositoryManifest]) -> bool {
    repositories
        .iter()
        .any(|repository| !repository.linked_worktrees.is_empty())
}

fn carries_review_refs(repositories: &[RepositoryManifest]) -> bool {
    repositories.iter().any(|repository| {
        repository.metadata.review_refs_metadata_only
            || repository.metadata.review_baseline_ref.is_some()
            || repository
                .metadata
                .saved_refs
                .keys()
                .any(|name| name.starts_with(mj_core::archive::MJOLNIR_REF_NAMESPACE))
    })
}

fn carries_origin_delta(repositories: &[RepositoryManifest]) -> bool {
    repositories
        .iter()
        .any(|repository| repository.metadata.origin_delta.is_some())
}

fn validate_manifest(manifest: &ArchiveManifest) -> Result<()> {
    let has_origin_delta = manifest_uses_origin_delta(manifest);
    ensure!(
        if has_origin_delta {
            manifest.schema_version >= ARCHIVE_SCHEMA_VERSION_ORIGIN_DELTA
        } else {
            manifest.schema_version != ARCHIVE_SCHEMA_VERSION_ORIGIN_DELTA
        },
        "origin delta repository metadata and archive schema disagree"
    );
    let expected_schema =
        if has_origin_delta {
            ARCHIVE_SCHEMA_VERSION_ORIGIN_DELTA
        } else if carries_linked_worktrees(&manifest.repositories) {
            ARCHIVE_SCHEMA_VERSION_LINKED_WORKTREES
        } else if carries_review_refs(&manifest.repositories) {
            ARCHIVE_SCHEMA_VERSION_REVIEW_REFS
        } else if carries_checkout_subdirectory(&manifest.repositories) {
            ARCHIVE_SCHEMA_VERSION_CHECKOUT_SUBDIRECTORY
        } else if carries_agent_reports(&manifest.payloads) {
            ARCHIVE_SCHEMA_VERSION_AGENT_REPORTS
        } else if manifest.repositories.iter().any(|repo| {
            !repo.metadata.saved_refs.is_empty() || !repo.metadata.stash_stack.is_empty()
        }) {
            ARCHIVE_SCHEMA_VERSION_CLONE_REFS
        } else {
            expected_schema_version(&manifest.payloads)
        };
    ensure!(
        manifest.schema_version == expected_schema
            || manifest.schema_version == ARCHIVE_SCHEMA_VERSION_PROMPT_CONTEXT
            || manifest.schema_version == ARCHIVE_SCHEMA_VERSION_NATIVE_USE
            || manifest.schema_version == ARCHIVE_SCHEMA_VERSION_MAILBOX
            || manifest.schema_version == ARCHIVE_SCHEMA_VERSION_NOTIFICATION_STACK
            || manifest.schema_version == ARCHIVE_SCHEMA_VERSION_LINKED_WORKTREES
            || (manifest.schema_version == ARCHIVE_SCHEMA_VERSION_REVIEW_REFS
                && expected_schema != ARCHIVE_SCHEMA_VERSION_LINKED_WORKTREES)
            || (manifest.schema_version == ARCHIVE_SCHEMA_VERSION_MESSAGES
                && expected_schema != ARCHIVE_SCHEMA_VERSION_REVIEW_REFS
                && expected_schema != ARCHIVE_SCHEMA_VERSION_LINKED_WORKTREES)
            || (expected_schema != ARCHIVE_SCHEMA_VERSION_LINKED_WORKTREES
                && expected_schema != ARCHIVE_SCHEMA_VERSION_REVIEW_REFS
                && expected_schema != ARCHIVE_SCHEMA_VERSION_CLONE_REFS
                && expected_schema != ARCHIVE_SCHEMA_VERSION_AGENT_REPORTS
                && expected_schema != ARCHIVE_SCHEMA_VERSION_CHECKOUT_SUBDIRECTORY
                && manifest.schema_version == ARCHIVE_SCHEMA_VERSION_CONTEXT),
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
    for repository in &manifest.repositories {
        if let Some(reference) = &repository.metadata.review_baseline_ref {
            ensure!(
                mj_core::archive::is_review_baseline_pin(reference),
                "invalid source review baseline ref: {reference}"
            );
        }
    }
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
        normalized_mode(descriptor.mode)?;
        ensure!(
            is_lower_hex_sha256(&descriptor.sha256),
            "invalid SHA-256 for payload '{}'",
            descriptor.path
        );
        validate_payload_parts(descriptor, &mut paths)?;
        if let PayloadRole::NativeArtifact { relative_path } = &descriptor.role {
            validate_archive_relative_path(relative_path)?;
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
        if let Some(subdirectory) = &metadata.checkout_subdirectory {
            validate_archive_relative_path(subdirectory)?;
        }
        ensure!(
            !origin_contains_credentials(&metadata.origin),
            "repository '{}' origin contains credentials",
            metadata.id
        );
        for push_url in &metadata.push_urls {
            ensure!(
                !origin_contains_credentials(push_url),
                "repository '{}' push URL contains credentials",
                metadata.id
            );
        }
        if metadata.remote_workspace {
            ensure!(
                !metadata.base_commit.is_empty(),
                "repository '{}' managed workspace has no base commit",
                metadata.id
            );
        }
        if let Some(snapshot) = &metadata.origin_delta {
            let origin_matches = if snapshot.history_cutoff {
                snapshot.origin_url.is_none() && metadata.origin.is_empty()
            } else {
                match snapshot.origin_url.as_deref() {
                    Some(url) => !url.is_empty() && metadata.origin == url,
                    None => {
                        metadata.origin.is_empty() && repository.origin_delta_pack_path.is_none()
                    }
                }
            };
            ensure!(
                origin_matches
                    && snapshot.head.commit() == metadata.head_commit
                    && !metadata.remote_workspace
                    && !metadata.plain_directory
                    && metadata.git_operation.is_none()
                    && repository.committed_bundle_path.is_empty()
                    && repository.staged_patch_path.is_empty()
                    && repository.unstaged_patch_path.is_empty()
                    && repository.untracked_tar_path.is_empty()
                    && repository.conflicted_worktree_tar_path.is_none()
                    && repository.operation_state_tar_path.is_none()
                    && repository.linked_worktrees.is_empty(),
                "repository '{}' carries incompatible origin delta metadata",
                metadata.id
            );
            if let Some(path) = &repository.origin_delta_pack_path {
                ensure!(
                    manifest.payloads.iter().any(|payload| {
                        payload.path == *path
                            && payload.size > 0
                            && payload.role
                                == (PayloadRole::GitOriginDeltaPack {
                                    repository_id: metadata.id.clone(),
                                })
                    }),
                    "repository '{}' is missing its origin delta pack",
                    metadata.id
                );
            }
            continue;
        }
        ensure!(
            repository.origin_delta_pack_path.is_none(),
            "legacy repository has an origin delta pack"
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
        let operation_payloads = match (
            &metadata.git_operation,
            &repository.conflicted_worktree_tar_path,
            &repository.operation_state_tar_path,
        ) {
            (None, None, None) => Vec::new(),
            (Some(operation), Some(worktree_path), Some(state_path)) => {
                validate_saved_git_operation(operation)
                    .with_context(|| format!("repository '{}'", metadata.id))?;
                vec![
                    (
                        worktree_path.as_str(),
                        PayloadRole::GitConflictedWorktreeTar {
                            repository_id: metadata.id.clone(),
                        },
                    ),
                    (
                        state_path.as_str(),
                        PayloadRole::GitOperationStateTar {
                            repository_id: metadata.id.clone(),
                        },
                    ),
                ]
            }
            _ => bail!(
                "repository '{}' Git operation metadata and payloads disagree",
                metadata.id
            ),
        };
        let mut worktree_payloads = Vec::new();
        let mut worktree_paths = BTreeSet::new();
        for (index, worktree) in repository.linked_worktrees.iter().enumerate() {
            validate_linked_worktree_metadata(&worktree.metadata)
                .with_context(|| format!("repository '{}'", metadata.id))?;
            ensure!(
                worktree_paths.insert(worktree.metadata.path.as_path()),
                "repository '{}' lists linked worktree '{}' twice",
                metadata.id,
                worktree.metadata.path.display()
            );
            let id = metadata.id.clone();
            let worktree_index = index as u32;
            worktree_payloads.push((
                worktree.staged_patch_path.as_str(),
                PayloadRole::GitLinkedWorktreeStagedPatch {
                    repository_id: id.clone(),
                    worktree: worktree_index,
                },
            ));
            worktree_payloads.push((
                worktree.unstaged_patch_path.as_str(),
                PayloadRole::GitLinkedWorktreeUnstagedPatch {
                    repository_id: id.clone(),
                    worktree: worktree_index,
                },
            ));
            worktree_payloads.push((
                worktree.untracked_tar_path.as_str(),
                PayloadRole::GitLinkedWorktreeUntrackedTar {
                    repository_id: id.clone(),
                    worktree: worktree_index,
                },
            ));
            match (
                &worktree.conflicted_worktree_tar_path,
                &worktree.operation_state_tar_path,
                &worktree.metadata.git_operation,
            ) {
                (None, None, None) => {}
                (Some(worktree_path), Some(state_path), Some(_)) => {
                    worktree_payloads.push((
                        worktree_path.as_str(),
                        PayloadRole::GitLinkedWorktreeConflictedWorktreeTar {
                            repository_id: id.clone(),
                            worktree: worktree_index,
                        },
                    ));
                    worktree_payloads.push((
                        state_path.as_str(),
                        PayloadRole::GitLinkedWorktreeOperationStateTar {
                            repository_id: id,
                            worktree: worktree_index,
                        },
                    ));
                }
                _ => bail!(
                    "repository '{}' linked worktree '{}' Git operation metadata and payloads disagree",
                    metadata.id,
                    worktree.metadata.path.display()
                ),
            }
        }
        for (path, role) in expected
            .into_iter()
            .chain(operation_payloads)
            .chain(worktree_payloads)
        {
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
            PayloadRole::GitOriginDeltaPack { repository_id } => {
                ensure!(
                    manifest.repositories.iter().any(|repository| {
                        repository.metadata.id == *repository_id
                            && repository.origin_delta_pack_path.as_ref() == Some(&payload.path)
                    }),
                    "unreferenced origin delta pack '{}'",
                    payload.path
                );
                Some(repository_id)
            }
            PayloadRole::GitBundle { repository_id }
            | PayloadRole::GitStagedPatch { repository_id }
            | PayloadRole::GitUnstagedPatch { repository_id }
            | PayloadRole::GitUntrackedTar { repository_id } => Some(repository_id),
            PayloadRole::GitConflictedWorktreeTar { repository_id }
            | PayloadRole::GitOperationStateTar { repository_id } => {
                ensure!(
                    manifest.repositories.iter().any(|repository| {
                        &repository.metadata.id == repository_id
                            && repository.metadata.git_operation.is_some()
                    }),
                    "payload '{}' holds a Git operation repository '{}' does not record",
                    payload.path,
                    repository_id
                );
                Some(repository_id)
            }
            PayloadRole::GitLinkedWorktreeStagedPatch {
                repository_id,
                worktree,
            }
            | PayloadRole::GitLinkedWorktreeUnstagedPatch {
                repository_id,
                worktree,
            }
            | PayloadRole::GitLinkedWorktreeUntrackedTar {
                repository_id,
                worktree,
            } => {
                ensure_linked_worktree_exists(manifest, payload, repository_id, *worktree)?;
                Some(repository_id)
            }
            PayloadRole::GitLinkedWorktreeConflictedWorktreeTar {
                repository_id,
                worktree,
            }
            | PayloadRole::GitLinkedWorktreeOperationStateTar {
                repository_id,
                worktree,
            } => {
                ensure_linked_worktree_exists(manifest, payload, repository_id, *worktree)?;
                Some(repository_id)
            }
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

fn ensure_linked_worktree_exists(
    manifest: &ArchiveManifest,
    payload: &PayloadDescriptor,
    repository_id: &str,
    worktree: u32,
) -> Result<()> {
    ensure!(
        manifest.repositories.iter().any(|repository| {
            repository.metadata.id == repository_id
                && (worktree as usize) < repository.linked_worktrees.len()
        }),
        "payload '{}' holds linked worktree {worktree} that repository '{repository_id}' does not record",
        payload.path
    );
    Ok(())
}

/// A linked worktree's directory is below the checkout and names nothing
/// outside it, and its commit and lock reason are ones Git can have written.
fn validate_linked_worktree_metadata(
    worktree: &mj_core::archive::LinkedWorktreeMetadata,
) -> Result<()> {
    validate_worktree_relative_path(&worktree.path)
        .with_context(|| format!("linked worktree '{}'", worktree.path.display()))?;
    ensure!(
        mj_core::remote_git::is_object_id(&worktree.head_commit),
        "linked worktree '{}' has an invalid head commit",
        worktree.path.display()
    );
    if let Some(operation) = &worktree.git_operation {
        validate_saved_git_operation(operation)
            .with_context(|| format!("linked worktree '{}'", worktree.path.display()))?;
    }
    Ok(())
}

/// Checks the index side of an archived Git operation: every path is a safe
/// relative path, and every stage, mode and object id is one Git can have
/// written.
fn validate_saved_git_operation(operation: &SavedGitOperation) -> Result<()> {
    let mut unmerged_paths = BTreeSet::new();
    for entry in &operation.unmerged {
        validate_worktree_relative_path(&entry.path)?;
        ensure!(
            (1..=3).contains(&entry.stage),
            "unmerged entry '{}' has stage {}",
            entry.path.display(),
            entry.stage
        );
        ensure!(
            matches!(
                entry.mode.as_str(),
                "100644" | "100755" | "120000" | "160000"
            ),
            "unmerged entry '{}' has mode {}",
            entry.path.display(),
            entry.mode
        );
        ensure!(
            mj_core::remote_git::is_object_id(&entry.object),
            "unmerged entry '{}' has an invalid object id",
            entry.path.display()
        );
        unmerged_paths.insert(entry.path.as_path());
    }
    for path in &operation.missing_worktree_paths {
        ensure!(
            unmerged_paths.contains(path.as_path()),
            "missing working-tree path '{}' is not unmerged",
            path.display()
        );
    }
    for object in &operation.objects {
        ensure!(
            mj_core::remote_git::is_object_id(object),
            "Git operation names an invalid object id {object:?}"
        );
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

fn normalized_mode(mode: u32) -> Result<u32> {
    ensure!(mode & !0o7777 == 0, "invalid payload mode {mode:o}");
    // Writable archive artifacts never need setuid/setgid/sticky bits.
    Ok(mode & 0o0777)
}

pub(super) fn digest_bytes(bytes: &[u8]) -> String {
    lower_hex(Sha256::digest(bytes))
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
    Ok(lower_hex(digest.finalize()))
}

#[cfg(unix)]
pub(super) fn restrict_archive_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("set 0600 permissions on {}", path.display()))
}

#[cfg(not(unix))]
pub(super) fn restrict_archive_permissions(_path: &Path) -> Result<()> {
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

/// Checks a path a restore writes or removes in a working tree. Besides the
/// archive path rules, it refuses a `.git` component in any case, as Git does
/// for every path it tracks: a hook written into the Git directory would run
/// on the restored clone's next Git command.
pub(crate) fn validate_worktree_relative_path(path: &Path) -> Result<()> {
    validate_archive_relative_path(path)?;
    ensure!(
        !path
            .components()
            .any(|component| component.as_os_str().eq_ignore_ascii_case(".git")),
        "refusing working-tree path '{}' inside a Git directory",
        path.display()
    );
    Ok(())
}

pub(crate) use mj_core::archive::archive_path_text as slash_path;

pub fn validate_component(value: &str, label: &str) -> Result<()> {
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

/// Remove embedded HTTP credentials before storing Git provenance.
pub fn redact_origin_credentials(origin: &str) -> Result<String> {
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

/// The largest file the session file export will return. A caller that wants a
/// build artifact wants the bundle, not a response the daemon has to hold in
/// memory in full.
pub const MAX_SESSION_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// Read one file from a session's workspace.
///
/// The path is relative and must stay inside `root` after both ends are
/// canonicalized, so neither a `..` component nor a symlink pointing out of the
/// workspace can reach the rest of the target's filesystem.
pub fn read_session_file(root: &Path, relative: &Path) -> Result<Vec<u8>, SessionExportError> {
    let refuse = |reason: String| SessionExportError::Refused(reason);
    mj_core::config::validate_relative_destination(relative)
        .map_err(|error| refuse(format!("{error:#}")))?;
    // The directory that was searched, as the caller spelled it, so a refusal
    // names somewhere the person can look (#1079).
    let searched = root.display().to_string();
    let root = root
        .canonicalize()
        .with_context(|| format!("resolve session workspace {}", root.display()))?;
    let path = root.join(relative).canonicalize().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            refuse(format!(
                "{} is not in the session workspace {searched}",
                relative.display()
            ))
        } else {
            SessionExportError::Failed(anyhow::Error::new(error).context(format!(
                "resolve {} in the session workspace",
                relative.display()
            )))
        }
    })?;
    if !path.starts_with(&root) {
        return Err(refuse(format!(
            "{} leaves the session workspace {searched}",
            relative.display()
        )));
    }
    let metadata = std::fs::metadata(&path)
        .with_context(|| format!("read {} in the session workspace", relative.display()))?;
    if !metadata.is_file() {
        return Err(refuse(format!(
            "{} is not a regular file",
            relative.display()
        )));
    }
    if metadata.len() > MAX_SESSION_FILE_BYTES {
        return Err(refuse(format!(
            "{} is larger than {MAX_SESSION_FILE_BYTES} bytes",
            relative.display()
        )));
    }
    Ok(fs::read(&path)
        .with_context(|| format!("read {} in the session workspace", relative.display()))?)
}

mod fetch;
pub use fetch::{
    FetchMode, FetchedKind, FetchedPath, ListedEntry, ListedKind, SavedFetch, SessionListing,
    create_fetch_spool, fetch_session_path,
};
mod file_input;
pub use file_input::{write_session_file, write_session_file_from_reader};

pub(crate) mod git;

pub(crate) use git::linked_worktree_directories;
#[cfg(test)]
pub(crate) use git::restore_linked_worktrees as restore_linked_worktrees_for_test;
pub use git::{
    DiffStatFile, EXPORT_REFUSED_EXIT_CODE, GitCollectionSpec, GitCommand, GitCommandRunner,
    GitHistoryMode, GitOutput, GitSnapshotProgress, NON_INTERACTIVE_GIT_ENV, PushBranchError,
    PushedBranch, ReflogAnchor, ReflogEntry, ReviewPoint, SessionDiff, SessionExportError,
    SystemGit, UntrackedFiles, collect_git_metadata_snapshot, collect_git_snapshot,
    collect_git_snapshot_with_progress, collect_plain_directory_snapshot, diff_between_trees,
    empty_tree_id, has_origin_refs, hidden_tracked_paths, managed_base_is_available,
    migrate_review_ref, numstat_between_trees, pin_review_tree, push_branch, push_review_base_ref,
    read_head_reflog, read_review_point, record_overridden_tree, record_review_point,
    record_reviewed_tree, remote_workspace_base, restore_git_snapshot, session_bundle_bytes,
    session_diff, session_diff_details, session_diff_patch, session_diff_patch_bytes,
    session_start_scope, tree_without_paths, validate_symlink_target,
};
pub(crate) use git::{
    LegacyRestoreOptions, ensure_no_symlink_ancestors, restore_git_snapshot_for_session,
    restore_git_snapshot_for_session_with_options,
};
#[cfg(test)]
use git::{build_untracked_tar, restore_untracked_tar};
use git::{
    validate_conflicted_worktree_tar_reader, validate_operation_state_tar_reader,
    validate_untracked_tar, validate_untracked_tar_reader,
};

mod purge;
pub use purge::{
    ArchiveUntrackedPurgeKind, ArchiveUntrackedPurgeReport, PreparedUntrackedPurge,
    StagedUntrackedPurge, StashUntrackedNote, archive_purge_not_cancelled, prepare_untracked_purge,
    purge_untracked_from_archive, purge_was_cancelled,
};

#[cfg(test)]
mod tests;
