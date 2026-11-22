//! Rewrite a legacy checkpoint archive so its untracked-file payloads are empty.
//!
//! The old reader still requires every `untracked_tar_path` entry and checks
//! its digest, so a purge replaces those bodies with a valid empty tar and
//! updates the manifest. Every other entry, including saved operation state,
//! is copied through unchanged, and the schema version stays the one the
//! archive already declared.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Cursor, Read, Seek, SeekFrom, Write};

use anyhow::{Context, Result, bail, ensure};
use flate2::bufread::ZlibDecoder;
use mj_core::hex::lower_hex;
use sha2::{Digest, Sha256};
use zip::CompressionMethod;

use super::{ARCHIVE_SCHEMA_VERSION_SHARDED, ArchiveManifest, RepositoryManifest};
use super::{
    MANIFEST_PATH, PayloadDescriptor, PayloadPartDescriptor, PayloadRole, digest_bytes,
    payload_part_path, validate_untracked_tar, write_stream_entry,
};

/// Commits larger than this are not stash metadata. Skip them instead of
/// allocating the size a corrupt pack header claims.
const MAX_INSPECTED_COMMIT_BYTES: usize = 16 * 1024 * 1024;

/// A stash commit whose third parent still holds untracked files.
///
/// Purge reports these and leaves the bundle alone. Restoring the stash would
/// bring those files back; deleting them would rewrite history the old reader
/// still verifies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashUntrackedNote {
    pub repository_id: String,
    pub commit: String,
    pub message: String,
}

/// What [`purge_untracked_from_archive`] did with the destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveUntrackedPurgeKind {
    /// Every repository is an origin-delta snapshot and no legacy untracked
    /// body is large enough to replace.
    AlreadyNewFormat,
    /// The archive is old, and every untracked body is already empty.
    AlreadyClean,
    /// Untracked bytes would be reclaimed. The destination was not written.
    DryRun,
    /// The destination is a verified archive with empty untracked payloads.
    Rewritten { archive_sha256: String },
}

/// The result of inspecting, and maybe rewriting, one archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveUntrackedPurgeReport {
    pub kind: ArchiveUntrackedPurgeKind,
    /// On-disk bytes a live purge removed: the old archive's file size minus
    /// the rewritten archive's file size. A dry run estimates the same saving
    /// from the ZIP central directory: the compressed size of each removed
    /// untracked entry, including shards, minus the compressed size of the
    /// empty tar that would replace it. Neither number is the uncompressed
    /// tar size.
    pub bytes_reclaimed: u64,
    pub stash_untracked_notes: Vec<StashUntrackedNote>,
    /// Non-fatal warnings (for example when stash bundle inspection could not
    /// determine whether stash entries still hold untracked files).
    pub warnings: Vec<String>,
    pub schema_version: u32,
}

/// A purge that stopped before installing a destination.
#[derive(Debug)]
pub enum PreparedUntrackedPurge {
    /// Nothing was written. The report is the whole result.
    Finished(ArchiveUntrackedPurgeReport),
    /// A verified temporary archive. Dropping this removes that file.
    Staged(StagedUntrackedPurge),
    /// `cancelled` asked the purge to stop. Temporary files are already gone.
    Cancelled,
}

/// A rewritten archive that has been fsynced and verified, but not installed.
///
/// The temporary file lives in the destination's directory. [`Self::install`]
/// is the atomic rename onto `destination`. Dropping this value deletes the
/// temporary file and does not touch `destination`.
#[derive(Debug)]
pub struct StagedUntrackedPurge {
    temporary: tempfile::NamedTempFile,
    destination: std::path::PathBuf,
    pub archive_sha256: String,
    pub bytes_reclaimed: u64,
    pub stash_untracked_notes: Vec<StashUntrackedNote>,
    pub warnings: Vec<String>,
    schema_version: u32,
}

impl StagedUntrackedPurge {
    /// Path the staged bytes will occupy after [`Self::install`].
    pub fn destination(&self) -> &std::path::Path {
        &self.destination
    }

    /// Install the staged bytes at the destination with `persist_noclobber`.
    ///
    /// A failure after the rename removes that installed file. A file that
    /// already occupied the destination is left in place, because
    /// `persist_noclobber` refuses to replace it.
    pub fn install(self) -> Result<ArchiveUntrackedPurgeReport> {
        let StagedUntrackedPurge {
            temporary,
            destination,
            archive_sha256,
            bytes_reclaimed,
            stash_untracked_notes,
            warnings,
            schema_version,
        } = self;
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        temporary
            .persist_noclobber(&destination)
            .map_err(|error| error.error)
            .with_context(|| format!("install purged archive {}", destination.display()))?;
        if let Err(error) = (|| -> Result<()> {
            super::restrict_archive_permissions(&destination)?;
            mj_core::config::sync_directory(parent)?;
            Ok(())
        })() {
            return Err(remove_installed(destination, error));
        }
        Ok(ArchiveUntrackedPurgeReport {
            kind: ArchiveUntrackedPurgeKind::Rewritten { archive_sha256 },
            bytes_reclaimed,
            stash_untracked_notes,
            warnings,
            schema_version,
        })
    }
}

fn remove_installed(destination: std::path::PathBuf, error: anyhow::Error) -> anyhow::Error {
    match std::fs::remove_file(&destination) {
        Ok(()) => error,
        Err(remove) => error.context(format!(
            "also failed to remove incomplete archive {}: {remove}",
            destination.display()
        )),
    }
}

/// A probe that never asks a purge to stop.
pub fn archive_purge_not_cancelled() -> &'static super::ArchivePurgeCancel {
    static NEVER: std::sync::OnceLock<super::ArchivePurgeCancel> = std::sync::OnceLock::new();
    NEVER.get_or_init(|| std::sync::Arc::new(|| false))
}

/// Whether a purge stopped because its cancellation probe fired.
pub fn purge_was_cancelled(error: &anyhow::Error) -> bool {
    super::archive_verification_cancelled(error)
        || error
            .chain()
            .any(|cause| cause.downcast_ref::<PurgeCancelled>().is_some())
}

#[derive(Debug)]
struct PurgeCancelled;

impl std::fmt::Display for PurgeCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("archive purge cancelled")
    }
}

impl std::error::Error for PurgeCancelled {}

fn stop_if_cancelled(cancelled: &super::ArchivePurgeCancel) -> Result<()> {
    if cancelled() {
        Err(PurgeCancelled.into())
    } else {
        Ok(())
    }
}

/// Replace every non-empty legacy untracked tar with an empty one.
///
/// `destination_archive` is written only when `dry_run` is false and some
/// untracked body is larger than an empty tar. The write installs with
/// `persist_noclobber` after the rewritten bytes have been verified. Only a
/// destination this call actually installed is removed if a later step fails.
/// A file that was already at `destination_archive` is left in place.
///
/// This entry never cancels. Call [`prepare_untracked_purge`] to stop when a
/// probe returns true, and install the staged file separately.
pub fn purge_untracked_from_archive(
    source_archive: &std::path::Path,
    destination_archive: &std::path::Path,
    dry_run: bool,
) -> Result<ArchiveUntrackedPurgeReport> {
    match prepare_untracked_purge(
        source_archive,
        destination_archive,
        dry_run,
        archive_purge_not_cancelled(),
    )? {
        PreparedUntrackedPurge::Finished(report) => Ok(report),
        PreparedUntrackedPurge::Cancelled => bail!("archive purge cancelled"),
        PreparedUntrackedPurge::Staged(staged) => staged.install(),
    }
}

