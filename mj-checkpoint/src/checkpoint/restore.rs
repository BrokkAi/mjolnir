use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointRestoreSpec {
    pub archive_path: PathBuf,
    pub workspace_root: PathBuf,
    pub relay_root: PathBuf,
    pub harness_home: PathBuf,
    pub restore_repositories: bool,
    pub restore_native: bool,
    pub discard_queued_prompts: bool,
    /// Where the primary repository actually sits, when that is not
    /// `workspace_root` joined with the archived destination. A resume that
    /// moves a session between representations puts the checkout somewhere the
    /// archive could not have named, and the restored harness session has to
    /// point at the real working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_repository_root: Option<PathBuf>,
}

pub fn restore_checkpoint(spec: &CheckpointRestoreSpec, git: &dyn GitCommandRunner) -> Result<()> {
    restore_checkpoint_with_native_state(spec, git, &NoNativeCheckpointState)
}

pub fn restore_checkpoint_with_native_state(
    spec: &CheckpointRestoreSpec,
    git: &dyn GitCommandRunner,
    native_state: &dyn NativeCheckpointState,
) -> Result<()> {
    ensure!(spec.workspace_root.is_dir(), "restore workspace is missing");
    // A restore seeds a relay that has no durable state yet. Existing state
    // means either a leaked worker still writing here or an unfinished
    // teardown, and the seed would be ignored in favour of the stale
    // snapshot, leaving a frontier no journal can support.
    let existing_relay_state = spec.relay_root.join(mj_core::relay::RELAY_STATE_FILE);
    ensure!(
        !existing_relay_state.exists(),
        "relay state already present in {}; a previous worker may still be running, \
         refusing to restore over it",
        spec.relay_root.display()
    );
    let archive = read_archive_verified(&spec.archive_path)?;
    // Deserialize and validate the schema-2 canonical projection before any
    // repository, relay, or native-session state can be mutated.
    let canonical_session = archive.canonical_session()?;
    // The archive carries no relay journal, so this is the restored worker's
    // only evidence about the native session it continues.
    let native_session_unused =
        spec.restore_native && !current_native_session_received_prompt(&canonical_session);
    // The relay that opens next needs the frontier it continues from and the
    // commands still queued, and nothing else. The transcript stays in the
    // archive; the controller already holds it as the durable projection.
    let mut seed = mj_core::relay::RestoredRelaySeed {
        event_frontier: canonical_session.event_frontier,
        event_frontier_digest: canonical_session.event_frontier_digest,
        queued_prompts: canonical_session.queued_prompts,
        // Selector values name one harness's catalogue, so they carry over
        // only when the same native conversation continues.
        accepted_config: if spec.restore_native {
            ["model", "effort"]
                .into_iter()
                .filter_map(|key| {
                    let value = canonical_session.session.configuration.get(key)?.as_str()?;
                    (!value.trim().is_empty()).then(|| (key.to_owned(), value.to_owned()))
                })
                .collect()
        } else {
            Default::default()
        },
        native_session_unused,
    };
    if spec.discard_queued_prompts {
        seed.queued_prompts.clear();
    }
    seed.validate()?;
    if spec.restore_repositories {
        restore_repositories_from_archive(&archive, &spec.workspace_root, git)?;
    }

    // Attachment data belongs to the relay, even when native harness history
    // is deliberately not restored. Install before publishing the queue seed.
    let image_store = mj_core::attachment::AttachmentStore::worker(&spec.relay_root);
    for descriptor in &archive.manifest.payloads {
        if let PayloadRole::NativeArtifact { relative_path } = &descriptor.role
            && relative_path.starts_with(mj_core::attachment::ARCHIVE_ATTACHMENT_DIR)
        {
            image_store.restore_artifact(relative_path, archive.payload(descriptor)?)?;
        }
    }
    restore_subagent_reports(&archive, &spec.workspace_root)?;
    for queued in &seed.queued_prompts {
        let blocks: Vec<agent_client_protocol::schema::v1::ContentBlock> = queued
            .content
            .iter()
            .cloned()
            .map(serde_json::from_value)
            .collect::<std::result::Result<_, _>>()?;
        for reference in mj_core::attachment::references(&blocks)? {
            image_store.read(&reference)?;
        }
    }
    fs::create_dir_all(&spec.relay_root)?;
    mj_core::relay::clear_native_session_identity(&spec.relay_root)?;
    write_private_file(
        &spec.relay_root,
        Path::new(mj_core::relay::RESTORED_RELAY_SEED_FILE),
        &serde_json::to_vec(&seed)?,
        0o600,
    )?;

    if spec.restore_native {
        let target_cwd = spec.primary_repository_root.clone().or_else(|| {
            target_primary_cwd(
                &archive.manifest.bundle.primary_repository,
                &archive.manifest.repositories,
                &spec.workspace_root,
            )
        });
        for descriptor in &archive.manifest.payloads {
            let PayloadRole::NativeArtifact { relative_path } = &descriptor.role else {
                continue;
            };
            if relative_path.starts_with(mj_core::attachment::ARCHIVE_ATTACHMENT_DIR)
                || relative_path.starts_with(mj_core::subagent::ARCHIVE_REPORT_DIR)
            {
                continue;
            }
            let native_data = archive.payload(descriptor)?;
            if native_state.restore(
                &archive.manifest.session,
                &spec.harness_home,
                relative_path,
                native_data,
            )? {
                continue;
            }
            ensure!(
                !relative_path.starts_with(mj_core::goal::NATIVE_ARTIFACT_ROOT),
                "this checkpoint requires native goal restoration by the target worker"
            );
            let relative_path = restored_native_relative_path(
                archive.manifest.session.harness_kind,
                relative_path,
                target_cwd.as_deref(),
            )?;
            let native_data = restored_native_artifact_bytes(
                archive.manifest.session.harness_kind,
                &relative_path,
                native_data,
                target_cwd.as_deref(),
                &spec.harness_home,
            )?;
            validate_relative_path(&relative_path)?;
            ensure!(
                !is_secret_like_path(&relative_path),
                "native artifact path is secret-like"
            );
            write_private_file(
                &spec.harness_home,
                &relative_path,
                &native_data,
                descriptor.mode,
            )?;
        }
    }
    Ok(())
}

