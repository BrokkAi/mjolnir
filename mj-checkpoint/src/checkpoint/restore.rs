use super::*;

/// Whether the destination worker owns dispatch immediately, drops the queue,
/// or waits for its controller to admit the archived commands after readiness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueRestorePolicy {
    Restore,
    Discard,
    Defer,
}

/// The relay metadata needed to continue a verified canonical session. An
/// in-place native Move already has its history and attachments on the target.
pub fn relay_seed_for_restore(
    canonical: &CanonicalSessionSnapshot,
    native_continuity: bool,
    queue_policy: QueueRestorePolicy,
) -> Result<mj_core::relay::RestoredRelaySeed> {
    let seed = mj_core::relay::RestoredRelaySeed {
        command_ledger: None,
        assessment_state: native_continuity
            .then(|| canonical.assessment_state.clone())
            .flatten(),
        event_frontier: canonical.event_frontier,
        event_frontier_digest: canonical.event_frontier_digest.clone(),
        queued_prompts: if queue_policy == QueueRestorePolicy::Restore {
            canonical.queued_prompts.clone()
        } else {
            Vec::new()
        },
        accepted_config: if native_continuity {
            mj_core::acp::AcceptedSessionConfig::from_archived_values(
                &canonical.session.configuration.values,
            )
        } else {
            Default::default()
        },
        native_session_unused: native_continuity
            && !current_native_session_received_prompt(canonical),
    };
    seed.validate()?;
    Ok(seed)
}