/// Verify `source_archive` and, when a live purge has something to remove,
/// rewrite it into a temporary file in the destination directory.
///
/// The destination is not installed. [`StagedUntrackedPurge::install`] does
/// that. When `cancelled` returns true, this drops any temporary file and
/// returns [`PreparedUntrackedPurge::Cancelled`] instead of an archive error.
pub fn prepare_untracked_purge(
    source_archive: &std::path::Path,
    destination_archive: &std::path::Path,
    dry_run: bool,
    cancelled: &super::ArchivePurgeCancel,
) -> Result<PreparedUntrackedPurge> {
    if cancelled() {
        return Ok(PreparedUntrackedPurge::Cancelled);
    }
    let verified = match super::verify_archive_streaming_unless(source_archive, cancelled) {
        Ok(verified) => verified,
        Err(error) if purge_was_cancelled(&error) || cancelled() => {
            return Ok(PreparedUntrackedPurge::Cancelled);
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "verify archive {} before purging untracked files",
                    source_archive.display()
                )
            });
        }
    };
    let manifest = verified.manifest;
    let schema_version = manifest.schema_version;
    let empty = empty_untracked_tar()?;
    let empty_len = empty.len() as u64;
    // Eligibility stays on the uncompressed body. The reported number is the
    // on-disk delta, which compression can make much smaller.
    let uncompressed_reclaim = manifest
        .payloads
        .iter()
        .filter(|payload| is_untracked(&payload.role) && payload.size > empty_len)
        .map(|payload| payload.size - empty_len)
        .fold(0_u64, |total, bytes| total.saturating_add(bytes));
    let origin_delta = !manifest.repositories.is_empty()
        && manifest
            .repositories
            .iter()
            .all(|repository| repository.metadata.origin_delta.is_some());
    if origin_delta && uncompressed_reclaim == 0 {
        return Ok(PreparedUntrackedPurge::Finished(ArchiveUntrackedPurgeReport {
            kind: ArchiveUntrackedPurgeKind::AlreadyNewFormat,
            bytes_reclaimed: 0,
            stash_untracked_notes: Vec::new(),
            warnings: Vec::new(),
            schema_version,
        }));
    }
    let (stash_untracked_notes, warnings) =
        match stash_untracked_notes(source_archive, &manifest, cancelled) {
            Ok(notes) => notes,
            Err(error) if purge_was_cancelled(&error) || cancelled() => {
                return Ok(PreparedUntrackedPurge::Cancelled);
            }
            Err(error) => return Err(error),
        };
    if uncompressed_reclaim == 0 {
        return Ok(PreparedUntrackedPurge::Finished(ArchiveUntrackedPurgeReport {
            kind: ArchiveUntrackedPurgeKind::AlreadyClean,
            bytes_reclaimed: 0,
            stash_untracked_notes,
            warnings,
            schema_version,
        }));
    }
    if dry_run {
        let bytes_reclaimed = match compressed_reclaim_estimate(
            source_archive,
            &manifest,
            &empty,
            cancelled,
        ) {
            Ok(bytes) => bytes,
            Err(error) if purge_was_cancelled(&error) || cancelled() => {
                return Ok(PreparedUntrackedPurge::Cancelled);
            }
            Err(error) => return Err(error),
        };
        return Ok(PreparedUntrackedPurge::Finished(ArchiveUntrackedPurgeReport {
            kind: ArchiveUntrackedPurgeKind::DryRun,
            bytes_reclaimed,
            stash_untracked_notes,
            warnings,
            schema_version,
        }));
    }
    if cancelled() {
        return Ok(PreparedUntrackedPurge::Cancelled);
    }
    let temporary = match stage_rewritten_archive(
        source_archive,
        destination_archive,
        &manifest,
        &empty,
        cancelled,
    ) {
        Ok(temporary) => temporary,
        Err(error) if purge_was_cancelled(&error) || cancelled() => {
            return Ok(PreparedUntrackedPurge::Cancelled);
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "purge untracked files from {} into {}",
                    source_archive.display(),
                    destination_archive.display()
                )
            });
        }
    };
    let archive_sha256 = match super::verify_archive_streaming_unless(temporary.path(), cancelled)
    {
        Ok(verified) => verified.archive_sha256,
        Err(error) if purge_was_cancelled(&error) || cancelled() => {
            return Ok(PreparedUntrackedPurge::Cancelled);
        }
        Err(error) => {
            return Err(error.context(
                "purged archive failed verification and was removed",
            ));
        }
    };
    // One checkpoint archive is one file. Its untracked shards are ZIP
    // entries in that file, so the file-size delta is the disk space every
    // rewritten shard released.
    let old_bytes = file_len(source_archive)?;
    let new_bytes = file_len(temporary.path())?;
    Ok(PreparedUntrackedPurge::Staged(StagedUntrackedPurge {
        temporary,
        destination: destination_archive.to_path_buf(),
        archive_sha256,
        bytes_reclaimed: old_bytes.saturating_sub(new_bytes),
        stash_untracked_notes,
        warnings,
        schema_version,
    }))
}

fn file_len(path: &std::path::Path) -> Result<u64> {
    Ok(std::fs::metadata(path)
        .with_context(|| format!("read the size of {}", path.display()))?
        .len())
}

fn is_untracked(role: &PayloadRole) -> bool {
    matches!(
        role,
        PayloadRole::GitUntrackedTar { .. } | PayloadRole::GitLinkedWorktreeUntrackedTar { .. }
    )
}

fn empty_untracked_tar() -> Result<Vec<u8>> {
    let mut body = Vec::new();
    tar::Builder::new(&mut body)
        .finish()
        .context("finish empty untracked tar")?;
    ensure!(
        body.len() == 1024 && body.iter().all(|byte| *byte == 0),
        "empty untracked tar must be 1024 zero bytes, got {} bytes",
        body.len()
    );
    validate_untracked_tar(&body).context("empty untracked tar failed validation")?;
    Ok(body)
}

struct PlannedReplacement {
    old_names: Vec<String>,
    mode: u32,
    entries: Vec<(String, Vec<u8>)>,
}

fn plan_replacements(manifest: &mut ArchiveManifest, empty: &[u8]) -> Vec<PlannedReplacement> {
    let empty_len = empty.len() as u64;
    let mut replaced = BTreeSet::new();
    let mut old_names = BTreeMap::<String, Vec<String>>::new();
    for payload in &manifest.payloads {
        if is_untracked(&payload.role) && payload.size > empty_len {
            replaced.insert(payload.path.clone());
            old_names.insert(payload.path.clone(), zip_names(payload));
        }
    }
    for payload in &mut manifest.payloads {
        if replaced.contains(&payload.path) {
            payload.sha256 = digest_bytes(empty);
            payload.size = empty_len;
            payload.parts.clear();
        }
    }
    // Schema 3 is valid only while some payload is sharded. Clearing the parts
    // of the one payload that forced that schema would make the same version
    // fail the reader, so the empty tar is split into two parts instead.
    let keep_sharded = manifest.schema_version == ARCHIVE_SCHEMA_VERSION_SHARDED
        && manifest
            .payloads
            .iter()
            .all(|payload| payload.parts.is_empty());
    if keep_sharded
        && let Some(payload) = manifest
            .payloads
            .iter_mut()
            .find(|payload| replaced.contains(&payload.path))
    {
        let mid = empty.len() / 2;
        let chunks = [&empty[..mid], &empty[mid..]];
        payload.parts = chunks
            .iter()
            .enumerate()
            .map(|(index, chunk)| PayloadPartDescriptor {
                path: payload_part_path(&payload.path, index),
                sha256: digest_bytes(chunk),
                size: chunk.len() as u64,
            })
            .collect();
    }
    manifest
        .payloads
        .iter()
        .filter(|payload| replaced.contains(&payload.path))
        .map(|payload| PlannedReplacement {
            old_names: old_names
                .remove(&payload.path)
                .expect("replaced untracked payload keeps its old entry names"),
            mode: payload.mode,
            entries: replacement_entries(payload, empty),
        })
        .collect()
}

