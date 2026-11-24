use super::*;
use crate::archive::git::{
    collect_file_backed_git_snapshot, collect_git_metadata_snapshot_with,
    collect_plain_directory_snapshot_with,
};
use crate::archive::{RepositoryPayload, StagedPayload, write_archive_hashed_repository_payloads};
use mj_core::hex::lower_hex;

use super::restore::prepare_private_destination;

pub(super) const CHECKPOINT_STAGE_MANIFEST: &str = "stage.json";

/// The name a capture gives the directory it fills before sealing it as the
/// stage, beside the stage. A capture that is killed leaves it behind.
pub const CHECKPOINT_CAPTURE_TEMPORARY_PREFIX: &str = ".checkpoint-capture-";

fn create_capture_directory(parent: &Path) -> Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    Ok(builder
        .prefix(CHECKPOINT_CAPTURE_TEMPORARY_PREFIX)
        .tempdir_in(parent)?)
}

/// Capture mutable target-owned state into a sealed, uncompressed generation.
/// The caller holds the relay barrier only for this operation; packaging the
/// generation is intentionally a separate command.
pub fn capture_checkpoint(
    spec: &CheckpointCaptureSpec,
    git: &dyn GitCommandRunner,
) -> Result<CapturedCheckpoint> {
    capture_checkpoint_with_native_state(spec, git, &NoNativeCheckpointState)
}

pub fn capture_checkpoint_with_native_state(
    spec: &CheckpointCaptureSpec,
    git: &dyn GitCommandRunner,
    native_state: &dyn NativeCheckpointState,
) -> Result<CapturedCheckpoint> {
    ensure!(
        spec.protocol_version == CHECKPOINT_STAGING_PROTOCOL_VERSION,
        "unsupported checkpoint staging protocol version {}; worker supports {}",
        spec.protocol_version,
        CHECKPOINT_STAGING_PROTOCOL_VERSION
    );
    let mut resolved = spec.clone();
    resolved.relay_root = resolve_target_path(&resolved.relay_root)?;
    resolved.harness_home = checkpoint_harness_home(
        &resolved.relay_root,
        &resolve_target_path(&resolved.harness_home)?,
    )?;
    resolved.workspace_root = resolve_target_path(&resolved.workspace_root)?;
    resolved.stage_path = resolve_target_path(&resolved.stage_path)?;
    validate_capture_spec(&resolved)?;

    let source_fingerprint_before = checkpoint_source_fingerprint(
        &resolved.harness_home,
        &resolved.workspace_root,
        resolved.report_directories.as_deref(),
    )?;
    if resolved.refresh_existing && resolved.stage_path.is_dir() {
        if let Some(captured) =
            refresh_checkpoint_stage(&resolved, &source_fingerprint_before, git)?
        {
            return Ok(captured);
        }
        fs::remove_dir_all(&resolved.stage_path).with_context(|| {
            format!(
                "remove stale checkpoint prestage {}",
                resolved.stage_path.display()
            )
        })?;
    }

    let (native_artifacts, omitted_reports) = collect_checkpoint_native_artifacts_with_reports(
        &resolved.session,
        &resolved.relay_root,
        &resolved.harness_home,
        &resolved.workspace_root,
        resolved.report_directories.as_deref(),
        resolved.allow_empty_native,
        native_state,
    )?;

    let parent = resolved
        .stage_path
        .parent()
        .context("checkpoint stage has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create checkpoint stage parent {}", parent.display()))?;
    ensure!(
        !resolved.stage_path.exists(),
        "checkpoint stage already exists at {}",
        resolved.stage_path.display()
    );
    let temporary = create_capture_directory(parent)
        .with_context(|| format!("create temporary checkpoint stage in {}", parent.display()))?;

    let mut staged_native = Vec::with_capacity(native_artifacts.len());
    let mut native_bytes = 0_u64;
    for (index, artifact) in native_artifacts.into_iter().enumerate() {
        let body_path = PathBuf::from(format!("native/{index:08}"));
        let size =
            write_private_reader(temporary.path(), &body_path, &mut artifact.reader()?, 0o600)?;
        native_bytes = native_bytes
            .checked_add(size)
            .context("native checkpoint size overflow")?;
        staged_native.push(StagedNativeArtifact {
            relative_path: artifact.relative_path,
            mode: artifact.mode,
            size,
            body_path,
        });
    }
    // Native bytes are copied before repository collection, which can take
    // long enough for the harness to append more history; the stage must hold
    // the native state selected above, as it did when bodies were read eagerly.
    let repositories = collect_checkpoint_repositories(
        &resolved.workspace_root,
        &resolved.repositories,
        git,
        temporary.path(),
    )?;
    let repository_bytes = checkpoint_repository_bytes(&repositories)?;
    let staged_repositories = write_staged_repositories(temporary.path(), repositories)?;
    let source_fingerprint_after = checkpoint_source_fingerprint(
        &resolved.harness_home,
        &resolved.workspace_root,
        resolved.report_directories.as_deref(),
    )?;
    let manifest = CheckpointStageManifest {
        omitted_reports: omitted_reports.clone(),
        protocol_version: CHECKPOINT_STAGING_PROTOCOL_VERSION,
        session: resolved.session,
        target: resolved.target,
        bundle: resolved.bundle,
        source_fingerprint: (source_fingerprint_before == source_fingerprint_after)
            .then_some(source_fingerprint_after),
        native_artifacts: staged_native,
        repositories: staged_repositories,
    };
    write_private_file(
        temporary.path(),
        Path::new(CHECKPOINT_STAGE_MANIFEST),
        &serde_json::to_vec(&manifest).context("serialize checkpoint stage manifest")?,
        0o600,
    )?;
    fs::rename(temporary.path(), &resolved.stage_path)
        .with_context(|| format!("seal checkpoint stage {}", resolved.stage_path.display()))?;

    Ok(CapturedCheckpoint {
        omitted_reports,
        stage_path: resolved.stage_path,
        native_bytes,
        repository_bytes,
        reused_native: false,
    })
}

