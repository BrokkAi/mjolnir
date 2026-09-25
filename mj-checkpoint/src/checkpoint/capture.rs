use super::*;
use mj_core::hex::lower_hex;

pub(super) const CHECKPOINT_STAGE_MANIFEST: &str = "stage.json";

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
    resolved.harness_home = resolve_target_path(&resolved.harness_home)?;
    resolved.workspace_root = resolve_target_path(&resolved.workspace_root)?;
    resolved.stage_path = resolve_target_path(&resolved.stage_path)?;
    validate_capture_spec(&resolved)?;

    let source_fingerprint_before =
        checkpoint_source_fingerprint(&resolved.harness_home, &resolved.workspace_root)?;
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

    let native_artifacts = collect_checkpoint_native_artifacts(
        &resolved.session,
        &resolved.relay_root,
        &resolved.harness_home,
        &resolved.workspace_root,
        resolved.allow_empty_native,
        native_state,
    )?;
    let repositories =
        collect_checkpoint_repositories(&resolved.workspace_root, &resolved.repositories, git)?;
    let native_bytes = native_artifacts
        .iter()
        .try_fold(0_u64, |total, artifact| {
            total.checked_add(artifact.data.len() as u64)
        })
        .context("native checkpoint size overflow")?;
    let repository_bytes = checkpoint_repository_bytes(&repositories)?;

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
    let temporary = tempfile::Builder::new()
        .prefix(".checkpoint-capture-")
        .tempdir_in(parent)
        .with_context(|| format!("create temporary checkpoint stage in {}", parent.display()))?;

    let mut staged_native = Vec::with_capacity(native_artifacts.len());
    for (index, artifact) in native_artifacts.into_iter().enumerate() {
        let body_path = PathBuf::from(format!("native/{index:08}"));
        write_private_file(temporary.path(), &body_path, &artifact.data, 0o600)?;
        staged_native.push(StagedNativeArtifact {
            relative_path: artifact.relative_path,
            mode: artifact.mode,
            size: artifact.data.len() as u64,
            body_path,
        });
    }
    let staged_repositories = write_staged_repositories(temporary.path(), repositories)?;
    let source_fingerprint_after =
        checkpoint_source_fingerprint(&resolved.harness_home, &resolved.workspace_root)?;
    let manifest = CheckpointStageManifest {
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

    let repositories =
        collect_checkpoint_repositories(&spec.workspace_root, &spec.repositories, git)?;
    let repository_bytes = checkpoint_repository_bytes(&repositories)?;
    let source_fingerprint_after =
        checkpoint_source_fingerprint(&spec.harness_home, &spec.workspace_root)?;
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
        stage_path: spec.stage_path.clone(),
        native_bytes,
        repository_bytes,
        reused_native: true,
    }))
}

pub(super) fn checkpoint_repository_bytes(repositories: &[RepositorySnapshot]) -> Result<u64> {
    repositories
        .iter()
        .try_fold(0_u64, |total, repository| {
            [
                repository.committed_bundle.len(),
                repository.staged_patch.len(),
                repository.unstaged_patch.len(),
                repository.untracked_tar.len(),
            ]
            .into_iter()
            .try_fold(total, |total, size| total.checked_add(size as u64))
        })
        .context("repository checkpoint size overflow")
}