fn zip_names(payload: &PayloadDescriptor) -> Vec<String> {
    if payload.parts.is_empty() {
        vec![payload.path.clone()]
    } else {
        payload.parts.iter().map(|part| part.path.clone()).collect()
    }
}

fn replacement_entries(payload: &PayloadDescriptor, empty: &[u8]) -> Vec<(String, Vec<u8>)> {
    if payload.parts.is_empty() {
        return vec![(payload.path.clone(), empty.to_vec())];
    }
    let mut offset = 0_usize;
    payload
        .parts
        .iter()
        .map(|part| {
            let start = offset;
            offset += part.size as usize;
            (part.path.clone(), empty[start..offset].to_vec())
        })
        .collect()
}

fn stage_rewritten_archive(
    source_archive: &std::path::Path,
    destination_archive: &std::path::Path,
    manifest: &ArchiveManifest,
    empty: &[u8],
    cancelled: &super::ArchivePurgeCancel,
) -> Result<tempfile::NamedTempFile> {
    stop_if_cancelled(cancelled)?;
    ensure!(
        !destination_archive.exists(),
        "refusing to replace existing archive {}",
        destination_archive.display()
    );
    let parent = destination_archive
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    ensure!(
        parent.is_dir(),
        "archive directory {} does not exist",
        parent.display()
    );
    let mut manifest = manifest.clone();
    let planned = plan_replacements(&mut manifest, empty);
    ensure!(
        !planned.is_empty(),
        "purge planned no untracked replacement"
    );
    let manifest_bytes =
        serde_json::to_vec_pretty(&manifest).context("serialize purged archive manifest")?;
    // The temporary file is the only thing this function creates. Dropping it
    // removes those bytes. The destination is installed later, and only then.
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary archive in {}", parent.display()))?;
    super::restrict_archive_permissions(temporary.path())?;
    {
        let mut writer = zip::ZipWriter::new(temporary.as_file_mut());
        stream_rewritten_entries(
            &mut writer,
            source_archive,
            &manifest_bytes,
            &planned,
            cancelled,
        )?;
        writer.finish().context("finish purged archive ZIP")?;
    }
    temporary
        .as_file()
        .sync_all()
        .context("fsync purged archive")?;
    Ok(temporary)
}

fn stream_rewritten_entries(
    writer: &mut zip::ZipWriter<&mut File>,
    source_archive: &std::path::Path,
    manifest_bytes: &[u8],
    planned: &[PlannedReplacement],
    cancelled: &super::ArchivePurgeCancel,
) -> Result<()> {
    let mut skip = BTreeSet::new();
    let mut emit_at = BTreeMap::<String, usize>::new();
    for (index, replacement) in planned.iter().enumerate() {
        let Some(first) = replacement.old_names.first() else {
            bail!("untracked payload has no archive entry to replace");
        };
        ensure!(
            emit_at.insert(first.clone(), index).is_none(),
            "two untracked payloads share archive entry '{first}'"
        );
        skip.extend(replacement.old_names.iter().cloned());
    }
    let mut archive = super::open_archive_probed(source_archive, cancelled)
        .with_context(|| format!("read {}", source_archive.display()))?;
    let manifest_digest = digest_bytes(manifest_bytes);
    let mut saw_manifest = false;
    let mut emitted = BTreeSet::new();
    for index in 0..archive.len() {
        stop_if_cancelled(cancelled)?;
        let name = {
            let entry = archive
                .by_index(index)
                .with_context(|| format!("read ZIP entry {index}"))?;
            entry.name().to_owned()
        };
        if name == MANIFEST_PATH {
            saw_manifest = true;
            write_stream_entry(
                writer,
                MANIFEST_PATH,
                0o600,
                CompressionMethod::Deflated,
                manifest_bytes.len() as u64,
                &mut Cursor::new(manifest_bytes),
                &manifest_digest,
            )?;
            continue;
        }
        if let Some(slot) = emit_at.get(&name).copied() {
            write_replacement(writer, &planned[slot])?;
            emitted.insert(slot);
            continue;
        }
        if skip.contains(&name) {
            continue;
        }
        let entry = archive
            .by_index(index)
            .with_context(|| format!("copy ZIP entry {index}"))?;
        writer
            .raw_copy_file(entry)
            .with_context(|| format!("copy ZIP entry '{name}'"))?;
    }
    ensure!(saw_manifest, "archive is missing {MANIFEST_PATH}");
    ensure!(
        emitted.len() == planned.len(),
        "archive is missing an untracked payload that the manifest names"
    );
    Ok(())
}

fn write_replacement(
    writer: &mut zip::ZipWriter<&mut File>,
    replacement: &PlannedReplacement,
) -> Result<()> {
    for (name, bytes) in &replacement.entries {
        let digest = digest_bytes(bytes);
        write_stream_entry(
            writer,
            name,
            replacement.mode,
            CompressionMethod::Zstd,
            bytes.len() as u64,
            &mut Cursor::new(bytes.as_slice()),
            &digest,
        )
        .with_context(|| format!("write empty untracked payload '{name}'"))?;
    }
    Ok(())
}

/// Dry-run estimate of the on-disk saving.
///
/// Each removed untracked entry, including every shard, contributes its ZIP
/// central-directory `compressed_size`. The empty tar that would replace
/// those entries is compressed the same way a live rewrite compresses it,
/// and that overhead is subtracted. The archive is not rewritten.
fn compressed_reclaim_estimate(
    source_archive: &std::path::Path,
    manifest: &ArchiveManifest,
    empty: &[u8],
    cancelled: &super::ArchivePurgeCancel,
) -> Result<u64> {
    stop_if_cancelled(cancelled)?;
    let mut planned_manifest = manifest.clone();
    let planned = plan_replacements(&mut planned_manifest, empty);
    ensure!(
        !planned.is_empty(),
        "purge planned no untracked replacement"
    );
    let mut archive = super::open_archive_probed(source_archive, cancelled)
        .with_context(|| format!("read {}", source_archive.display()))?;
    let mut removed = 0_u64;
    for replacement in &planned {
        for name in &replacement.old_names {
            stop_if_cancelled(cancelled)?;
            let entry = archive.by_name(name).with_context(|| {
                format!("read compressed size of untracked entry '{name}'")
            })?;
            removed = removed.saturating_add(entry.compressed_size());
        }
    }
    let overhead = replacement_compressed_overhead(&planned)?;
    Ok(removed.saturating_sub(overhead))
}