fn deserialize_queue_policy<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<QueueRestorePolicy, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Wire {
        Legacy(bool),
        Policy(QueueRestorePolicy),
    }
    Ok(match Wire::deserialize(deserializer)? {
        Wire::Legacy(true) => QueueRestorePolicy::Discard,
        Wire::Legacy(false) => QueueRestorePolicy::Restore,
        Wire::Policy(policy) => policy,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointRestoreSpec {
    pub archive_path: PathBuf,
    pub workspace_root: PathBuf,
    /// Target-side root for report files; older controllers restore beside repositories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_root: Option<PathBuf>,
    pub relay_root: PathBuf,
    pub harness_home: PathBuf,
    pub restore_repositories: bool,
    pub restore_native: bool,
    #[serde(
        alias = "discard_queued_prompts",
        deserialize_with = "deserialize_queue_policy"
    )]
    pub queue_policy: QueueRestorePolicy,
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
    // The relay that opens next needs the frontier it continues from and the
    // commands still queued, and nothing else. The transcript stays in the
    // archive; the controller already holds it as the durable projection.
    let seed = relay_seed_for_restore(&canonical_session, spec.restore_native, spec.queue_policy)?;
    if spec.restore_repositories {
        restore_repositories_from_archive(&archive, &spec.workspace_root, git, true)?;
    } else {
        // Move has already transported workspace files. Review refs still
        // need their destination checkout names without changing its index.
        restore_repositories_from_archive(&archive, &spec.workspace_root, git, false)?;
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
    let report_root = match &spec.report_root {
        Some(root) => resolve_target_path(root)?,
        None => spec.workspace_root.join(mj_core::subagent::REPORT_ROOT_DIR),
    };
    restore_subagent_reports(&archive, &spec.workspace_root, &report_root)?;
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
        // A restored OpenCode database replaces the one in the home, and
        // SQLite would replay a write-ahead log left beside the old one over
        // it. An archive from before snapshots carries its own log, which the
        // loop below writes after this.
        let opencode_database = Path::new(super::native_scan::OPENCODE_DATABASE);
        if archive.manifest.session.harness_kind == HarnessKind::OpenCode
            && archive.manifest.payloads.iter().any(|descriptor| {
                matches!(&descriptor.role, PayloadRole::NativeArtifact { relative_path }
                    if relative_path == opencode_database)
            })
        {
            for sidecar in super::native_scan::OPENCODE_DATABASE_SIDECARS {
                remove_private_file(&spec.harness_home, Path::new(sidecar))?;
            }
        }
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
    restore_repositories_from_archive(&archive, workspace_root, git, true)
}

pub(super) fn restore_repositories_from_archive(
    archive: &crate::archive::VerifiedArchive,
    workspace_root: &Path,
    git: &dyn GitCommandRunner,
    restore_contents: bool,
) -> Result<()> {
    for repository in &archive.manifest.repositories {
        // A plain project folder is the user's own directory on a bare local
        // target. Its files were never captured, so a restore must never
        // check out, rewrite or remove anything in it.
        if repository.metadata.plain_directory {
            continue;
        }
        if !restore_contents
            && !repository
                .metadata
                .saved_refs
                .keys()
                .any(|name| name.starts_with(mj_core::archive::MJOLNIR_REF_NAMESPACE))
        {
            continue;
        }
        let id = &repository.metadata.id;
        let snapshot = archived_repository_snapshot(archive, repository)?;
        let path = checkout_top_level(
            &workspace_root.join(&repository.metadata.relative_destination),
            &repository.metadata,
        )
        .with_context(|| format!("restore repository {id:?}"))?;
        crate::archive::restore_git_snapshot_for_session(
            git,
            &path,
            &snapshot,
            Some(&archive.manifest.session.id),
            restore_contents,
        )
        .with_context(|| format!("restore repository {id:?}"))?;
    }
    Ok(())
}

/// The top level of the checkout a repository's session directory belongs to.
///
/// A session working in a subdirectory of its checkout is described by that
/// subdirectory, while its snapshot covers the whole checkout, so the restore
/// climbs back out of the subdirectory the snapshot recorded.
fn checkout_top_level(
    directory: &Path,
    metadata: &crate::archive::RepositoryMetadata,
) -> Result<PathBuf> {
    let Some(subdirectory) = &metadata.checkout_subdirectory else {
        return Ok(directory.to_path_buf());
    };
    ensure!(
        directory.ends_with(subdirectory),
        "{} is not the checkout subdirectory {} the checkpoint was taken from",
        directory.display(),
        subdirectory.display()
    );
    directory
        .ancestors()
        .nth(subdirectory.components().count())
        .map(Path::to_path_buf)
        .with_context(|| format!("{} has no checkout above it", directory.display()))
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
        operation_payloads: match repository.metadata.git_operation {
            None => None,
            Some(_) => Some(crate::archive::GitOperationPayloads {
                conflicted_worktree_tar: archive
                    .payload_by_role(&PayloadRole::GitConflictedWorktreeTar {
                        repository_id: id.clone(),
                    })?
                    .to_vec(),
                state_tar: archive
                    .payload_by_role(&PayloadRole::GitOperationStateTar {
                        repository_id: id.clone(),
                    })?
                    .to_vec(),
            }),
        },
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
    ensure!(
        !repository.metadata.plain_directory,
        "this session's project is a directory without a committed Git checkout, so it cannot be restored into one"
    );
    let mut snapshot = archived_repository_snapshot(&archive, repository)?;
    let archived_branch = snapshot.metadata.branch.replace(branch.to_owned());
    // This explicit host restore uses a linked worktree whose local Git
    // configuration is shared with the user's main checkout; keep managed
    // network configuration out of that shared config.
    snapshot.metadata.remote_workspace = false;
    crate::archive::restore_git_snapshot_for_session(
        git,
        repository_path,
        &snapshot,
        Some(&archive.manifest.session.id),
        true,
    )
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
    ensure!(
        !repository.metadata.plain_directory,
        "this session's project is a directory without a committed Git checkout, so it cannot be restored into one"
    );
    let mut snapshot = archived_repository_snapshot(&archive, repository)?;
    snapshot.metadata.remote_workspace = false;
    crate::archive::restore_git_snapshot_for_session(
        git,
        repository_path,
        &snapshot,
        Some(&archive.manifest.session.id),
        true,
    )
    .context("restore independent checkout before moving it into a target")
}

/// Native session files use harness-specific working-directory keys. Rewrite
/// the archived key to the one the harness derives from `target_cwd`, the
/// directory the worker launches it in.
pub(super) fn restored_native_relative_path(
    harness: HarnessKind,
    relative_path: &Path,
    target_cwd: Option<&Path>,
) -> Result<PathBuf> {
    let Some(target_cwd) = target_cwd else {
        return Ok(relative_path.to_path_buf());
    };
    let target_cwd = &harness_storage_cwd(harness, target_cwd);
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
        // OpenCode's database records session directories itself, and
        // dsh-acp records its directory inside the session file; neither has
        // a per-workspace path key to rewrite.
        HarnessKind::Codex
        | HarnessKind::Muse
        | HarnessKind::OpenCode
        | HarnessKind::Mimo
        | HarnessKind::Dsh => Ok(relative_path.to_path_buf()),
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
    let target_cwd = &harness_storage_cwd(harness, target_cwd);
    if harness == HarnessKind::Muse
        && relative_path
            .file_name()
            .is_some_and(|name| name == "session.jsonl")
    {
        return crate::native::muse::relocate(data, target_cwd);
    }
    if harness == HarnessKind::Dsh && relative_path.starts_with("sessions") {
        return rewrite_dsh_session_cwd(data, target_cwd);
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

/// A dsh-acp session record names the directory it was created in, and
/// `session/load` refuses any other, so a restore at a new workspace path
/// records the target's. Additional directories need no rewrite: every load
/// replaces them with the ones it is given.
fn rewrite_dsh_session_cwd(data: &[u8], target_cwd: &Path) -> Result<Vec<u8>> {
    let mut record: Value = serde_json::from_slice(data).context("parse dsh-acp session record")?;
    let object = record
        .as_object_mut()
        .context("dsh-acp session record is not a JSON object")?;
    ensure!(
        object.contains_key("cwd"),
        "dsh-acp session record names no cwd"
    );
    object.insert(
        "cwd".into(),
        Value::String(
            target_cwd
                .to_str()
                .context("dsh-acp target cwd is not valid UTF-8")?
                .to_owned(),
        ),
    );
    Ok(serde_json::to_vec(&record)?)
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
    report_root: &Path,
) -> Result<()> {
    let root = report_root
        .strip_prefix(workspace_root)
        .context("report restore root is outside the workspace")?;
    for descriptor in &archive.manifest.payloads {
        let PayloadRole::NativeArtifact { relative_path } = &descriptor.role else {
            continue;
        };
        let Ok(report) = relative_path.strip_prefix(mj_core::subagent::ARCHIVE_REPORT_DIR) else {
            continue;
        };
        let relative = root.join(report);
        validate_relative_path(&relative)?;
        ensure_no_symlink_ancestors(workspace_root, &relative)?;
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

/// Remove `relative` from `root` if it is there, under the same rules as
/// [`write_private_file`]: never through a symlinked directory.
fn remove_private_file(root: &Path, relative: &Path) -> Result<()> {
    validate_relative_path(relative)?;
    ensure_no_symlink_ancestors(root, relative)?;
    let path = root.join(relative);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
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