pub(super) fn refresh_checkpoint_stage(
    spec: &CheckpointCaptureSpec,
    source_fingerprint_before: &str,
    git: &dyn GitCommandRunner,
) -> Result<Option<CapturedCheckpoint>> {
    let manifest_body = read_staged_file(
        &spec.stage_path,
        Path::new(CHECKPOINT_STAGE_MANIFEST),
        8 * 1024 * 1024,
    )?;
    let mut manifest: CheckpointStageManifest =
        serde_json::from_slice(&manifest_body).context("parse checkpoint stage manifest")?;
    ensure!(
        manifest.protocol_version == CHECKPOINT_STAGING_PROTOCOL_VERSION,
        "unsupported sealed checkpoint stage version {}",
        manifest.protocol_version
    );
    if manifest.session != spec.session
        || manifest.target != spec.target
        || manifest.bundle != spec.bundle
        || manifest.source_fingerprint.as_deref() != Some(source_fingerprint_before)
    {
        return Ok(None);
    }

    let temporary = create_capture_directory(
        spec.stage_path
            .parent()
            .context("checkpoint stage has no parent")?,
    )?;
    let repositories = collect_checkpoint_repositories(
        &spec.workspace_root,
        &spec.repositories,
        git,
        temporary.path(),
    )?;
    let repository_bytes = checkpoint_repository_bytes(&repositories)?;
    let source_fingerprint_after = checkpoint_source_fingerprint(
        &spec.harness_home,
        &spec.workspace_root,
        spec.report_directories.as_deref(),
    )?;
    if source_fingerprint_after != source_fingerprint_before {
        return Ok(None);
    }
    let repositories_root = spec.stage_path.join("repositories");
    if repositories_root.exists() {
        fs::remove_dir_all(&repositories_root).with_context(|| {
            format!(
                "replace prestaged repository state {}",
                repositories_root.display()
            )
        })?;
    }
    manifest.repositories = write_staged_repositories(&spec.stage_path, repositories)?;
    manifest.source_fingerprint = Some(source_fingerprint_after);
    write_private_file(
        &spec.stage_path,
        Path::new(CHECKPOINT_STAGE_MANIFEST),
        &serde_json::to_vec(&manifest).context("serialize refreshed checkpoint stage manifest")?,
        0o600,
    )?;
    let native_bytes = manifest
        .native_artifacts
        .iter()
        .try_fold(0_u64, |total, artifact| total.checked_add(artifact.size))
        .context("native checkpoint size overflow")?;
    Ok(Some(CapturedCheckpoint {
        omitted_reports: manifest.omitted_reports,
        stage_path: spec.stage_path.clone(),
        native_bytes,
        repository_bytes,
        reused_native: true,
    }))
}

pub(super) fn checkpoint_repository_bytes(
    repositories: &[RepositorySnapshot<StagedPayload>],
) -> Result<u64> {
    repositories.iter().try_fold(0_u64, |total, repository| {
        [
            &repository.committed_bundle,
            &repository.staged_patch,
            &repository.unstaged_patch,
            &repository.untracked_tar,
        ]
        .into_iter()
        .chain(
            repository
                .operation_payloads
                .iter()
                .flat_map(|payloads| [&payloads.conflicted_worktree_tar, &payloads.state_tar]),
        )
        .chain(repository.linked_worktrees.iter().flat_map(|worktree| {
            [
                &worktree.staged_patch,
                &worktree.unstaged_patch,
                &worktree.untracked_tar,
            ]
            .into_iter()
            .chain(
                worktree
                    .operation_payloads
                    .iter()
                    .flat_map(|payloads| [&payloads.conflicted_worktree_tar, &payloads.state_tar]),
            )
        }))
        .try_fold(total, |total, payload| {
            total
                .checked_add(payload.size()?)
                .context("repository checkpoint size overflow")
        })
    })
}

fn install_staged_payload(root: &Path, relative: &Path, payload: &StagedPayload) -> Result<()> {
    match payload {
        StagedPayload::Inline(bytes) => write_private_file(root, relative, bytes, 0o600),
        StagedPayload::File(source) => {
            let destination = prepare_private_destination(root, relative)?;
            fs::rename(source, &destination)
                .with_context(|| format!("stage repository payload at {}", destination.display()))
        }
    }
}