/// Compressed size of the empty-tar entries a live rewrite would store.
fn replacement_compressed_overhead(planned: &[PlannedReplacement]) -> Result<u64> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for replacement in planned {
        for (name, bytes) in &replacement.entries {
            writer
                .start_file(
                    name.as_str(),
                    zip::write::SimpleFileOptions::default()
                        .compression_method(CompressionMethod::Zstd)
                        .compression_level(Some(super::ZSTD_LEVEL))
                        .unix_permissions(replacement.mode)
                        .large_file(bytes.len() as u64 > zip::ZIP64_BYTES_THR),
                )
                .with_context(|| format!("measure empty untracked payload '{name}'"))?;
            writer
                .write_all(bytes)
                .with_context(|| format!("measure empty untracked payload '{name}'"))?;
        }
    }
    let cursor = writer.finish().context("finish empty-tar size probe")?;
    let mut archive = zip::ZipArchive::new(cursor).context("read empty-tar size probe")?;
    let mut overhead = 0_u64;
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .context("read empty-tar size probe entry")?;
        overhead = overhead.saturating_add(entry.compressed_size());
    }
    Ok(overhead)
}

fn stash_untracked_notes(
    source_archive: &std::path::Path,
    manifest: &ArchiveManifest,
    cancelled: &super::ArchivePurgeCancel,
) -> Result<(Vec<StashUntrackedNote>, Vec<String>)> {
    let mut notes = Vec::new();
    let mut warnings = Vec::new();
    for repository in &manifest.repositories {
        stop_if_cancelled(cancelled)?;
        if repository.metadata.origin_delta.is_some()
            || repository.metadata.stash_stack.is_empty()
            || repository.committed_bundle_path.is_empty()
        {
            continue;
        }
        match notes_for_repository(source_archive, manifest, repository, cancelled) {
            Ok((repo_notes, repo_warnings)) => {
                notes.extend(repo_notes);
                warnings.extend(repo_warnings);
            }
            Err(error) if purge_was_cancelled(&error) => return Err(error),
            Err(error) => {
                tracing::warn!(
                    repository_id = %repository.metadata.id,
                    error = %error,
                    "could not inspect stash entries for untracked files; the purge continues"
                );
                warnings.push(format!(
                    "repository {}: could not inspect stash entries for untracked files ({error}); stash entries may still contain untracked files",
                    repository.metadata.id
                ));
            }
        }
    }
    Ok((notes, warnings))
}

fn notes_for_repository(
    source_archive: &std::path::Path,
    manifest: &ArchiveManifest,
    repository: &RepositoryManifest,
    cancelled: &super::ArchivePurgeCancel,
) -> Result<(Vec<StashUntrackedNote>, Vec<String>)> {
    stop_if_cancelled(cancelled)?;
    let Some(descriptor) = manifest
        .payloads
        .iter()
        .find(|payload| payload.path == repository.committed_bundle_path)
    else {
        tracing::warn!(
            repository_id = %repository.metadata.id,
            "stash inspection skipped a repository with no bundle payload"
        );
        return Ok((
            Vec::new(),
            vec![format!(
                "repository {}: could not inspect stash entries (bundle payload missing); stash entries may still contain untracked files",
                repository.metadata.id
            )],
        ));
    };
    if descriptor.size == 0 {
        return Ok((Vec::new(), Vec::new()));
    }
    // Shards are concatenated in manifest order onto a temporary file. The
    // pack walker streams that file and keeps only commits, so a repository
    // pack is never buffered in a `Vec<u8>`.
    let spool = match spool_bundle(source_archive, descriptor, cancelled) {
        Ok(spool) => spool,
        Err(error) if purge_was_cancelled(&error) => return Err(error),
        Err(error) => {
            tracing::warn!(
                repository_id = %repository.metadata.id,
                error = %error,
                "could not read the bundle while inspecting stash entries; the purge continues"
            );
            return Ok((
                Vec::new(),
                vec![format!(
                    "repository {}: could not read bundle to inspect stash entries ({error}); stash entries may still contain untracked files",
                    repository.metadata.id
                )],
            ));
        }
    };
    let mut reader = BufReader::new(spool);
    match bundle_prefix_is_git(&mut reader) {
        Ok(false) => return Ok((Vec::new(), Vec::new())),
        Ok(true) => {}
        Err(error) => {
            tracing::warn!(
                repository_id = %repository.metadata.id,
                error = %error,
                "could not read the bundle while inspecting stash entries; the purge continues"
            );
            return Ok((
                Vec::new(),
                vec![format!(
                    "repository {}: could not read bundle prefix to inspect stash entries ({error}); stash entries may still contain untracked files",
                    repository.metadata.id
                )],
            ));
        }
    }
    match inspect_bundle_stashes_reader(&mut reader, repository) {
        Ok(result) => Ok(result),
        Err(error) => {
            tracing::warn!(
                repository_id = %repository.metadata.id,
                error = %error,
                "could not inspect stash entries for untracked files; the purge continues"
            );
            Ok((
                Vec::new(),
                vec![format!(
                    "repository {}: could not inspect stash entries for untracked files ({error}); stash entries may still contain untracked files",
                    repository.metadata.id
                )],
            ))
        }
    }
}

fn bundle_prefix_is_git(reader: &mut impl BufRead) -> Result<bool> {
    let prefix = reader.fill_buf().context("read bundle prefix")?;
    const MAGIC_LEN: usize = 16;
    if prefix.len() < MAGIC_LEN {
        return Ok(false);
    }
    Ok(prefix.starts_with(b"# v2 git bundle\n") || prefix.starts_with(b"# v3 git bundle\n"))
}

/// Copy every shard of one bundle, in manifest order, to a temporary file.
///
/// Hashing happens while the bytes are copied. The returned file is rewound.
/// Disk use follows the bundle; the caller does not hold its bytes.
fn spool_bundle(
    source_archive: &std::path::Path,
    descriptor: &PayloadDescriptor,
    cancelled: &super::ArchivePurgeCancel,
) -> Result<tempfile::NamedTempFile> {
    let mut archive = super::open_archive_probed(source_archive, cancelled)?;
    let mut spool =
        tempfile::NamedTempFile::new().context("create a temporary git bundle spool")?;
    let mut whole = Sha256::new();
    if descriptor.parts.is_empty() {
        copy_bundle_entry(
            &mut archive,
            &descriptor.path,
            descriptor.size,
            descriptor.mode,
            &mut spool,
            &mut whole,
        )?;
    } else {
        let mut covered = 0_u64;
        for part in &descriptor.parts {
            let digest = copy_bundle_entry(
                &mut archive,
                &part.path,
                part.size,
                descriptor.mode,
                &mut spool,
                &mut whole,
            )?;
            ensure!(
                digest == part.sha256,
                "SHA-256 mismatch for payload part '{}'",
                part.path
            );
            covered = covered
                .checked_add(part.size)
                .context("payload part sizes overflow")?;
        }
        ensure!(
            covered == descriptor.size,
            "payload parts do not cover '{}'",
            descriptor.path
        );
    }
    let digest = lower_hex(whole.finalize());
    ensure!(
        digest == descriptor.sha256,
        "SHA-256 mismatch for payload '{}'",
        descriptor.path
    );
    spool
        .seek(SeekFrom::Start(0))
        .context("rewind git bundle spool")?;
    Ok(spool)
}