/// Reads the controller-owned projection without restoring target artifacts.
pub fn read_checkpoint_session(path: &Path) -> Result<CanonicalSessionSnapshot> {
    Ok(verify_archive_streaming(path)?.canonical_session)
}

pub fn restore_repositories(
    archive_path: &Path,
    workspace_root: &Path,
    git: &dyn GitCommandRunner,
) -> Result<()> {
    ensure!(workspace_root.is_dir(), "restore workspace is missing");
    let archive = read_archive_verified(archive_path)?;
    restore_repositories_from_archive(&archive, workspace_root, git)
}

pub(super) fn restore_repositories_from_archive(
    archive: &crate::archive::VerifiedArchive,
    workspace_root: &Path,
    git: &dyn GitCommandRunner,
) -> Result<()> {
    for repository in &archive.manifest.repositories {
        let id = &repository.metadata.id;
        let snapshot = archived_repository_snapshot(archive, repository)?;
        let path = workspace_root.join(&repository.metadata.relative_destination);
        restore_git_snapshot(git, &path, &snapshot)
            .with_context(|| format!("restore repository {id:?}"))?;
    }
    Ok(())
}

pub(super) fn archived_repository_snapshot(
    archive: &crate::archive::VerifiedArchive,
    repository: &crate::archive::RepositoryManifest,
) -> Result<RepositorySnapshot> {
    let id = &repository.metadata.id;
    Ok(RepositorySnapshot {
        metadata: repository.metadata.clone(),
        committed_bundle: archive
            .payload_by_role(&PayloadRole::GitBundle {
                repository_id: id.clone(),
            })?
            .to_vec(),
        staged_patch: archive
            .payload_by_role(&PayloadRole::GitStagedPatch {
                repository_id: id.clone(),
            })?
            .to_vec(),
        unstaged_patch: archive
            .payload_by_role(&PayloadRole::GitUnstagedPatch {
                repository_id: id.clone(),
            })?
            .to_vec(),
        untracked_tar: archive
            .payload_by_role(&PayloadRole::GitUntrackedTar {
                repository_id: id.clone(),
            })?
            .to_vec(),
    })
}