pub(super) fn write_staged_repositories(
    stage_root: &Path,
    repositories: Vec<RepositorySnapshot<StagedPayload>>,
) -> Result<Vec<StagedRepository>> {
    repositories
        .into_iter()
        .enumerate()
        .map(|(index, repository)| {
            let root = PathBuf::from(format!("repositories/{index:08}"));
            let committed_bundle_path = root.join("committed.bundle");
            let staged_patch_path = root.join("staged.patch");
            let unstaged_patch_path = root.join("unstaged.patch");
            let untracked_tar_path = root.join("untracked.tar");
            install_staged_payload(
                stage_root,
                &committed_bundle_path,
                &repository.committed_bundle,
            )?;
            install_staged_payload(stage_root, &staged_patch_path, &repository.staged_patch)?;
            install_staged_payload(stage_root, &unstaged_patch_path, &repository.unstaged_patch)?;
            install_staged_payload(stage_root, &untracked_tar_path, &repository.untracked_tar)?;
            let (conflicted_worktree_tar_path, operation_state_tar_path) =
                match &repository.operation_payloads {
                    None => (None, None),
                    Some(payloads) => {
                        let worktree_path = root.join("conflicted-worktree.tar");
                        let state_path = root.join("operation-state.tar");
                        install_staged_payload(
                            stage_root,
                            &worktree_path,
                            &payloads.conflicted_worktree_tar,
                        )?;
                        install_staged_payload(stage_root, &state_path, &payloads.state_tar)?;
                        (Some(worktree_path), Some(state_path))
                    }
                };
            let linked_worktrees = repository
                .linked_worktrees
                .into_iter()
                .enumerate()
                .map(|(worktree_index, worktree)| {
                    let worktree_root = root.join(format!("worktrees/{worktree_index:08}"));
                    let staged_patch_path = worktree_root.join("staged.patch");
                    let unstaged_patch_path = worktree_root.join("unstaged.patch");
                    let untracked_tar_path = worktree_root.join("untracked.tar");
                    for (path, data) in [
                        (&staged_patch_path, &worktree.staged_patch),
                        (&unstaged_patch_path, &worktree.unstaged_patch),
                        (&untracked_tar_path, &worktree.untracked_tar),
                    ] {
                        install_staged_payload(stage_root, path, data)?;
                    }
                    let (conflicted_worktree_tar_path, operation_state_tar_path) = match &worktree
                        .operation_payloads
                    {
                        None => (None, None),
                        Some(payloads) => {
                            let worktree_path = worktree_root.join("conflicted-worktree.tar");
                            let state_path = worktree_root.join("operation-state.tar");
                            install_staged_payload(
                                stage_root,
                                &worktree_path,
                                &payloads.conflicted_worktree_tar,
                            )?;
                            install_staged_payload(stage_root, &state_path, &payloads.state_tar)?;
                            (Some(worktree_path), Some(state_path))
                        }
                    };
                    Ok(StagedLinkedWorktree {
                        metadata: worktree.metadata,
                        staged_patch_path,
                        unstaged_patch_path,
                        untracked_tar_path,
                        conflicted_worktree_tar_path,
                        operation_state_tar_path,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(StagedRepository {
                metadata: repository.metadata,
                committed_bundle_path,
                staged_patch_path,
                unstaged_patch_path,
                untracked_tar_path,
                conflicted_worktree_tar_path,
                operation_state_tar_path,
                linked_worktrees,
            })
        })
        .collect()
}

pub(super) fn native_source_fingerprint(root: &Path) -> Result<String> {
    let mut digest = Sha256::new();
    fingerprint_tree(root, root, &mut digest)?;
    Ok(lower_hex(digest.finalize()))
}

/// What a prestaged generation's native artifacts were read from: the harness
/// home and, when there is one, the sub-agent report directory. A report
/// written after the prestage makes the prestage stale, like a harness file.
/// Without a report directory it is the harness home's own fingerprint.
pub(super) fn checkpoint_source_fingerprint(
    harness_home: &Path,
    workspace_root: &Path,
    report_directories: Option<&[PathBuf]>,
) -> Result<String> {
    let roots = checkpoint_report_sources(workspace_root, report_directories)?;
    if roots.is_empty() {
        return native_source_fingerprint(harness_home);
    }
    let mut digest = Sha256::new();
    fingerprint_tree(harness_home, harness_home, &mut digest)?;
    for (root, prefix) in roots {
        digest.update(b"\0sub-agent reports\0");
        digest.update(prefix.to_string_lossy().as_bytes());
        fingerprint_tree(&root, &root, &mut digest)?;
    }
    Ok(lower_hex(digest.finalize()))
}

/// Both collection and invalidation use this exact ownership selection.
fn checkpoint_report_sources(
    workspace_root: &Path,
    directories: Option<&[PathBuf]>,
) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut roots = Vec::new();
    match directories {
        None => roots.push((
            workspace_root.join(mj_core::subagent::REPORT_ROOT_DIR),
            PathBuf::new(),
        )),
        Some(directories) => {
            let mut children = BTreeSet::new();
            for directory in directories {
                let directory = resolve_target_path(directory)?;
                let child = directory
                    .file_name()
                    .context("report directory has no child id")?;
                validate_component(&child.to_string_lossy(), "report child id")?;
                ensure!(
                    children.insert(child.to_os_string()),
                    "duplicate child report directory"
                );
                roots.push((directory.clone(), PathBuf::from(child)));
            }
        }
    }
    roots.retain(|(root, _)| fs::symlink_metadata(root).is_ok_and(|metadata| metadata.is_dir()));
    roots.sort();
    Ok(roots)
}

/// Newest regular report files that fit, plus metadata about the files left out.
pub(super) fn collect_subagent_reports(
    workspace_root: &Path,
    report_directories: Option<&[PathBuf]>,
) -> Result<(Vec<NativeArtifact>, CheckpointReportOmissions)> {
    let roots = checkpoint_report_sources(workspace_root, report_directories)?;
    let mut files = Vec::new();
    let mut directories = std::collections::BTreeMap::<String, u64>::new();
    let mut pending = roots
        .into_iter()
        .map(|(root, prefix)| (root.clone(), root, prefix))
        .collect::<Vec<_>>();
    while let Some((directory, root, prefix)) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .with_context(|| format!("scan sub-agent reports {}", directory.display()))?
        {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.is_dir() {
                pending.push((entry.path(), root.clone(), prefix.clone()));
            } else if metadata.is_file() {
                let path = entry.path();
                let relative = prefix.join(
                    path.strip_prefix(&root)
                        .context("report escaped its root")?,
                );
                if let Some(child) = relative
                    .parent()
                    .filter(|path| !path.as_os_str().is_empty())
                    .and_then(|_| relative.components().next())
                {
                    let bytes = directories
                        .entry(child.as_os_str().to_string_lossy().into_owned())
                        .or_default();
                    *bytes = bytes.saturating_add(metadata.len());
                }
                let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                files.push((modified, metadata.len(), path, relative));
            }
        }
    }
    files.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.2.cmp(&right.2)));
    let mut total = 0_u64;
    let mut artifacts = Vec::new();
    let mut omitted = CheckpointReportOmissions::default();
    for (_, size, path, relative) in files {
        if total.saturating_add(size) > mj_core::subagent::MAX_CHECKPOINT_REPORT_BYTES {
            omitted.files += 1;
            omitted.bytes = omitted.bytes.saturating_add(size);
            continue;
        }
        let file = File::open(&path)
            .with_context(|| format!("open sub-agent report {}", path.display()))?;
        let mut data = Vec::new();
        (&file)
            .take(mj_core::subagent::MAX_CHECKPOINT_REPORT_BYTES - total + 1)
            .read_to_end(&mut data)
            .with_context(|| format!("read sub-agent report {}", path.display()))?;
        // Files can grow while prestaging without a barrier. Apply the bound
        // to the actual bytes too, rather than sealing an oversized generation.
        if total.saturating_add(data.len() as u64) > mj_core::subagent::MAX_CHECKPOINT_REPORT_BYTES
        {
            omitted.files += 1;
            omitted.bytes = omitted.bytes.saturating_add(
                file.metadata()
                    .with_context(|| format!("inspect growing report {}", path.display()))?
                    .len(),
            );
            continue;
        }
        total += data.len() as u64;
        artifacts.push(NativeArtifact {
            relative_path: Path::new(mj_core::subagent::ARCHIVE_REPORT_DIR).join(relative),
            data: data.into(),
            mode: 0o600,
        });
    }
    if omitted.files > 0 {
        omitted.largest_directories = directories
            .into_iter()
            .map(|(child_id, bytes)| CheckpointReportDirectory { child_id, bytes })
            .collect();
        omitted.largest_directories.sort_by(|a, b| {
            b.bytes
                .cmp(&a.bytes)
                .then_with(|| a.child_id.cmp(&b.child_id))
        });
        omitted.largest_directories.truncate(3);
    }
    Ok((artifacts, omitted))
}

pub(super) fn fingerprint_tree(root: &Path, path: &Path, digest: &mut Sha256) -> Result<()> {
    let mut entries = fs::read_dir(path)
        .with_context(|| format!("scan checkpoint source {}", path.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .context("checkpoint source escaped its root")?;
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("inspect checkpoint source {}", path.display()))?;
        digest.update(relative.to_string_lossy().as_bytes());
        digest.update([0]);
        digest.update(metadata.len().to_le_bytes());
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        digest.update(modified.to_le_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            digest.update(metadata.mode().to_le_bytes());
        }
        if metadata.is_dir() {
            digest.update(b"directory");
            fingerprint_tree(root, &path, digest)?;
        } else if metadata.is_file() {
            digest.update(b"file");
        } else {
            digest.update(b"other");
        }
    }
    Ok(())
}