fn copy_bundle_entry(
    archive: &mut super::Archive,
    path: &str,
    expected_size: u64,
    expected_mode: u32,
    output: &mut impl Write,
    whole: &mut Sha256,
) -> Result<String> {
    let mut entry = archive
        .by_name(path)
        .with_context(|| format!("archive is missing payload '{path}'"))?;
    ensure!(!entry.is_dir(), "archive payload '{path}' is a directory");
    let enclosed = entry
        .enclosed_name()
        .ok_or_else(|| anyhow::anyhow!("unsafe ZIP entry path '{}'", entry.name()))?;
    ensure!(
        super::slash_path(&enclosed)? == path,
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
    let mut part = Sha256::new();
    let mut buffer = [0_u8; 8 * 1024];
    let mut copied = 0_u64;
    loop {
        let read = entry
            .read(&mut buffer)
            .with_context(|| format!("read ZIP entry '{path}'"))?;
        if read == 0 {
            break;
        }
        let chunk = &buffer[..read];
        output.write_all(chunk).context("write git bundle spool")?;
        part.update(chunk);
        whole.update(chunk);
        copied = copied.saturating_add(read as u64);
        ensure!(
            copied <= expected_size,
            "payload '{path}' exceeds its declared size"
        );
    }
    ensure!(
        copied == expected_size,
        "size mismatch for payload '{path}'"
    );
    Ok(lower_hex(part.finalize()))
}

#[cfg(test)]
fn inspect_bundle_stashes(
    bytes: &[u8],
    repository: &RepositoryManifest,
) -> Result<(Vec<StashUntrackedNote>, Vec<String>)> {
    inspect_bundle_stashes_reader(&mut Cursor::new(bytes), repository)
}

fn inspect_bundle_stashes_reader(
    reader: &mut impl BufRead,
    repository: &RepositoryManifest,
) -> Result<(Vec<StashUntrackedNote>, Vec<String>)> {
    // Clone bundles are thin: `git bundle unbundle` refuses to run until the
    // prerequisite commits exist, and purge does not have that checkout.
    // Stash parents are stored on the commit object, so read the pack directly.
    // A delta whose base is the missing prerequisite is skipped; that still
    // must not fail the purge, but is reported as a warning so the caller knows
    // that stash commit could not be verified.
    let parents =
        commit_parent_counts_reader(reader).context("read stash commits from the bundle pack")?;
    let mut notes = Vec::new();
    let mut warnings = Vec::new();
    for entry in &repository.metadata.stash_stack {
        let Some(parent_count) = parents.get(&entry.commit.to_ascii_lowercase()) else {
            tracing::warn!(
                repository_id = %repository.metadata.id,
                commit = %entry.commit,
                "stash commit is not in the archived bundle"
            );
            warnings.push(format!(
                "repository {}: could not inspect stash commit {} ({}) in the archived bundle; it may still contain untracked files",
                repository.metadata.id, entry.commit, entry.message
            ));
            continue;
        };
        // `git rev-list --parents` prints the commit and then each parent.
        // More than three words means a third parent, which is the untracked tree.
        if *parent_count >= 3 {
            notes.push(StashUntrackedNote {
                repository_id: repository.metadata.id.clone(),
                commit: entry.commit.clone(),
                message: entry.message.clone(),
            });
        }
    }
    Ok((notes, warnings))
}

const MAX_BUNDLE_HEADER_BYTES: usize = 16 * 1024 * 1024;

#[cfg(test)]
fn commit_parent_counts(bytes: &[u8]) -> Result<BTreeMap<String, usize>> {
    commit_parent_counts_reader(&mut Cursor::new(bytes))
}

fn commit_parent_counts_reader(reader: &mut impl BufRead) -> Result<BTreeMap<String, usize>> {
    let sha256 = read_bundle_header(reader)?;
    let hash_len = if sha256 { 32 } else { 20 };
    // Position counts bytes consumed from the pack, not the bundle header.
    // `ZlibDecoder` advances that position only through `BufRead::consume`.
    let mut pack = CountingReader::new(reader);
    let walked = read_pack_commits(&mut pack, hash_len)?;
    resolve_commit_parents(walked, sha256)
}

fn read_bundle_header(reader: &mut impl BufRead) -> Result<bool> {
    let mut sha256 = false;
    let mut total = 0_usize;
    let mut first = true;
    loop {
        let line = read_limited_line(reader, MAX_BUNDLE_HEADER_BYTES - total)?;
        ensure!(!line.is_empty(), "git bundle has no header terminator");
        total += line.len();
        if first {
            ensure!(
                line.starts_with(b"# v2 git bundle\n") || line.starts_with(b"# v3 git bundle\n"),
                "git bundle has an unsupported header"
            );
            first = false;
        }
        if line == b"\n" {
            return Ok(sha256);
        }
        let body = line.strip_suffix(b"\n").unwrap_or(line.as_slice());
        if body == b"@object-format=sha256" {
            sha256 = true;
        }
    }
}

fn read_limited_line(reader: &mut impl BufRead, remaining: usize) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    loop {
        let (chunk, done) = {
            let buf = reader.fill_buf().context("read git bundle header")?;
            if buf.is_empty() {
                (Vec::new(), true)
            } else if let Some(index) = buf.iter().position(|byte| *byte == b'\n') {
                (buf[..=index].to_vec(), true)
            } else {
                (buf.to_vec(), false)
            }
        };
        if chunk.is_empty() {
            break;
        }
        ensure!(
            line.len() + chunk.len() <= remaining,
            "git bundle header exceeds {MAX_BUNDLE_HEADER_BYTES} bytes"
        );
        reader.consume(chunk.len());
        line.extend_from_slice(&chunk);
        if done {
            break;
        }
    }
    Ok(line)
}

/// Counts bytes consumed from a pack. `consume` is the only place the
/// position moves, including when [`Read::read`] is used, so a zlib decoder
/// that prefetches through [`BufRead`] does not double-count.
struct CountingReader<R> {
    inner: R,
    pos: usize,
}

impl<R> CountingReader<R> {
    fn new(inner: R) -> Self {
        Self { inner, pos: 0 }
    }

    fn position(&self) -> usize {
        self.pos
    }
}

impl<R: BufRead> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let copied = {
            let available = self.fill_buf()?;
            if available.is_empty() {
                return Ok(0);
            }
            let copied = available.len().min(buf.len());
            buf[..copied].copy_from_slice(&available[..copied]);
            copied
        };
        self.consume(copied);
        Ok(copied)
    }
}

impl<R: BufRead> BufRead for CountingReader<R> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        self.inner.fill_buf()
    }

    fn consume(&mut self, amount: usize) {
        self.inner.consume(amount);
        self.pos = self.pos.saturating_add(amount);
    }
}

struct PackWalk {
    objects: Vec<PackedObject>,
    /// Offsets of trees, blobs, and tags. A delta based on one of these is not a commit.
    non_commits: BTreeSet<usize>,
}

struct PackedObject {
    offset: usize,
    kind: PackedKind,
}

enum PackedKind {
    Commit(Vec<u8>),
    Delta { base: DeltaBase, delta: Vec<u8> },
}

enum DeltaBase {
    Offset(usize),
    Oid(String),
}