/// Restore a checkpoint's only repository into an existing checkout, on a
/// branch the caller names.
///
/// A resume that moves a session out of its workspace restores it into a
/// worktree of the user's own repository, where the archived branch is usually
/// already checked out somewhere else and `git checkout -B` would refuse it.
/// Returns the branch the checkpoint recorded.
pub fn restore_single_repository_onto_branch(
    archive_path: &Path,
    repository_path: &Path,
    branch: &str,
    git: &dyn GitCommandRunner,
) -> Result<Option<String>> {
    ensure!(repository_path.is_dir(), "restore checkout is missing");
    let archive = read_archive_verified(archive_path)?;
    let [repository] = archive.manifest.repositories.as_slice() else {
        bail!(
            "this checkpoint holds {} repositories; exactly one can be restored into a checkout",
            archive.manifest.repositories.len()
        );
    };
    let mut snapshot = archived_repository_snapshot(&archive, repository)?;
    let archived_branch = snapshot.metadata.branch.replace(branch.to_owned());
    // This explicit host restore uses a linked worktree whose local Git
    // configuration is shared with the user's main checkout; keep managed
    // network configuration out of that shared config.
    snapshot.metadata.remote_workspace = false;
    restore_git_snapshot(git, repository_path, &snapshot)
        .with_context(|| format!("restore repository {:?}", repository.metadata.id))?;
    Ok(archived_branch)
}

/// Restore a retired independent clone on the branch saved in its archive.
pub fn restore_single_repository_into_checkout(
    archive_path: &Path,
    repository_path: &Path,
    git: &dyn GitCommandRunner,
) -> Result<()> {
    ensure!(repository_path.is_dir(), "restore checkout is missing");
    let archive = read_archive_verified(archive_path)?;
    let [repository] = archive.manifest.repositories.as_slice() else {
        bail!("an isolated checkout requires exactly one archived repository");
    };
    let mut snapshot = archived_repository_snapshot(&archive, repository)?;
    snapshot.metadata.remote_workspace = false;
    restore_git_snapshot(git, repository_path, &snapshot)
        .context("restore independent checkout before moving it into a target")
}

/// Native session files use harness-specific working-directory keys. Rewrite
pub(super) fn restored_native_relative_path(
    harness: HarnessKind,
    relative_path: &Path,
    target_cwd: Option<&Path>,
) -> Result<PathBuf> {
    let Some(target_cwd) = target_cwd else {
        return Ok(relative_path.to_path_buf());
    };
    let mut components = relative_path.components();
    match harness {
        HarnessKind::Claude => {
            if components.next() != Some(Component::Normal("projects".as_ref()))
                || components.next().is_none()
            {
                return Ok(relative_path.to_path_buf());
            }
            let mut rewritten = PathBuf::from("projects");
            rewritten.push(claude_project_slug(target_cwd));
            rewritten.extend(components);
            Ok(rewritten)
        }
        HarnessKind::Kimi => {
            if components.next() != Some(Component::Normal("sessions".as_ref()))
                || components.next().is_none()
            {
                return Ok(relative_path.to_path_buf());
            }
            let mut rewritten = PathBuf::from("sessions");
            rewritten.push(kimi_workspace_key(target_cwd));
            rewritten.extend(components);
            Ok(rewritten)
        }
        HarnessKind::Grok => {
            if components.next() != Some(Component::Normal("sessions".as_ref()))
                || components.next().is_none()
            {
                return Ok(relative_path.to_path_buf());
            }
            let mut rewritten = PathBuf::from("sessions");
            rewritten.push(grok_cwd_key(target_cwd));
            rewritten.extend(components);
            Ok(rewritten)
        }
        HarnessKind::Codex | HarnessKind::Muse => Ok(relative_path.to_path_buf()),
    }
}

pub(super) fn target_primary_cwd(
    primary_repository: &str,
    repositories: &[crate::archive::RepositoryManifest],
    workspace_root: &Path,
) -> Option<PathBuf> {
    repositories
        .iter()
        .find(|repository| repository.metadata.id == primary_repository)
        .map(|primary| workspace_root.join(&primary.metadata.relative_destination))
}