/// Package a previously sealed target generation with the controller's
/// canonical projection. This operation runs after ACP dispatch resumes.
pub fn pack_checkpoint(spec: &CheckpointPackSpec) -> Result<TargetCheckpoint> {
    let started = std::time::Instant::now();
    ensure!(
        spec.protocol_version == CHECKPOINT_STAGING_PROTOCOL_VERSION,
        "unsupported checkpoint staging protocol version {}; worker supports {}",
        spec.protocol_version,
        CHECKPOINT_STAGING_PROTOCOL_VERSION
    );
    spec.canonical_session.validate()?;
    let relay_root = resolve_target_path(&spec.relay_root)?;
    let stage_path = resolve_target_path(&spec.stage_path)?;
    let output_path = resolve_target_path(&spec.output_path)?;
    validate_stage_path(&relay_root, &stage_path, "checkpoint stage")?;
    validate_stage_path(&relay_root, &output_path, "checkpoint archive")?;
    ensure!(stage_path.is_dir(), "checkpoint stage is missing");

    let result = (|| -> Result<TargetCheckpoint> {
        let manifest_body = read_staged_file(
            &stage_path,
            Path::new(CHECKPOINT_STAGE_MANIFEST),
            8 * 1024 * 1024,
        )?;
        let manifest: CheckpointStageManifest =
            serde_json::from_slice(&manifest_body).context("parse checkpoint stage manifest")?;
        ensure!(
            manifest.protocol_version == CHECKPOINT_STAGING_PROTOCOL_VERSION,
            "unsupported sealed checkpoint stage version {}",
            manifest.protocol_version
        );
        let native_started = std::time::Instant::now();
        let native_artifacts = manifest
            .native_artifacts
            .into_iter()
            .map(|artifact| {
                validate_relative_path(&artifact.body_path)?;
                let path = stage_path.join(&artifact.body_path);
                ensure_no_symlink_ancestors(&stage_path, &artifact.body_path)?;
                let size = fs::symlink_metadata(&path)?;
                ensure!(size.is_file(), "staged native artifact is not a file");
                ensure!(
                    size.len() == artifact.size,
                    "staged native artifact size changed"
                );
                Ok(NativeArtifact {
                    relative_path: artifact.relative_path,
                    data: mj_core::archive::NativeArtifactData::File(path),
                    mode: artifact.mode,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let native_ms = native_started.elapsed().as_millis() as u64;
        let repositories_started = std::time::Instant::now();
        let repositories = read_sealed_checkpoint_repositories(&stage_path, manifest.repositories)?;
        let repositories_ms = repositories_started.elapsed().as_millis() as u64;
        let event_frontier = spec.canonical_session.event_frontier;
        let event_frontier_digest = spec.canonical_session.event_frontier_digest.clone();
        let archive_started = std::time::Instant::now();
        let (sha256, archive_manifest) = write_archive_hashed_repository_payloads(
            &output_path,
            &manifest.session,
            &manifest.target,
            &manifest.bundle,
            &spec.canonical_session,
            &native_artifacts,
            &repositories,
        )?;
        let archive_ms = archive_started.elapsed().as_millis() as u64;
        Ok(TargetCheckpoint {
            path: output_path.clone(),
            sha256,
            event_frontier,
            event_frontier_digest,
            timings: Some(CheckpointExportTimings {
                native_ms,
                repositories_ms,
                archive_ms,
                total_ms: started.elapsed().as_millis() as u64,
            }),
            manifest: Some(archive_manifest),
        })
    })();
    if let Err(cleanup_error) = fs::remove_dir_all(&stage_path)
        && cleanup_error.kind() != std::io::ErrorKind::NotFound
    {
        return match result {
            Ok(_) => Err(cleanup_error).with_context(|| {
                format!("remove consumed checkpoint stage {}", stage_path.display())
            }),
            Err(error) => Err(error.context(format!(
                "also failed to remove checkpoint stage {}: {cleanup_error}",
                stage_path.display()
            ))),
        };
    }
    result
}

pub(super) fn read_sealed_checkpoint_repositories(
    stage_path: &Path,
    repositories: Vec<StagedRepository>,
) -> Result<Vec<RepositorySnapshot<StagedPayload>>> {
    repositories
        .into_iter()
        .map(|repository| {
            Ok(RepositorySnapshot {
                origin_delta_pack: None,
                metadata: repository.metadata,
                committed_bundle: staged_payload(stage_path, &repository.committed_bundle_path)?,
                staged_patch: staged_payload(stage_path, &repository.staged_patch_path)?,
                unstaged_patch: staged_payload(stage_path, &repository.unstaged_patch_path)?,
                untracked_tar: staged_payload(stage_path, &repository.untracked_tar_path)?,
                operation_payloads: staged_operation(
                    stage_path,
                    repository.conflicted_worktree_tar_path.as_deref(),
                    repository.operation_state_tar_path.as_deref(),
                )?,
                linked_worktrees: repository
                    .linked_worktrees
                    .into_iter()
                    .map(|worktree| {
                        Ok(crate::archive::LinkedWorktreeSnapshot {
                            metadata: worktree.metadata,
                            staged_patch: staged_payload(stage_path, &worktree.staged_patch_path)?,
                            unstaged_patch: staged_payload(
                                stage_path,
                                &worktree.unstaged_patch_path,
                            )?,
                            untracked_tar: staged_payload(
                                stage_path,
                                &worktree.untracked_tar_path,
                            )?,
                            operation_payloads: staged_operation(
                                stage_path,
                                worktree.conflicted_worktree_tar_path.as_deref(),
                                worktree.operation_state_tar_path.as_deref(),
                            )?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
            })
        })
        .collect()
}

fn staged_operation(
    stage_path: &Path,
    worktree: Option<&Path>,
    state: Option<&Path>,
) -> Result<Option<crate::archive::GitOperationPayloads<StagedPayload>>> {
    match (worktree, state) {
        (None, None) => Ok(None),
        (Some(worktree), Some(state)) => Ok(Some(crate::archive::GitOperationPayloads {
            conflicted_worktree_tar: staged_payload(stage_path, worktree)?,
            state_tar: staged_payload(stage_path, state)?,
        })),
        _ => bail!("staged snapshot has only one of its Git operation payloads"),
    }
}

fn staged_payload(root: &Path, relative: &Path) -> Result<StagedPayload> {
    let (path, _) = inspect_staged_file(root, relative)?;
    Ok(StagedPayload::File(path))
}

pub(super) fn validate_stage_path(relay_root: &Path, path: &Path, name: &str) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    ensure!(
        parent.starts_with(relay_root),
        "{name} must be beneath relay root"
    );
    Ok(())
}

fn inspect_staged_file(root: &Path, relative: &Path) -> Result<(PathBuf, fs::Metadata)> {
    validate_relative_path(relative)?;
    ensure_no_symlink_ancestors(root, relative)?;
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("inspect staged checkpoint file {}", path.display()))?;
    ensure!(metadata.is_file(), "staged checkpoint path is not a file");
    Ok((path, metadata))
}

pub(super) fn read_staged_file(root: &Path, relative: &Path, maximum: u64) -> Result<Vec<u8>> {
    let (path, metadata) = inspect_staged_file(root, relative)?;
    ensure!(
        metadata.len() <= maximum,
        "staged checkpoint file is too large"
    );
    let mut file = mj_core::archive::NativeArtifactData::open_file(&path)
        .with_context(|| format!("open staged checkpoint file {}", path.display()))?;
    let mut body = Vec::with_capacity(metadata.len().min(usize::MAX as u64) as usize);
    std::io::Read::read_to_end(&mut file, &mut body)
        .with_context(|| format!("read staged checkpoint file {}", path.display()))?;
    ensure!(
        body.len() as u64 == metadata.len(),
        "staged checkpoint file size changed"
    );
    Ok(body)
}

pub fn export_checkpoint(spec: &CheckpointExportSpec) -> Result<TargetCheckpoint> {
    export_checkpoint_with_git(spec, &SystemGit)
}

pub fn export_checkpoint_with_git(
    spec: &CheckpointExportSpec,
    git: &dyn GitCommandRunner,
) -> Result<TargetCheckpoint> {
    export_checkpoint_with_native_state(spec, git, &NoNativeCheckpointState)
}

pub fn export_checkpoint_with_native_state(
    spec: &CheckpointExportSpec,
    git: &dyn GitCommandRunner,
    native_state: &dyn NativeCheckpointState,
) -> Result<TargetCheckpoint> {
    let started = std::time::Instant::now();
    let protocol_version = CheckpointRepositoryCapture::export_protocol_version(&spec.repositories);
    ensure!(
        spec.protocol_version == protocol_version,
        "unsupported checkpoint export protocol version {}; worker supports {}",
        spec.protocol_version,
        protocol_version
    );
    let relay_root = resolve_target_path(&spec.relay_root)?;
    let harness_home =
        checkpoint_harness_home(&relay_root, &resolve_target_path(&spec.harness_home)?)?;
    let workspace_root = resolve_target_path(&spec.workspace_root)?;
    let output_path = resolve_target_path(&spec.output_path)?;
    spec.canonical_session.validate()?;
    validate_checkpoint_source(
        &spec.session,
        &relay_root,
        &harness_home,
        &workspace_root,
        &spec.repositories,
    )?;
    validate_stage_path(&relay_root, &output_path, "checkpoint archive")?;
    let event_frontier = spec.canonical_session.event_frontier;
    let event_frontier_digest = spec.canonical_session.event_frontier_digest.clone();
    // A native session that never received a prompt legitimately has no
    // harness artifacts yet; requiring them would make an unused session, or
    // one just replaced by `/clear`, impossible to close cleanly (R4-5).
    let allow_empty_native = current_native_session_is_unused(
        &spec.canonical_session,
        Some(spec.session.native_session_id.as_str()),
    );
    let native_started = std::time::Instant::now();
    let mut native_artifacts = collect_checkpoint_native_artifacts(
        &spec.session,
        &relay_root,
        &harness_home,
        &workspace_root,
        spec.report_directories.as_deref(),
        allow_empty_native,
        native_state,
    )?;
    // The barrier stops dispatch, but a harness can still finish an async
    // native write. Own the captured bytes before repository collection and ZIP
    // planning, just as the staged capture path does.
    let parent = output_path
        .parent()
        .context("checkpoint archive has no parent")?;
    fs::create_dir_all(parent)?;
    let native_snapshot = create_capture_directory(parent)?;
    for (index, artifact) in native_artifacts.iter_mut().enumerate() {
        if matches!(artifact.data, mj_core::archive::NativeArtifactData::File(_)) {
            let relative = PathBuf::from(index.to_string());
            {
                let mut reader = artifact.reader()?;
                write_private_reader(native_snapshot.path(), &relative, &mut reader, 0o600)?;
            }
            artifact.data =
                mj_core::archive::NativeArtifactData::File(native_snapshot.path().join(relative));
        }
    }
    let native_ms = native_started.elapsed().as_millis() as u64;
    let repositories_started = std::time::Instant::now();
    if spec
        .repositories
        .iter()
        .any(|repository| repository.capture == CheckpointRepositoryCapture::OriginDelta)
    {
        verify_origin_delta_capture_scope(
            &relay_root,
            &workspace_root,
            &spec.session.id,
            &spec.repositories,
            git,
        )?;
    }
    let repositories = collect_checkpoint_repositories_with_policy(
        &workspace_root,
        &spec.repositories,
        git,
        Some(spec.session.id.as_str()),
        native_snapshot.path(),
        crate::archive::UntrackedFiles::Include,
        &spec.consent,
    )?;
    let repositories_ms = repositories_started.elapsed().as_millis() as u64;
    // The export runs while the relay's barrier freezes ACP dispatch, so it
    // hashes the archive it just wrote instead of structurally re-reading it.
    // `CheckpointTransfer::execute` checks the controller's copy against this
    // digest; resume/import performs full structural verification when read.
    let archive_started = std::time::Instant::now();
    let (sha256, archive_manifest) = write_archive_hashed_repository_payloads(
        &output_path,
        &spec.session,
        &spec.target,
        &spec.bundle,
        &spec.canonical_session,
        &native_artifacts,
        &repositories,
    )?;
    let archive_ms = archive_started.elapsed().as_millis() as u64;
    Ok(TargetCheckpoint {
        path: output_path,
        sha256,
        event_frontier,
        event_frontier_digest,
        timings: Some(CheckpointExportTimings {
            native_ms,
            repositories_ms,
            archive_ms,
            total_ms: started.elapsed().as_millis() as u64,
        }),
        manifest: Some(archive_manifest),
    })
}

pub(super) fn collect_checkpoint_repositories(
    workspace_root: &Path,
    specifications: &[CheckpointRepositorySpec],
    git: &dyn GitCommandRunner,
    snapshot_directory: &Path,
) -> Result<Vec<RepositorySnapshot<StagedPayload>>> {
    collect_checkpoint_repositories_with_policy(
        workspace_root,
        specifications,
        git,
        None,
        snapshot_directory,
        crate::archive::UntrackedFiles::Include,
        &crate::snapshot::CaptureConsent::default(),
    )
}

fn verify_origin_delta_capture_scope(
    relay_root: &Path,
    workspace_root: &Path,
    session_id: &str,
    repositories: &[CheckpointRepositorySpec],
    git: &dyn GitCommandRunner,
) -> Result<()> {
    use mj_core::worker_launch::{WorkerLaunchConfig, WorkerRunMode};
    use mj_core::worker_suspend::{self as wire, SuspendReceipt, SuspendRequest};

    let launch = WorkerLaunchConfig::read(&relay_root.join("launch.json"))?;
    ensure!(
        launch.session_id == session_id && launch.run_mode == WorkerRunMode::CheckpointOnly,
        "managed clone capture requires a checkpoint-only worker for this session"
    );
    let checkout = if let Some(capture) = launch.suspend_capture {
        // The caller holds the relay barrier. This worker serves that barrier
        // only after draining writers under its root lock.
        let operation: String = wire::read(relay_root, wire::ACCEPTED)?
            .context("managed clone capture has no accepted suspend operation")?;
        ensure!(
            capture.operation == operation,
            "capture worker belongs to a different suspend operation"
        );
        capture.checkout
    } else {
        // restart_worker_for_checkpoint can replace hel while retaining a
        // legacy capture launch. Keep its receipt until those launches can
        // no longer reach the current exporter through that restart.
        let request: SuspendRequest = wire::read(relay_root, wire::REQUEST)?
            .context("managed clone capture has no quiescence request")?;
        let receipt: SuspendReceipt = wire::read(relay_root, wire::RECEIPT)?
            .context("managed clone capture has no quiescence receipt")?;
        ensure!(
            receipt.operation == request.operation && receipt.error.is_none(),
            "managed clone capture requires a successful quiescence receipt"
        );
        request.checkout
    };
    let quiesced_checkout = checkout.canonicalize()?;
    for repository in repositories
        .iter()
        .filter(|repository| repository.capture == CheckpointRepositoryCapture::OriginDelta)
    {
        let location = crate::archive::git::locate_checkout(
            git,
            &workspace_root.join(&repository.relative_destination),
        )?;
        ensure!(
            location
                .top_level
                .canonicalize()?
                .starts_with(&quiesced_checkout),
            "suspend capture scope belongs to a different checkout"
        );
    }
    Ok(())
}

/// Entire `.mj/keep` directory under a managed checkout, scanned by suspend
/// preflight and snapshot capture before a managed checkout is deleted so
/// current per-session files (`.mj/keep/<session-id>/...`), legacy direct
/// files (`.mj/keep/...`), and sibling subdirectories hidden by `/.mj/keep/`
/// in `.git/info/exclude` are all listed and require discard consent.
pub fn checkout_keep_dir(checkout: &Path) -> PathBuf {
    mj_core::session_keep::snapshot_keep_dir(checkout, None)
        .expect("checkout_keep_dir has no session_id to reject")
}

/// Per-session `.mj/keep/<session-id>` directory under a repository checkout,
/// shared by worker launch (`MJ_KEEP_DIR`) and Move transfer / `--skip`.
pub fn session_keep_dir(checkout: &Path, session_id: &str) -> Result<PathBuf> {
    mj_core::session_keep::snapshot_keep_dir(checkout, Some(session_id))
}

pub fn managed_clone_snapshot_input(
    id: &str,
    path: &Path,
    _session_id: Option<&str>,
    pack_path: PathBuf,
    origin_override: Option<String>,
) -> Result<crate::snapshot::RepositoryInput> {
    use crate::snapshot::{CheckoutOwnership, ExclusionSource, RepositoryInput};

    Ok(RepositoryInput {
        id: id.to_owned(),
        checkout: path.to_path_buf(),
        ownership: CheckoutOwnership::Managed,
        origin_override,
        pack_path,
        exclusions: ExclusionSource::Origin,
        keep_dir: Some(checkout_keep_dir(path)),
    })
}

fn collect_origin_delta(
    git: &dyn GitCommandRunner,
    path: &Path,
    repository: &CheckpointRepositorySpec,
    session_id: Option<&str>,
    snapshot_directory: &Path,
    consent: &crate::snapshot::CaptureConsent,
) -> Result<RepositorySnapshot<StagedPayload>> {
    use crate::snapshot::{SessionCaptureConsent, SnapshotHead, capture_session_snapshot};

    let location = crate::archive::git::locate_checkout(git, path)?;
    let input = managed_clone_snapshot_input(
        &repository.id,
        &location.top_level,
        session_id,
        snapshot_directory.join(&repository.id),
        repository.origin_override.clone(),
    )?;
    let session_consent = SessionCaptureConsent {
        repositories: [(repository.id.clone(), consent.clone())]
            .into_iter()
            .collect(),
    };
    let mut snapshot = capture_session_snapshot(git, &[input], &session_consent)?
        .repositories
        .remove(&repository.id)
        .context("origin delta snapshot is missing")?;

    Ok(RepositorySnapshot {
        metadata: crate::archive::RepositoryMetadata {
            id: repository.id.clone(),
            relative_destination: repository.relative_destination.clone(),
            checkout_subdirectory: location.subdirectory,
            origin: snapshot
                .origin_url
                .clone()
                .context("managed checkout has no origin")?,
            push_urls: Vec::new(),
            remote_workspace: false,
            plain_directory: false,
            base_commit: String::new(),
            head_commit: snapshot.head.commit().to_owned(),
            branch: match &snapshot.head {
                SnapshotHead::Branch { name, .. } => Some(name.clone()),
                SnapshotHead::Detached(_) => None,
            },
            review_refs_metadata_only: false,
            review_baseline_ref: None,
            saved_refs: Default::default(),
            stash_stack: Vec::new(),
            git_operation: None,
            origin_delta: Some(Box::new(snapshot.metadata)),
        },
        origin_delta_pack: snapshot.pack.take(),
        committed_bundle: StagedPayload::default(),
        staged_patch: StagedPayload::default(),
        unstaged_patch: StagedPayload::default(),
        untracked_tar: StagedPayload::default(),
        operation_payloads: None,
        linked_worktrees: Vec::new(),
    })
}

pub fn raw_snapshot_input(
    id: &str,
    path: &Path,
    launch: Option<crate::snapshot::LaunchRecord>,
    origin_override: Option<String>,
    pack_path: PathBuf,
) -> crate::snapshot::RepositoryInput {
    use crate::snapshot::{CheckoutOwnership, ExclusionSource, RepositoryInput};

    RepositoryInput {
        id: id.to_owned(),
        checkout: path.to_path_buf(),
        ownership: CheckoutOwnership::Raw { launch },
        origin_override,
        pack_path,
        exclusions: ExclusionSource::Origin,
        keep_dir: None,
    }
}

fn collect_origin_delta_raw(
    git: &dyn GitCommandRunner,
    path: &Path,
    repository: &CheckpointRepositorySpec,
    session_id: &str,
    snapshot_directory: &Path,
    consent: &crate::snapshot::CaptureConsent,
) -> Result<RepositorySnapshot<StagedPayload>> {
    use crate::snapshot::{
        LaunchRecord, SessionCaptureConsent, SnapshotHead, capture_session_snapshot,
    };

    let location = crate::archive::git::locate_checkout(git, path)?;
    let launch = LaunchRecord::read(git, &location.top_level, session_id)?;
    let input = raw_snapshot_input(
        &repository.id,
        &location.top_level,
        launch,
        repository.origin_override.clone(),
        snapshot_directory.join(&repository.id),
    );
    let session_consent = SessionCaptureConsent {
        repositories: [(repository.id.clone(), consent.clone())]
            .into_iter()
            .collect(),
    };
    let mut snapshot = capture_session_snapshot(git, &[input], &session_consent)?
        .repositories
        .remove(&repository.id)
        .context("raw origin delta snapshot is missing")?;

    Ok(RepositorySnapshot {
        metadata: crate::archive::RepositoryMetadata {
            id: repository.id.clone(),
            relative_destination: repository.relative_destination.clone(),
            checkout_subdirectory: location.subdirectory,
            origin: snapshot.origin_url.clone().unwrap_or_default(),
            push_urls: Vec::new(),
            remote_workspace: false,
            plain_directory: false,
            base_commit: String::new(),
            head_commit: snapshot.head.commit().to_owned(),
            branch: match &snapshot.head {
                SnapshotHead::Branch { name, .. } => Some(name.clone()),
                SnapshotHead::Detached(_) => None,
            },
            review_refs_metadata_only: false,
            review_baseline_ref: None,
            saved_refs: Default::default(),
            stash_stack: Vec::new(),
            git_operation: None,
            origin_delta: Some(Box::new(snapshot.metadata)),
        },
        origin_delta_pack: snapshot.pack.take(),
        committed_bundle: StagedPayload::default(),
        staged_patch: StagedPayload::default(),
        unstaged_patch: StagedPayload::default(),
        untracked_tar: StagedPayload::default(),
        operation_payloads: None,
        linked_worktrees: Vec::new(),
    })
}

pub fn cutoff_snapshot_input(
    id: &str,
    path: &Path,
    _session_id: Option<&str>,
    exclude_commits: Vec<String>,
    pack_path: PathBuf,
) -> Result<crate::snapshot::RepositoryInput> {
    use crate::snapshot::{CheckoutOwnership, ExclusionSource, RepositoryInput};

    Ok(RepositoryInput {
        id: id.to_owned(),
        checkout: path.to_path_buf(),
        ownership: CheckoutOwnership::Managed,
        origin_override: None,
        pack_path,
        exclusions: ExclusionSource::Commits(exclude_commits),
        keep_dir: Some(checkout_keep_dir(path)),
    })
}

fn collect_origin_delta_cutoff(
    git: &dyn GitCommandRunner,
    path: &Path,
    repository: &CheckpointRepositorySpec,
    session_id: Option<&str>,
    exclude_commits: Vec<String>,
    snapshot_directory: &Path,
    consent: &crate::snapshot::CaptureConsent,
) -> Result<RepositorySnapshot<StagedPayload>> {
    use crate::snapshot::{SessionCaptureConsent, SnapshotHead, capture_session_snapshot};

    let location = crate::archive::git::locate_checkout(git, path)?;
    let input = cutoff_snapshot_input(
        &repository.id,
        &location.top_level,
        session_id,
        exclude_commits,
        snapshot_directory.join(&repository.id),
    )?;
    let session_consent = SessionCaptureConsent {
        repositories: [(repository.id.clone(), consent.clone())]
            .into_iter()
            .collect(),
    };
    let mut snapshot = capture_session_snapshot(git, &[input], &session_consent)?
        .repositories
        .remove(&repository.id)
        .context("history-cutoff origin delta snapshot is missing")?;

    Ok(RepositorySnapshot {
        metadata: crate::archive::RepositoryMetadata {
            id: repository.id.clone(),
            relative_destination: repository.relative_destination.clone(),
            checkout_subdirectory: location.subdirectory,
            origin: String::new(),
            push_urls: Vec::new(),
            remote_workspace: false,
            plain_directory: false,
            base_commit: String::new(),
            head_commit: snapshot.head.commit().to_owned(),
            branch: match &snapshot.head {
                SnapshotHead::Branch { name, .. } => Some(name.clone()),
                SnapshotHead::Detached(_) => None,
            },
            review_refs_metadata_only: false,
            review_baseline_ref: None,
            saved_refs: Default::default(),
            stash_stack: Vec::new(),
            git_operation: None,
            origin_delta: Some(Box::new(snapshot.metadata)),
        },
        origin_delta_pack: snapshot.pack.take(),
        committed_bundle: StagedPayload::default(),
        staged_patch: StagedPayload::default(),
        unstaged_patch: StagedPayload::default(),
        untracked_tar: StagedPayload::default(),
        operation_payloads: None,
        linked_worktrees: Vec::new(),
    })
}

fn collect_checkpoint_repositories_with_policy(
    workspace_root: &Path,
    specifications: &[CheckpointRepositorySpec],
    git: &dyn GitCommandRunner,
    session_id: Option<&str>,
    snapshot_directory: &Path,
    untracked_files: crate::archive::UntrackedFiles,
    consent: &crate::snapshot::CaptureConsent,
) -> Result<Vec<RepositorySnapshot<StagedPayload>>> {
    // Indexed parallel iteration preserves the spec order, which in turn keeps
    // the manifest and ZIP entry order deterministic.
    specifications
        .par_iter()
        .map(|repository| {
            let path = workspace_root.join(&repository.relative_destination);
            ensure!(path.is_dir(), "repository {} is missing", path.display());
            let (history, remote_base) = match &repository.capture {
                CheckpointRepositoryCapture::OriginDelta => {
                    ensure!(
                        session_id.is_some(),
                        "origin-delta suspend requires a single-shot export"
                    );
                    return collect_origin_delta(
                        git,
                        &path,
                        repository,
                        session_id,
                        snapshot_directory,
                        consent,
                    );
                }
                CheckpointRepositoryCapture::OriginDeltaRaw => {
                    return collect_origin_delta_raw(
                        git,
                        &path,
                        repository,
                        session_id.context("raw origin-delta export requires a session id")?,
                        snapshot_directory,
                        consent,
                    );
                }
                CheckpointRepositoryCapture::OriginDeltaCutoff { exclude_commits } => {
                    ensure!(
                        session_id.is_some(),
                        "history-cutoff origin-delta export requires a single-shot export"
                    );
                    return collect_origin_delta_cutoff(
                        git,
                        &path,
                        repository,
                        session_id,
                        exclude_commits.clone(),
                        snapshot_directory,
                        consent,
                    );
                }
                CheckpointRepositoryCapture::PlainDirectory => {
                    return collect_plain_directory_snapshot_with(&GitCollectionSpec {
                        id: repository.id.clone(),
                        relative_destination: repository.relative_destination.clone(),
                        history: GitHistoryMode::NoBundle,
                        origin_override: None,
                    })
                    .with_context(|| format!("repository '{}'", repository.id));
                }
                CheckpointRepositoryCapture::MetadataOnly => {
                    return collect_git_metadata_snapshot_with(
                        git,
                        &path,
                        &GitCollectionSpec {
                            id: repository.id.clone(),
                            relative_destination: repository.relative_destination.clone(),
                            history: GitHistoryMode::NoBundle,
                            origin_override: repository.origin_override.clone(),
                        },
                    )
                    .with_context(|| format!("repository '{}'", repository.id));
                }
                CheckpointRepositoryCapture::SessionDelta => {
                    repair_origin_refs(git, &path, &repository.id)?;
                    (GitHistoryMode::SessionDelta, None)
                }
                CheckpointRepositoryCapture::ManagedClone { base_commit } => {
                    (GitHistoryMode::CloneFrom(base_commit.clone()), None)
                }
                CheckpointRepositoryCapture::DeltaFrom { base_commit } => {
                    (GitHistoryMode::DeltaFrom(base_commit.clone()), None)
                }
                CheckpointRepositoryCapture::RemoteWorkspace => {
                    let base_commit = remote_workspace_base(git, &path)
                        .with_context(|| format!("repository '{}'", repository.id))?
                        .with_context(|| {
                            format!(
                                "repository '{}' is not marked as a managed remote workspace",
                                repository.id
                            )
                        })?;
                    (
                        GitHistoryMode::CloneFrom(base_commit.clone()),
                        Some(base_commit),
                    )
                }
            };
            reject_dirty_submodules(git, &path)
                .with_context(|| format!("repository '{}'", repository.id))?;
            for worktree in crate::archive::linked_worktree_directories(git, &path)
                .with_context(|| format!("repository '{}'", repository.id))?
            {
                reject_dirty_submodules(git, &worktree).with_context(|| {
                    format!(
                        "repository '{}' linked worktree {}",
                        repository.id,
                        worktree.display()
                    )
                })?;
            }
            let mut snapshot = collect_file_backed_git_snapshot(
                git,
                &path,
                &GitCollectionSpec {
                    id: repository.id.clone(),
                    relative_destination: repository.relative_destination.clone(),
                    history,
                    origin_override: repository.origin_override.clone(),
                },
                untracked_files,
                snapshot_directory,
            )
            .with_context(|| format!("repository '{}'", repository.id))?;
            if let Some(base_commit) = remote_base {
                // select_git_history records the merge base. Preserve the
                // immutable launch value exactly so a split/fork checkpoint
                // can restore the workspace marker after origin moved.
                ensure!(
                    !snapshot.metadata.origin.is_empty(),
                    "repository '{}' managed workspace has no Git origin",
                    repository.id
                );
                mj_core::remote_git::validate_network_url(&snapshot.metadata.origin)?;
                for url in &snapshot.metadata.push_urls {
                    mj_core::remote_git::validate_network_url(url)?;
                }
                snapshot.metadata.base_commit = base_commit;
                snapshot.metadata.remote_workspace = true;
            }
            Ok(snapshot)
        })
        .collect()
}

/// Use the installed worker's source paths while retaining the controller's
/// requirement that the export belongs to the same physical harness home.
pub fn checkpoint_harness_home(relay_root: &Path, requested: &Path) -> Result<PathBuf> {
    let Some(installed) =
        mj_core::worker_launch::installed_harness_home(&relay_root.join("launch.json"))?
    else {
        // Earlier launch formats without a home use the checkpoint protocol's
        // explicit source, as do standalone exports without a live worker.
        return Ok(requested.to_path_buf());
    };
    let installed = resolve_home_relative_target_path(&installed)?;
    ensure!(
        fs::canonicalize(requested).context("resolve checkpoint harness home")?
            == fs::canonicalize(&installed).context("resolve installed worker harness home")?,
        "checkpoint harness home does not match the installed worker"
    );
    // An upgraded controller names <worker root>/profile, which can still be
    // a link to an older worker's original home. Its launch file owns the
    // paths used by both native history and the project-memory replica.
    Ok(installed)
}

/// Codex exports carry the relay-root scan cache so that a long-lived session
/// probes each unrelated rollout at most once.
pub(super) fn collect_checkpoint_native_artifacts(
    session: &SessionManifest,
    relay_root: &Path,
    harness_home: &Path,
    workspace_root: &Path,
    report_directories: Option<&[PathBuf]>,
    allow_empty: bool,
    native_state: &dyn NativeCheckpointState,
) -> Result<Vec<NativeArtifact>> {
    Ok(collect_checkpoint_native_artifacts_with_reports(
        session,
        relay_root,
        harness_home,
        workspace_root,
        report_directories,
        allow_empty,
        native_state,
    )?
    .0)
}

pub(super) fn collect_checkpoint_native_artifacts_with_reports(
    session: &SessionManifest,
    relay_root: &Path,
    harness_home: &Path,
    workspace_root: &Path,
    report_directories: Option<&[PathBuf]>,
    allow_empty: bool,
    native_state: &dyn NativeCheckpointState,
) -> Result<(Vec<NativeArtifact>, CheckpointReportOmissions)> {
    let session_id = &session.native_session_id;
    let mut artifacts = if session.harness_kind != HarnessKind::Codex {
        collect_native_artifacts(session.harness_kind, harness_home, session_id, allow_empty)?
    } else {
        let mut cache = load_codex_scan_cache(relay_root, session_id);
        let known = cache.not_ours.len();
        let artifacts = collect_native_artifacts_cached(
            HarnessKind::Codex,
            harness_home,
            session_id,
            allow_empty,
            Some(&mut cache),
        )?;
        if cache.not_ours.len() != known {
            save_codex_scan_cache(relay_root, &cache)?;
        }
        artifacts
    };
    artifacts.extend(native_state.collect(session, harness_home)?);
    artifacts.extend(mj_core::attachment::AttachmentStore::worker(relay_root).archive_artifacts()?);
    let launch_path = relay_root.join("launch.json");
    let launch = read_project_memory_checkpoint_endpoint(&launch_path);
    // Children share the parent's workspace; only the parent's checkpoint
    // carries that report tree. The stable launch flag owns this distinction.
    let (reports, omitted_reports) = if launch.as_ref().is_ok_and(|launch| launch.handback_tool) {
        (Vec::new(), CheckpointReportOmissions::default())
    } else {
        collect_subagent_reports(workspace_root, report_directories)?
    };
    artifacts.extend(reports);
    match launch {
        Ok(launch) => {
            if let Some(memory) = launch.project_memory {
                let root = resolve_home_relative_target_path(&memory.root)?;
                anyhow::ensure!(
                    root.starts_with(harness_home),
                    "project memory replica is outside the harness home"
                );
                if root.is_dir() {
                    collect_claude_memory_tree(harness_home, &root, &mut artifacts)?;
                }
            }
        }
        Err(_error) if !launch_path.exists() => {}
        Err(error) => return Err(error.context("read project memory checkpoint endpoint")),
    }
    artifacts.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    artifacts.dedup_by(|left, right| left.relative_path == right.relative_path);
    Ok((artifacts, omitted_reports))
}

/// Checkpoint exporters can be refreshed independently of the live worker, so
/// read only the stable launch fields needed for collection. Fully parsing the
/// current worker config would reject launch files written by older versions.
#[derive(Deserialize)]
pub(super) struct ProjectMemoryCheckpointLaunch {
    #[serde(default)]
    handback_tool: bool,
    #[serde(default)]
    project_memory: Option<ProjectMemoryCheckpointEndpoint>,
}

#[derive(Deserialize)]
pub(super) struct ProjectMemoryCheckpointEndpoint {
    root: PathBuf,
}

pub(super) fn read_project_memory_checkpoint_endpoint(
    path: &Path,
) -> Result<ProjectMemoryCheckpointLaunch> {
    let body =
        fs::read(path).with_context(|| format!("read worker launch config {}", path.display()))?;
    serde_json::from_slice(&body)
        .with_context(|| format!("parse worker launch config {}", path.display()))
}

pub(super) fn validate_capture_spec(spec: &CheckpointCaptureSpec) -> Result<()> {
    validate_checkpoint_source(
        &spec.session,
        &spec.relay_root,
        &spec.harness_home,
        &spec.workspace_root,
        &spec.repositories,
    )?;
    validate_stage_path(&spec.relay_root, &spec.stage_path, "checkpoint stage")
}

pub(super) fn validate_checkpoint_source(
    session: &SessionManifest,
    relay_root: &Path,
    harness_home: &Path,
    workspace_root: &Path,
    repositories: &[CheckpointRepositorySpec],
) -> Result<()> {
    validate_component(&session.id, "session ID")?;
    validate_component(&session.native_session_id, "native session ID")?;
    ensure!(relay_root.is_dir(), "relay root is missing");
    ensure!(harness_home.is_dir(), "harness home is missing");
    ensure!(workspace_root.is_dir(), "workspace root is missing");
    ensure!(!repositories.is_empty(), "checkpoint has no repositories");
    let mut ids = BTreeSet::new();
    let mut destinations = BTreeSet::new();
    for repository in repositories {
        validate_component(&repository.id, "repository ID")?;
        validate_relative_path(&repository.relative_destination)?;
        if let CheckpointRepositoryCapture::DeltaFrom { base_commit } = &repository.capture {
            ensure!(!base_commit.trim().is_empty(), "base commit is empty");
        }
        ensure!(ids.insert(&repository.id), "duplicate repository ID");
        ensure!(
            destinations.insert(&repository.relative_destination),
            "duplicate destination"
        );
    }
    Ok(())
}