fn read_pack_commits<R: BufRead>(
    pack: &mut CountingReader<R>,
    hash_len: usize,
) -> Result<PackWalk> {
    let mut header = [0_u8; 12];
    pack.read_exact(&mut header)
        .context("git pack is truncated")?;
    ensure!(header.starts_with(b"PACK"), "git pack has a bad signature");
    let mut version_bytes = [0_u8; 4];
    version_bytes.copy_from_slice(&header[4..8]);
    let version = u32::from_be_bytes(version_bytes);
    ensure!(
        version == 2 || version == 3,
        "unsupported git pack version {version}"
    );
    let mut count_bytes = [0_u8; 4];
    count_bytes.copy_from_slice(&header[8..12]);
    let count = u32::from_be_bytes(count_bytes) as usize;
    let mut objects = Vec::new();
    let mut non_commits = BTreeSet::new();
    for _ in 0..count {
        let header_at = pack.position();
        let (typ, size) = read_pack_type_and_size(pack)?;
        match typ {
            1 => {
                if size > MAX_INSPECTED_COMMIT_BYTES {
                    tracing::warn!(
                        size,
                        "skipping an oversized commit while inspecting stash entries"
                    );
                    discard_zlib(pack)?;
                    non_commits.insert(header_at);
                    continue;
                }
                let body = inflate_exact(pack, size)?;
                objects.push(PackedObject {
                    offset: header_at,
                    kind: PackedKind::Commit(body),
                });
            }
            2..=4 => {
                discard_zlib(pack)?;
                non_commits.insert(header_at);
            }
            6 | 7 => {
                let base = if typ == 6 {
                    let ofs = read_ofs_delta(pack)?;
                    ensure!(
                        ofs > 0 && ofs <= header_at,
                        "ofs delta points outside the pack"
                    );
                    DeltaBase::Offset(header_at - ofs)
                } else {
                    let mut oid = vec![0_u8; hash_len];
                    pack.read_exact(&mut oid)
                        .context("ref delta object id is truncated")?;
                    DeltaBase::Oid(lower_hex(&oid))
                };
                // An ofs-delta of a blob or tree is the same type. Drop it
                // now so a deltified pack does not keep those bodies.
                let known_non_commit = matches!(
                    &base,
                    DeltaBase::Offset(offset) if non_commits.contains(offset)
                );
                if known_non_commit || size > MAX_INSPECTED_COMMIT_BYTES {
                    discard_zlib(pack)?;
                    non_commits.insert(header_at);
                    continue;
                }
                let delta = inflate_exact(pack, size)?;
                objects.push(PackedObject {
                    offset: header_at,
                    kind: PackedKind::Delta { base, delta },
                });
            }
            _ => bail!("git pack contains object type {typ}"),
        }
    }
    let mut trailer = vec![0_u8; hash_len];
    pack.read_exact(&mut trailer)
        .context("git pack trailer is truncated")?;
    let mut extra = [0_u8; 1];
    let extra_len = pack.read(&mut extra).context("read git pack trailer")?;
    ensure!(
        extra_len == 0,
        "git pack trailer is longer than {hash_len} bytes"
    );
    Ok(PackWalk {
        objects,
        non_commits,
    })
}

fn resolve_commit_parents(walk: PackWalk, sha256: bool) -> Result<BTreeMap<String, usize>> {
    let PackWalk {
        objects,
        mut non_commits,
    } = walk;
    let mut by_offset: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
    let mut by_oid: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut pending = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        match &object.kind {
            PackedKind::Commit(body) => {
                let oid = git_object_id("commit", body, sha256);
                by_oid.insert(oid, body.clone());
                by_offset.insert(object.offset, body.clone());
            }
            PackedKind::Delta { .. } => pending.push(index),
        }
    }

    let mut passes = 0;
    while !pending.is_empty() && passes <= objects.len() {
        passes += 1;
        let pending_offsets: BTreeSet<usize> =
            pending.iter().map(|index| objects[*index].offset).collect();
        let mut next = Vec::new();
        let mut progress = false;
        for index in pending {
            let object = &objects[index];
            let PackedKind::Delta { base, delta } = &object.kind else {
                continue;
            };
            if let DeltaBase::Offset(offset) = base
                && non_commits.contains(offset)
            {
                non_commits.insert(object.offset);
                progress = true;
                continue;
            }
            let base_bytes = match base {
                DeltaBase::Offset(offset) => by_offset.get(offset).cloned(),
                DeltaBase::Oid(oid) => by_oid.get(oid).cloned(),
            };
            let Some(base_bytes) = base_bytes else {
                let waiting = match base {
                    DeltaBase::Offset(offset) => pending_offsets.contains(offset),
                    // The base commit may be a later delta whose id is not known yet.
                    DeltaBase::Oid(_) => true,
                };
                if waiting {
                    next.push(index);
                }
                continue;
            };
            match apply_delta(&base_bytes, delta) {
                Ok(body) => {
                    let oid = git_object_id("commit", &body, sha256);
                    by_oid.insert(oid, body.clone());
                    by_offset.insert(object.offset, body);
                    progress = true;
                }
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "could not apply a pack delta while inspecting stash commits"
                    );
                    non_commits.insert(object.offset);
                    progress = true;
                }
            }
        }
        if !progress {
            break;
        }
        pending = next;
    }

    Ok(by_oid
        .into_iter()
        .map(|(oid, body)| (oid, commit_parent_count(&body)))
        .collect())
}

fn commit_parent_count(body: &[u8]) -> usize {
    body.split(|byte| *byte == b'\n')
        .take_while(|line| !line.is_empty())
        .filter(|line| line.starts_with(b"parent "))
        .count()
}

pub(super) fn git_object_id(kind: &str, body: &[u8], sha256: bool) -> String {
    let len = body.len();
    let mut encoded = format!("{kind} {len}").into_bytes();
    encoded.push(0);
    encoded.extend_from_slice(body);
    if sha256 {
        lower_hex(Sha256::digest(&encoded))
    } else {
        lower_hex(ring::digest::digest(
            &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
            &encoded,
        ))
    }
}

fn read_u8(pack: &mut impl BufRead) -> Result<u8> {
    let mut buf = [0_u8; 1];
    pack.read_exact(&mut buf).context("truncated pack")?;
    Ok(buf[0])
}

fn read_pack_type_and_size(pack: &mut impl BufRead) -> Result<(u8, usize)> {
    let mut byte = read_u8(pack).context("truncated pack object header")?;
    let typ = (byte >> 4) & 7;
    let mut size = usize::from(byte & 0x0f);
    let mut shift = 4;
    while byte & 0x80 != 0 {
        ensure!(shift <= 60, "pack object size is too large");
        byte = read_u8(pack).context("truncated pack object size")?;
        size |= usize::from(byte & 0x7f) << shift;
        shift += 7;
    }
    Ok((typ, size))
}

fn read_ofs_delta(pack: &mut impl BufRead) -> Result<usize> {
    let mut byte = read_u8(pack).context("truncated ofs delta")?;
    let mut offset = usize::from(byte & 0x7f);
    while byte & 0x80 != 0 {
        byte = read_u8(pack).context("truncated ofs delta")?;
        offset = offset
            .checked_add(1)
            .and_then(|value| value.checked_shl(7))
            .and_then(|value| value.checked_add(usize::from(byte & 0x7f)))
            .context("ofs delta offset overflows")?;
    }
    Ok(offset)
}