pub(super) fn restored_native_artifact_bytes(
    harness: HarnessKind,
    relative_path: &Path,
    data: &[u8],
    target_cwd: Option<&Path>,
    harness_home: &Path,
) -> Result<Vec<u8>> {
    let Some(target_cwd) = target_cwd else {
        return Ok(data.to_vec());
    };
    if harness == HarnessKind::Muse
        && relative_path
            .file_name()
            .is_some_and(|name| name == "session.jsonl")
    {
        return crate::native::muse::relocate(data, target_cwd);
    }
    if !matches!(harness, HarnessKind::Kimi | HarnessKind::Grok) {
        return Ok(data.to_vec());
    }
    if harness == HarnessKind::Grok {
        return if is_grok_session_summary(relative_path) {
            rewrite_grok_session_summary(data, target_cwd, harness_home)
        } else {
            Ok(data.to_vec())
        };
    }
    if relative_path == Path::new("workspaces.json") {
        return rewrite_kimi_workspace_registry(data, target_cwd);
    }
    if relative_path == Path::new("session_index.jsonl") {
        return rewrite_kimi_session_index(data, target_cwd, harness_home);
    }
    if !is_kimi_session_state(relative_path) {
        return Ok(data.to_vec());
    }
    let mut state: Value =
        serde_json::from_slice(data).context("parse Kimi native session state")?;
    let object = state
        .as_object_mut()
        .context("Kimi native session state is not a JSON object")?;
    for key in ["workDir", "cwd"] {
        if object.contains_key(key) {
            object.insert(
                key.into(),
                Value::String(target_cwd.to_string_lossy().into_owned()),
            );
        }
    }
    Ok(serde_json::to_vec(&state)?)
}

/// Write `relative` under `root` with owner-only permissions, refusing to
/// follow a symlink anywhere along the way.
///
/// A restore lands in a directory the target user can write, so a planted
/// symlink must never redirect the write outside `root`. `Path::exists`
/// follows links and reports a dangling symlink as missing, so the destination
/// is inspected with `symlink_metadata` and, on Unix, opened with `O_NOFOLLOW`
/// so the check cannot be raced.
/// Put a checkpoint's sub-agent report files back beside the repositories,
/// whether or not the harness's own history is restored: the parent reads
/// them with its own tools. A file already there is newer than the archive's
/// copy and is kept.
fn restore_subagent_reports(
    archive: &crate::archive::VerifiedArchive,
    workspace_root: &Path,
) -> Result<()> {
    for descriptor in &archive.manifest.payloads {
        let PayloadRole::NativeArtifact { relative_path } = &descriptor.role else {
            continue;
        };
        let Ok(report) = relative_path.strip_prefix(mj_core::subagent::ARCHIVE_REPORT_DIR) else {
            continue;
        };
        let relative = Path::new(mj_core::subagent::REPORT_ROOT_DIR).join(report);
        validate_relative_path(&relative)?;
        if fs::symlink_metadata(workspace_root.join(&relative)).is_ok() {
            continue;
        }
        write_private_file(
            workspace_root,
            &relative,
            archive.payload(descriptor)?,
            0o600,
        )?;
    }
    Ok(())
}

pub(super) fn write_private_file(
    root: &Path,
    relative: &Path,
    bytes: &[u8],
    mode: u32,
) -> Result<()> {
    validate_relative_path(relative)?;
    let path = root.join(relative);
    ensure_no_symlink_ancestors(root, relative)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create directory {}", parent.display()))?;
        // `create_dir_all` walks through symlinked directories, so re-inspect
        // the ancestors it just materialized.
        ensure_no_symlink_ancestors(root, relative)?;
    }
    match fs::symlink_metadata(&path) {
        Ok(metadata) => ensure!(
            !metadata.file_type().is_symlink(),
            "refusing to write through symlink {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    }
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode & 0o700).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("write {}", path.display()))?;
    std::io::Write::write_all(&mut file, bytes)
        .with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(mode & 0o700))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(())
}