pub(super) fn write_staged_repositories(
    stage_root: &Path,
    repositories: Vec<RepositorySnapshot>,
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
            write_private_file(
                stage_root,
                &committed_bundle_path,
                &repository.committed_bundle,
                0o600,
            )?;
            write_private_file(
                stage_root,
                &staged_patch_path,
                &repository.staged_patch,
                0o600,
            )?;
            write_private_file(
                stage_root,
                &unstaged_patch_path,
                &repository.unstaged_patch,
                0o600,
            )?;
            write_private_file(
                stage_root,
                &untracked_tar_path,
                &repository.untracked_tar,
                0o600,
            )?;
            Ok(StagedRepository {
                metadata: repository.metadata,
                committed_bundle_path,
                staged_patch_path,
                unstaged_patch_path,
                untracked_tar_path,
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
) -> Result<String> {
    let reports = workspace_root.join(mj_core::subagent::REPORT_ROOT_DIR);
    if !fs::symlink_metadata(&reports).is_ok_and(|metadata| metadata.is_dir()) {
        return native_source_fingerprint(harness_home);
    }
    let mut digest = Sha256::new();
    fingerprint_tree(harness_home, harness_home, &mut digest)?;
    digest.update(b"\0sub-agent reports\0");
    fingerprint_tree(&reports, &reports, &mut digest)?;
    Ok(lower_hex(digest.finalize()))
}

/// The most sub-agent report bytes one checkpoint carries. Reports are the
/// details a child's short handback points to, so a checkpoint keeps the
/// newest files up to this bound rather than failing over them.
pub(super) const MAX_SUBAGENT_REPORT_BYTES: u64 = 64 * 1024 * 1024;

/// The regular files under `<workspace_root>/.mj-agents`, as native artifacts
/// under [`mj_core::subagent::ARCHIVE_REPORT_DIR`]. The directory sits beside
/// the repositories, outside all of them, so no repository capture sees it.
/// Symbolic links are skipped: a report is a file its child wrote.
pub(super) fn collect_subagent_reports(workspace_root: &Path) -> Result<Vec<NativeArtifact>> {
    let root = workspace_root.join(mj_core::subagent::REPORT_ROOT_DIR);
    if !fs::symlink_metadata(&root).is_ok_and(|metadata| metadata.is_dir()) {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    let mut pending = vec![root.clone()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .with_context(|| format!("scan sub-agent reports {}", directory.display()))?
        {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.is_file() {
                let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                files.push((modified, metadata.len(), entry.path()));
            }
        }
    }
    // Newest first, so the bound drops the oldest reports.
    files.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.2.cmp(&right.2)));
    let mut total = 0_u64;
    let mut artifacts = Vec::new();
    let mut skipped = 0_usize;
    for (_, size, path) in files {
        if total.saturating_add(size) > MAX_SUBAGENT_REPORT_BYTES {
            skipped += 1;
            continue;
        }
        let data =
            fs::read(&path).with_context(|| format!("read sub-agent report {}", path.display()))?;
        total += data.len() as u64;
        let relative = path
            .strip_prefix(&root)
            .context("sub-agent report escaped its directory")?;
        artifacts.push(NativeArtifact {
            relative_path: Path::new(mj_core::subagent::ARCHIVE_REPORT_DIR).join(relative),
            data,
            mode: 0o600,
        });
    }
    if skipped > 0 {
        // This runs on the target, where standard error is the only log.
        eprintln!(
            "sub-agent reports exceed the {MAX_SUBAGENT_REPORT_BYTES}-byte checkpoint limit; \
             the oldest {skipped} were left out"
        );
    }
    Ok(artifacts)
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
                let data = read_staged_file(&stage_path, &artifact.body_path, MAX_NATIVE_FILE)?;
                ensure!(
                    data.len() as u64 == artifact.size,
                    "staged native artifact size changed"
                );
                Ok(NativeArtifact {
                    relative_path: artifact.relative_path,
                    data,
                    mode: artifact.mode,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let native_ms = native_started.elapsed().as_millis() as u64;
        let repositories_started = std::time::Instant::now();
        let repositories = manifest
            .repositories
            .into_iter()
            .map(|repository| {
                Ok(RepositorySnapshot {
                    metadata: repository.metadata,
                    committed_bundle: read_staged_file(
                        &stage_path,
                        &repository.committed_bundle_path,
                        MAX_NATIVE_TOTAL,
                    )?,
                    staged_patch: read_staged_file(
                        &stage_path,
                        &repository.staged_patch_path,
                        MAX_NATIVE_TOTAL,
                    )?,
                    unstaged_patch: read_staged_file(
                        &stage_path,
                        &repository.unstaged_patch_path,
                        MAX_NATIVE_TOTAL,
                    )?,
                    untracked_tar: read_staged_file(
                        &stage_path,
                        &repository.untracked_tar_path,
                        MAX_NATIVE_TOTAL,
                    )?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let repositories_ms = repositories_started.elapsed().as_millis() as u64;
        let event_frontier = spec.canonical_session.event_frontier;
        let event_frontier_digest = spec.canonical_session.event_frontier_digest.clone();
        let archive_started = std::time::Instant::now();
        let sha256 = write_archive_hashed(
            &output_path,
            &ArchiveInput {
                session: manifest.session,
                target: manifest.target,
                bundle: manifest.bundle,
                canonical_session: spec.canonical_session.clone(),
                native_artifacts,
                repositories,
            },
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

pub(super) fn validate_stage_path(relay_root: &Path, path: &Path, name: &str) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    ensure!(
        parent.starts_with(relay_root),
        "{name} must be beneath relay root"
    );
    Ok(())
}

pub(super) fn read_staged_file(root: &Path, relative: &Path, maximum: u64) -> Result<Vec<u8>> {
    validate_relative_path(relative)?;
    ensure_no_symlink_ancestors(root, relative)?;
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("inspect staged checkpoint file {}", path.display()))?;
    ensure!(metadata.is_file(), "staged checkpoint path is not a file");
    ensure!(
        metadata.len() <= maximum,
        "staged checkpoint file is too large"
    );
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(&path)
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
    ensure!(
        spec.protocol_version == CHECKPOINT_EXPORT_PROTOCOL_VERSION,
        "unsupported checkpoint export protocol version {}; worker supports {}",
        spec.protocol_version,
        CHECKPOINT_EXPORT_PROTOCOL_VERSION
    );
    let relay_root = resolve_target_path(&spec.relay_root)?;
    let harness_home = resolve_target_path(&spec.harness_home)?;
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
    let prompted = current_native_session_received_prompt(&spec.canonical_session);
    let native_started = std::time::Instant::now();
    let native_artifacts = collect_checkpoint_native_artifacts(
        &spec.session,
        &relay_root,
        &harness_home,
        &workspace_root,
        !prompted,
        native_state,
    )?;
    let native_ms = native_started.elapsed().as_millis() as u64;
    let repositories_started = std::time::Instant::now();
    let repositories = collect_checkpoint_repositories(&workspace_root, &spec.repositories, git)?;
    let repositories_ms = repositories_started.elapsed().as_millis() as u64;
    // The export runs while the relay's barrier freezes ACP dispatch, so it
    // hashes the archive it just wrote instead of structurally re-reading it.
    // `CheckpointTransfer::execute` checks the controller's copy against this
    // digest; resume/import performs full structural verification when read.
    let archive_started = std::time::Instant::now();
    let sha256 = write_archive_hashed_borrowed(
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
    })
}

pub(super) fn collect_checkpoint_repositories(
    workspace_root: &Path,
    specifications: &[CheckpointRepositorySpec],
    git: &dyn GitCommandRunner,
) -> Result<Vec<RepositorySnapshot>> {
    // Indexed parallel iteration preserves the spec order, which in turn keeps
    // the manifest and ZIP entry order deterministic.
    specifications
        .par_iter()
        .map(|repository| {
            let path = workspace_root.join(&repository.relative_destination);
            ensure!(path.is_dir(), "repository {} is missing", path.display());
            let (history, remote_base) = match &repository.capture {
                CheckpointRepositoryCapture::MetadataOnly => {
                    return collect_git_metadata_snapshot(
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
            let mut snapshot = collect_git_snapshot(
                git,
                &path,
                &GitCollectionSpec {
                    id: repository.id.clone(),
                    relative_destination: repository.relative_destination.clone(),
                    history,
                    origin_override: repository.origin_override.clone(),
                },
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

/// Codex exports carry the relay-root scan cache so that a long-lived session
/// probes each unrelated rollout at most once.
pub(super) fn collect_checkpoint_native_artifacts(
    session: &SessionManifest,
    relay_root: &Path,
    harness_home: &Path,
    workspace_root: &Path,
    allow_empty: bool,
    native_state: &dyn NativeCheckpointState,
) -> Result<Vec<NativeArtifact>> {
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
    artifacts.extend(collect_subagent_reports(workspace_root)?);
    let launch_path = relay_root.join("launch.json");
    match read_project_memory_checkpoint_endpoint(&launch_path) {
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
    let total = artifacts.iter().try_fold(0_u64, |total, artifact| {
        total.checked_add(artifact.data.len() as u64)
    });
    anyhow::ensure!(
        total.context("native artifact size overflow")? <= MAX_NATIVE_TOTAL,
        "native session artifacts are too large"
    );
    Ok(artifacts)
}

/// Checkpoint exporters can be refreshed independently of the live worker, so
/// read only the stable launch fields needed for collection. Fully parsing the
/// current worker config would reject launch files written by older versions.
#[derive(Deserialize)]
pub(super) struct ProjectMemoryCheckpointLaunch {
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