fn inflate_exact(pack: &mut impl BufRead, size: usize) -> Result<Vec<u8>> {
    let mut decoder = ZlibDecoder::new(pack);
    let mut body = vec![0_u8; size];
    decoder
        .read_exact(&mut body)
        .context("inflate pack object")?;
    let mut extra = [0_u8; 1];
    let extra_len = decoder.read(&mut extra).context("finish pack object")?;
    ensure!(
        extra_len == 0,
        "pack object is longer than its header declares"
    );
    Ok(body)
}

fn discard_zlib(pack: &mut impl BufRead) -> Result<()> {
    let mut decoder = ZlibDecoder::new(pack);
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = decoder.read(&mut buffer).context("skip pack object")?;
        if read == 0 {
            break;
        }
        note_discarded_read(read);
    }
    Ok(())
}

#[cfg(test)]
std::thread_local! {
    static DISCARD_READS: std::cell::Cell<(usize, u64)> = const { std::cell::Cell::new((0, 0)) };
}

#[cfg(test)]
fn note_discarded_read(bytes: usize) {
    DISCARD_READS.with(|cell| {
        let (peak, total) = cell.get();
        cell.set((peak.max(bytes), total.saturating_add(bytes as u64)));
    });
}

#[cfg(not(test))]
fn note_discarded_read(_bytes: usize) {}

/// Peak read and total uncompressed bytes skipped while inspecting a bundle.
///
/// The peak is the largest single discard read. A blob buffered whole would
/// exceed the 8 KiB discard buffer.
#[cfg(test)]
pub(super) fn reset_bundle_discard_stats() {
    DISCARD_READS.with(|cell| cell.set((0, 0)));
}

#[cfg(test)]
pub(super) fn bundle_discard_stats() -> (usize, u64) {
    DISCARD_READS.with(|cell| cell.get())
}

fn apply_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>> {
    let (source_size, mut pos) = read_delta_size(delta, 0)?;
    let (target_size, pos_after_sizes) = read_delta_size(delta, pos)?;
    pos = pos_after_sizes;
    ensure!(
        source_size == base.len(),
        "delta base is {} bytes, not {source_size}",
        base.len()
    );
    ensure!(
        target_size <= MAX_INSPECTED_COMMIT_BYTES,
        "delta target is {target_size} bytes"
    );
    let mut out = Vec::with_capacity(target_size);
    while pos < delta.len() {
        let command = delta[pos];
        pos += 1;
        if command == 0 {
            bail!("delta contains an empty command");
        } else if command & 0x80 != 0 {
            let mut offset = 0_usize;
            let mut size = 0_usize;
            for bit in 0..4 {
                if command & (1 << bit) != 0 {
                    ensure!(pos < delta.len(), "truncated delta copy offset");
                    offset |= usize::from(delta[pos]) << (8 * bit);
                    pos += 1;
                }
            }
            for bit in 0..3 {
                if command & (1 << (4 + bit)) != 0 {
                    ensure!(pos < delta.len(), "truncated delta copy length");
                    size |= usize::from(delta[pos]) << (8 * bit);
                    pos += 1;
                }
            }
            if size == 0 {
                size = 0x10000;
            }
            let end = offset.checked_add(size).context("delta copy overflows")?;
            ensure!(end <= base.len(), "delta copy exceeds its base");
            out.extend_from_slice(&base[offset..end]);
        } else {
            let length = usize::from(command);
            ensure!(pos + length <= delta.len(), "truncated delta insert");
            out.extend_from_slice(&delta[pos..pos + length]);
            pos += length;
        }
        ensure!(
            out.len() <= target_size,
            "delta expanded past its target size"
        );
    }
    ensure!(
        out.len() == target_size,
        "delta expanded to {} bytes, not {target_size}",
        out.len()
    );
    Ok(out)
}

fn read_delta_size(delta: &[u8], mut pos: usize) -> Result<(usize, usize)> {
    let mut size = 0_usize;
    let mut shift = 0;
    loop {
        ensure!(pos < delta.len(), "truncated delta size");
        ensure!(shift <= 60, "delta size is too large");
        let byte = delta[pos];
        pos += 1;
        size |= usize::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    Ok((size, pos))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::path::PathBuf;

    use super::super::{RepositoryManifest, RepositoryMetadata, SavedStashEntry};
    use super::{commit_parent_counts, git_object_id, inspect_bundle_stashes};

    /// `git hash-object -t commit` of [`KNOWN_COMMIT`].
    const KNOWN_COMMIT: &[u8] = b"\
tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
parent 0000000000000000000000000000000000000001\n\
author T <t@t> 1 +0000\n\
committer T <t@t> 1 +0000\n\
\n\
note\n";
    const KNOWN_COMMIT_SHA: &str = "48ca35318d1399e84e52432c1b0d1b0f76444c35";
    const KNOWN_COMMIT_SHA256: &str =
        "ec74e4e8cec8bd9cf3d2592a1f86c17308a4aaf4becf116d5187757c3f39b242";

    #[test]
    fn purge_pack_reader_matches_git_object_ids_and_in_pack_deltas() {
        assert_eq!(
            git_object_id("commit", KNOWN_COMMIT, false),
            KNOWN_COMMIT_SHA
        );
        assert_eq!(
            git_object_id("commit", KNOWN_COMMIT, true),
            KNOWN_COMMIT_SHA256
        );

        let three_parents = commit_with_parents(3, "holds untracked");
        let two_parents = commit_with_parents(2, "tracked only");
        let chained = commit_with_parents(3, "chained untracked");
        let three_id = git_object_id("commit", &three_parents, false);
        let two_id = git_object_id("commit", &two_parents, false);
        let chained_id = git_object_id("commit", &chained, false);

        let mut pack = pack_header(6);
        let known_at = pack.len();
        push_raw(&mut pack, 1, KNOWN_COMMIT);
        let three_at = pack.len();
        push_ofs_delta(&mut pack, known_at, KNOWN_COMMIT, &three_parents);
        push_ref_delta(
            &mut pack,
            &decode_oid(KNOWN_COMMIT_SHA),
            KNOWN_COMMIT,
            &two_parents,
        );
        let blob_at = pack.len();
        push_raw(&mut pack, 3, b"not-a-commit");
        push_ofs_delta(&mut pack, blob_at, b"not-a-commit", b"not-a-commit!");
        push_ofs_delta(&mut pack, three_at, &three_parents, &chained);
        pack.extend_from_slice(&[0_u8; 20]);

        let parents = commit_parent_counts(&bundle(&pack)).unwrap();
        assert_eq!(parents.get(KNOWN_COMMIT_SHA).copied(), Some(1));
        assert_eq!(parents.get(&three_id).copied(), Some(3));
        assert_eq!(parents.get(&two_id).copied(), Some(2));
        assert_eq!(parents.get(&chained_id).copied(), Some(3));
        assert_eq!(parents.len(), 4);

        let (notes, warnings) = inspect_bundle_stashes(
            &bundle(&pack),
            &stash_manifest(&[
                (three_id.to_ascii_uppercase(), "holds untracked"),
                (two_id, "tracked only"),
                ("ab".repeat(20), "missing"),
            ]),
        )
        .unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].repository_id, "repo");
        assert_eq!(notes[0].commit, three_id.to_ascii_uppercase());
        assert_eq!(notes[0].message, "holds untracked");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains(&"ab".repeat(20)));
    }

    #[test]
    fn purge_pack_reader_skips_a_delta_whose_base_is_outside_the_pack() {
        let three_parents = commit_with_parents(3, "holds untracked");
        let mut pack = pack_header(1);
        push_ref_delta(&mut pack, &[0xab; 20], KNOWN_COMMIT, &three_parents);
        pack.extend_from_slice(&[0_u8; 20]);
        let parents = commit_parent_counts(&bundle(&pack)).unwrap();
        assert!(parents.is_empty());
    }

    #[test]
    fn purge_pack_reader_discards_a_large_blob_in_bounded_chunks() {
        let blob = vec![b'x'; 200 * 1024];
        let mut pack = pack_header(2);
        push_raw(&mut pack, 1, KNOWN_COMMIT);
        push_raw(&mut pack, 3, &blob);
        pack.extend_from_slice(&[0_u8; 20]);
        super::reset_bundle_discard_stats();
        let parents = commit_parent_counts(&bundle(&pack)).unwrap();
        assert_eq!(parents.get(KNOWN_COMMIT_SHA).copied(), Some(1));
        let (peak, total) = super::bundle_discard_stats();
        assert!(peak > 0 && peak <= 8 * 1024, "discard peak {peak}");
        assert!(
            total >= blob.len() as u64,
            "discarded {total} bytes of a {} byte blob",
            blob.len()
        );
    }

    #[test]
    fn purge_pack_reader_rejects_a_truncated_bundle() {
        let error = commit_parent_counts(b"# v2 git bundle\n\nPACK").unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("truncated") || message.contains("pack"),
            "{message}"
        );
    }

    fn stash_manifest(entries: &[(String, &str)]) -> RepositoryManifest {
        RepositoryManifest {
            metadata: RepositoryMetadata {
                id: "repo".into(),
                relative_destination: PathBuf::from("repo"),
                checkout_subdirectory: None,
                origin: "https://example.test/repo.git".into(),
                push_urls: Vec::new(),
                remote_workspace: false,
                plain_directory: false,
                base_commit: "a".repeat(40),
                head_commit: "b".repeat(40),
                branch: Some("main".into()),
                review_refs_metadata_only: false,
                review_baseline_ref: None,
                saved_refs: BTreeMap::new(),
                stash_stack: entries
                    .iter()
                    .map(|(commit, message)| SavedStashEntry {
                        commit: commit.clone(),
                        message: (*message).to_owned(),
                    })
                    .collect(),
                git_operation: None,
                origin_delta: None,
            },
            committed_bundle_path: "bundle".into(),
            staged_patch_path: String::new(),
            unstaged_patch_path: String::new(),
            untracked_tar_path: "untracked.tar".into(),
            conflicted_worktree_tar_path: None,
            operation_state_tar_path: None,
            linked_worktrees: Vec::new(),
            origin_delta_pack_path: None,
        }
    }

    fn commit_with_parents(parents: usize, message: &str) -> Vec<u8> {
        let mut body = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n".to_vec();
        for index in 0..parents {
            let parent = index + 1;
            body.extend(format!("parent {parent:040x}\n").into_bytes());
        }
        body.extend(
            format!("author T <t@t> 1 +0000\ncommitter T <t@t> 1 +0000\n\n{message}\n")
                .into_bytes(),
        );
        body
    }

    fn bundle(pack: &[u8]) -> Vec<u8> {
        let mut bytes = b"# v2 git bundle\n\n".to_vec();
        bytes.extend_from_slice(pack);
        bytes
    }

    fn pack_header(count: u32) -> Vec<u8> {
        let mut pack = b"PACK".to_vec();
        pack.extend_from_slice(&2_u32.to_be_bytes());
        pack.extend_from_slice(&count.to_be_bytes());
        pack
    }

    fn push_raw(pack: &mut Vec<u8>, typ: u8, body: &[u8]) {
        push_header(pack, typ, body.len());
        pack.extend(zlib(body));
    }

    fn push_ofs_delta(pack: &mut Vec<u8>, base_at: usize, base: &[u8], target: &[u8]) {
        let header_at = pack.len();
        let delta = encode_delta(base, target);
        push_header(pack, 6, delta.len());
        push_ofs(pack, header_at - base_at);
        pack.extend(zlib(&delta));
    }

    fn push_ref_delta(pack: &mut Vec<u8>, base_oid: &[u8; 20], base: &[u8], target: &[u8]) {
        let delta = encode_delta(base, target);
        push_header(pack, 7, delta.len());
        pack.extend_from_slice(base_oid);
        pack.extend(zlib(&delta));
    }

    fn push_header(pack: &mut Vec<u8>, typ: u8, mut size: usize) {
        let mut first = (typ << 4) | ((size & 0x0f) as u8);
        size >>= 4;
        if size > 0 {
            first |= 0x80;
        }
        pack.push(first);
        while size > 0 {
            let mut byte = (size & 0x7f) as u8;
            size >>= 7;
            if size > 0 {
                byte |= 0x80;
            }
            pack.push(byte);
        }
    }

    fn push_ofs(pack: &mut Vec<u8>, mut offset: usize) {
        let mut bytes = vec![(offset & 0x7f) as u8];
        offset >>= 7;
        while offset > 0 {
            offset -= 1;
            bytes.push(0x80 | ((offset & 0x7f) as u8));
            offset >>= 7;
        }
        bytes.reverse();
        pack.extend(bytes);
    }

    fn encode_delta(base: &[u8], target: &[u8]) -> Vec<u8> {
        let mut delta = Vec::new();
        push_size(&mut delta, base.len());
        push_size(&mut delta, target.len());
        let prefix = base
            .iter()
            .zip(target.iter())
            .take_while(|(left, right)| left == right)
            .count();
        if prefix > 0 {
            push_copy(&mut delta, 0, prefix);
        }
        let rest = &target[prefix..];
        let mut pos = 0;
        while pos < rest.len() {
            let length = (rest.len() - pos).min(127);
            delta.push(length as u8);
            delta.extend_from_slice(&rest[pos..pos + length]);
            pos += length;
        }
        delta
    }

    fn push_size(out: &mut Vec<u8>, mut size: usize) {
        loop {
            let mut byte = (size & 0x7f) as u8;
            size >>= 7;
            if size > 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if size == 0 {
                break;
            }
        }
    }

    fn push_copy(out: &mut Vec<u8>, offset: usize, size: usize) {
        let mut command = 0x80_u8;
        let mut bytes = Vec::new();
        for bit in 0..4 {
            let byte = (offset >> (8 * bit)) & 0xff;
            if byte != 0 {
                command |= 1 << bit;
                bytes.push(byte as u8);
            }
        }
        for bit in 0..3 {
            let byte = (size >> (8 * bit)) & 0xff;
            if byte != 0 {
                command |= 1 << (4 + bit);
                bytes.push(byte as u8);
            }
        }
        out.push(command);
        out.extend(bytes);
    }

    fn zlib(body: &[u8]) -> Vec<u8> {
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(body).unwrap();
        encoder.finish().unwrap()
    }

    fn decode_oid(hex: &str) -> [u8; 20] {
        let mut oid = [0_u8; 20];
        for (index, byte) in oid.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).unwrap();
        }
        oid
    }
}
