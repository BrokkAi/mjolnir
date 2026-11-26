use super::*;

/// Who dispatches the archived queue and the archived worker's pending mail.
///
/// Pending mail joins the same dispatch decision as the queue: a restored
/// worker given a waking event starts a turn on its own, so mail the
/// controller must hold back (behind a Move's ownership commit, or behind a
/// fresh native session's handoff) is admitted by the controller with the
/// queue. Mail already claimed by a queued prompt travels with that prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueRestorePolicy {
    /// The destination worker owns its queue and pending mail at once.
    Restore,
    /// Drop the queue and the mail its prompts claimed; the destination worker
    /// owns pending mail at once.
    Discard,
    /// The controller admits the queue and pending mail after readiness. The
    /// mail a queued prompt claimed is bound to its command ID now, so it is
    /// there before that prompt can start.
    Defer,
    /// Drop the queue and the mail its prompts claimed; the controller admits
    /// pending mail after readiness.
    DiscardDeferringMail,
}

impl QueueRestorePolicy {
    /// Whether the controller, not the restored worker, admits pending mail.
    pub fn defers_mail(self) -> bool {
        matches!(self, Self::Defer | Self::DiscardDeferringMail)
    }

    pub fn keeps_queue(self) -> bool {
        matches!(self, Self::Restore | Self::Defer)
    }
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
        notification_stack: native_continuity
            .then(|| canonical.notification_stack.clone())
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
            && current_native_session_is_unused(canonical, None),
        mailbox: canonical.mailbox.as_ref().and_then(|mailbox| {
            let mailbox = mj_core::archive::CanonicalMailbox {
                pending_events: if queue_policy.defers_mail() {
                    Vec::new()
                } else {
                    mailbox.pending_events.clone()
                },
                delivered_event_keys: mailbox.delivered_event_keys.clone(),
                quota_input: mj_core::archive::CanonicalQuotaInput {
                    pending: {
                        let mut retained = Vec::new();
                        if !queue_policy.keeps_queue() {
                            for queued in &canonical.queued_prompts {
                                if let Some(input) =
                                    mailbox.quota_input.claimed.get(&queued.command_id)
                                {
                                    retained.extend(input.iter().cloned());
                                }
                                if let Some(input) =
                                    mailbox.quota_input.steered.get(&queued.command_id)
                                {
                                    retained.extend(input.iter().cloned());
                                }
                            }
                        }
                        retained.extend(mailbox.quota_input.pending.iter().cloned());
                        retained
                    },
                    steered: if queue_policy.keeps_queue() {
                        mailbox.quota_input.steered.clone()
                    } else {
                        Default::default()
                    },
                    frozen_prompt_contexts: if queue_policy.keeps_queue() {
                        mailbox.quota_input.frozen_prompt_contexts.clone()
                    } else {
                        Default::default()
                    },
                    claimed: if queue_policy.keeps_queue() {
                        mailbox.quota_input.claimed.clone()
                    } else {
                        Default::default()
                    },
                },
                prompt_contexts: if queue_policy.keeps_queue() {
                    mailbox.prompt_contexts.clone()
                } else {
                    Default::default()
                },
            };
            (!mailbox.is_empty()).then_some(mailbox)
        }),
        native_use_evidence: native_continuity
            .then(|| canonical.native_use_evidence.clone())
            .flatten(),
        pending_prompt_context: canonical.pending_prompt_context.clone(),
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
    restore_checkpoint_with_notices(spec, git, native_state).map(drop)
}

pub fn restore_checkpoint_with_notices(
    spec: &CheckpointRestoreSpec,
    git: &dyn GitCommandRunner,
    native_state: &dyn NativeCheckpointState,
) -> Result<Vec<String>> {
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
    let archive = crate::archive::read_archive_for_restore(&spec.archive_path)?;
    let mut payload_reader = crate::archive::CheckpointPayloadReader::open(&spec.archive_path)?;
    // Deserialize and validate the schema-2 canonical projection before any
    // repository, relay, or native-session state can be mutated.
    let canonical_session = archive.canonical_session()?;
    // The relay that opens next needs the frontier it continues from and the
    // commands still queued, and nothing else. The transcript stays in the
    // archive; the controller already holds it as the durable projection.
    let seed = relay_seed_for_restore(&canonical_session, spec.restore_native, spec.queue_policy)?;
    let notices = if spec.restore_repositories {
        restore_repositories_from_archive(
            &spec.archive_path,
            &archive,
            &spec.workspace_root,
            spec.primary_repository_root.as_deref(),
            git,
            true,
        )?
    } else {
        // Move has already transported workspace files. Review refs still
        // need their destination checkout names without changing its index.
        restore_repositories_from_archive(
            &spec.archive_path,
            &archive,
            &spec.workspace_root,
            spec.primary_repository_root.as_deref(),
            git,
            false,
        )?
    };

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
        let blocks: Vec<agent_client_protocol::v1::schema::ContentBlock> = queued
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
        restore_native_artifacts(&archive, &mut payload_reader, spec, native_state, false)?;
    }
    Ok(notices)
}

/// Retryable reconstruction in a private staging checkout. Publishing must
/// atomically refuse any directory that appeared at the current checkout path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckoutRecoverySpec {
    pub version: u32,
    pub checkpoint_sha256: String,
    pub restore: CheckpointRestoreSpec,
    pub staging_root: PathBuf,
    pub staged_checkout: PathBuf,
    pub destination_checkout: PathBuf,
    #[serde(default)]
    pub additional_checkouts: Vec<CheckoutRecoveryPublication>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckoutRecoveryPublication {
    pub staged_checkout: PathBuf,
    pub destination_checkout: PathBuf,
}

/// Target-side partial recovery never writes checkout files, refs or child reports.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ParentRecoveryOutcome {
    pub native_restored: bool,
    pub relay_restored: bool,
}

pub fn restore_missing_parent_checkpoint(
    spec: &CheckpointRestoreSpec,
    native_state: &dyn NativeCheckpointState,
    current_seed: Option<&mj_core::relay::RestoredRelaySeed>,
) -> Result<ParentRecoveryOutcome> {
    ensure!(
        !spec.restore_repositories,
        "parent recovery cannot restore repositories"
    );
    ensure!(spec.workspace_root.is_dir(), "current workspace is missing");
    // Runtime reconstruction needs no archive when native history survives
    // and the controller supplies the current durable queue/frontier.
    if !spec.restore_native
        && let Some(seed) = current_seed
    {
        return Ok(ParentRecoveryOutcome {
            native_restored: false,
            relay_restored: restore_missing_parent_relay(&spec.relay_root, seed)?,
        });
    }
    let archive = crate::archive::read_archive_for_restore(&spec.archive_path)?;
    let mut payload_reader = crate::archive::CheckpointPayloadReader::open(&spec.archive_path)?;
    let canonical = archive.canonical_session()?;
    let mut outcome = ParentRecoveryOutcome::default();
    if spec.restore_native {
        outcome.native_restored =
            restore_native_artifacts(&archive, &mut payload_reader, spec, native_state, true)?;
    }
    // A current journal always wins over the archive.
    if parent_relay_missing(&spec.relay_root)? {
        let archived_seed;
        let seed = if let Some(seed) = current_seed {
            seed
        } else {
            archived_seed = relay_seed_for_restore(&canonical, true, spec.queue_policy)?;
            &archived_seed
        };
        let images = mj_core::attachment::AttachmentStore::worker(&spec.relay_root);
        for descriptor in &archive.manifest.payloads {
            if let PayloadRole::NativeArtifact { relative_path } = &descriptor.role
                && relative_path.starts_with(mj_core::attachment::ARCHIVE_ATTACHMENT_DIR)
            {
                images.restore_artifact(relative_path, archive.payload(descriptor)?)?;
            }
        }
        outcome.relay_restored = restore_missing_parent_relay(&spec.relay_root, seed)?;
    }
    Ok(outcome)
}

fn parent_relay_missing(root: &Path) -> Result<bool> {
    Ok(!root.join(mj_core::relay::RELAY_STATE_FILE).try_exists()?
        && !root.join(mj_core::relay::RELAY_JOURNAL_DIR).try_exists()?)
}

fn restore_missing_parent_relay(
    root: &Path,
    seed: &mj_core::relay::RestoredRelaySeed,
) -> Result<bool> {
    if !parent_relay_missing(root)? {
        return Ok(false);
    }
    let relative = Path::new(mj_core::relay::RESTORED_RELAY_SEED_FILE);
    ensure_no_symlink_ancestors(root, relative)?;
    fs::create_dir_all(root)?;
    let destination = root.join(relative);
    match fs::symlink_metadata(&destination) {
        Ok(metadata) => ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "invalid parent relay seed destination"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    // A seed is bootstrap input, not the durable journal. Once that journal
    // is lost, only the chosen current projection can authorize its queue.
    let mut temporary = tempfile::NamedTempFile::new_in(root)?;
    std::io::Write::write_all(&mut temporary, &serde_json::to_vec(seed)?)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(&destination)
        .map_err(|error| error.error)?;
    #[cfg(unix)]
    fs::File::open(root)?.sync_all()?;
    Ok(true)
}

/// Collapse a legacy database/WAL family before publishing any authoritative
/// file. A retry can then treat a present database as complete, with no receipt
/// or partially published sidecars to reconcile.
fn restore_sqlite_families(
    archive: &crate::archive::VerifiedArchive,
    payload_reader: &mut crate::archive::CheckpointPayloadReader,
    spec: &CheckpointRestoreSpec,
    target_cwd: Option<&Path>,
    missing_only: bool,
) -> Result<(bool, std::collections::BTreeSet<PathBuf>)> {
    let mut changed = false;
    let mut families = std::collections::BTreeSet::new();
    let harness = archive.manifest.session.harness_kind;
    for database in &archive.manifest.payloads {
        let PayloadRole::NativeArtifact { relative_path } = &database.role else {
            continue;
        };
        if !super::native_scan::is_sqlite_native_database(harness, relative_path) {
            continue;
        }
        let sidecars = super::native_scan::sqlite_sidecars(relative_path);
        let archived_sidecars =
            archive
                .manifest
                .payloads
                .iter()
                .filter_map(|payload| match &payload.role {
                    PayloadRole::NativeArtifact { relative_path }
                        if sidecars.contains(relative_path) =>
                    {
                        Some((relative_path, payload))
                    }
                    _ => None,
                });
        let mut archived_sidecars = archived_sidecars.peekable();
        if !missing_only && archived_sidecars.peek().is_none() {
            continue;
        }

        families.insert(relative_path.clone());
        families.extend(sidecars.iter().cloned());
        if missing_only {
            if spec.harness_home.join(relative_path).try_exists()? {
                continue;
            }
            for sidecar in &sidecars {
                ensure!(
                    !spec.harness_home.join(sidecar).try_exists()?,
                    "native database is absent but its sidecar remains; retaining both for recovery"
                );
            }
        }
        let scratch = tempfile::tempdir()?;
        write_private_with(scratch.path(), relative_path, database.mode, |file| {
            payload_reader.copy_payload(database, file)
        })?;
        for (sidecar, payload) in archived_sidecars {
            write_private_with(scratch.path(), sidecar, payload.mode, |file| {
                payload_reader.copy_payload(payload, file)
            })?;
        }
        // The collapsed copy stays a file; only a relocating provider needs
        // its bytes in memory to rewrite them.
        let snapshot = super::native_scan::sqlite_snapshot_file(
            &scratch.path().join(relative_path),
            "recovered native",
            |_| Ok(()),
        )?;
        if target_cwd.is_some() && native_artifact_relocation(harness, relative_path).is_some() {
            let complete = restored_native_artifact_bytes(
                harness,
                &archive.manifest.session.native_session_id,
                relative_path,
                &fs::read(snapshot.path())?,
                target_cwd,
                &spec.harness_home,
            )?;
            if missing_only {
                changed |= write_missing_private_file(
                    &spec.harness_home,
                    relative_path,
                    &complete,
                    database.mode,
                )?;
            } else {
                write_private_file(&spec.harness_home, relative_path, &complete, database.mode)?;
                changed = true;
            }
        } else {
            let copy_snapshot = |file: &mut File| {
                std::io::copy(&mut File::open(snapshot.path())?, file)?;
                Ok(())
            };
            if missing_only {
                changed |= write_missing_private_with(
                    &spec.harness_home,
                    relative_path,
                    database.mode,
                    copy_snapshot,
                )?;
            } else {
                write_private_with(
                    &spec.harness_home,
                    relative_path,
                    database.mode,
                    copy_snapshot,
                )?;
                changed = true;
            }
        }
    }
    Ok((changed, families))
}

fn restore_native_artifacts(
    archive: &crate::archive::VerifiedArchive,
    payload_reader: &mut crate::archive::CheckpointPayloadReader,
    spec: &CheckpointRestoreSpec,
    native_state: &dyn NativeCheckpointState,
    missing_only: bool,
) -> Result<bool> {
    let target_cwd = spec.primary_repository_root.clone().or_else(|| {
        target_primary_cwd(
            &archive.manifest.bundle.primary_repository,
            &archive.manifest.repositories,
            &spec.workspace_root,
        )
    });
    // Remove stale sidecars before replacing any database. Archived sidecars
    // are replayed in scratch before the restored database is published.
    if !missing_only {
        for descriptor in &archive.manifest.payloads {
            if let PayloadRole::NativeArtifact { relative_path } = &descriptor.role
                && super::native_scan::is_sqlite_native_database(
                    archive.manifest.session.harness_kind,
                    relative_path,
                )
            {
                for sidecar in super::native_scan::sqlite_sidecars(relative_path) {
                    remove_private_file(&spec.harness_home, &sidecar)?;
                }
            }
        }
    }
    let (mut changed, sqlite_families) = restore_sqlite_families(
        archive,
        payload_reader,
        spec,
        target_cwd.as_deref(),
        missing_only,
    )?;
    for descriptor in &archive.manifest.payloads {
        let PayloadRole::NativeArtifact { relative_path } = &descriptor.role else {
            continue;
        };
        if relative_path.starts_with(mj_core::attachment::ARCHIVE_ATTACHMENT_DIR)
            || relative_path.starts_with(mj_core::subagent::ARCHIVE_REPORT_DIR)
        {
            continue;
        }
        if sqlite_families.contains(relative_path) {
            continue;
        }
        if !relative_path.starts_with(mj_core::goal::NATIVE_ARTIFACT_ROOT) {
            let destination = restored_native_relative_path(
                archive.manifest.session.harness_kind,
                relative_path,
                target_cwd.as_deref(),
            )?;
            // Claude and Codex transcripts need only path remapping.
            if target_cwd.is_none()
                || native_artifact_relocation(archive.manifest.session.harness_kind, &destination)
                    .is_none()
            {
                if missing_only {
                    changed |= write_missing_private_with(
                        &spec.harness_home,
                        &destination,
                        descriptor.mode,
                        |file| payload_reader.copy_payload(descriptor, file),
                    )?;
                } else {
                    write_private_with(
                        &spec.harness_home,
                        &destination,
                        descriptor.mode,
                        |file| payload_reader.copy_payload(descriptor, file),
                    )?;
                }
                continue;
            }
        }
        let mut loaded = Vec::new();
        let native_data = if let Some(bytes) = archive.payloads.get(&descriptor.path) {
            bytes.as_slice()
        } else {
            payload_reader.copy_payload(descriptor, &mut loaded)?;
            &loaded
        };
        if missing_only {
            if let Some(written) = native_state.restore_missing(
                &archive.manifest.session,
                &spec.harness_home,
                relative_path,
                native_data,
            )? {
                changed |= written;
                continue;
            }
        } else if native_state.restore(
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
            &archive.manifest.session.native_session_id,
            &relative_path,
            native_data,
            target_cwd.as_deref(),
            &spec.harness_home,
        )?;
        validate_relative_path(&relative_path)?;
        if missing_only {
            changed |= write_missing_private_file(
                &spec.harness_home,
                &relative_path,
                &native_data,
                descriptor.mode,
            )?;
        } else {
            write_private_file(
                &spec.harness_home,
                &relative_path,
                &native_data,
                descriptor.mode,
            )?;
        }
    }
    Ok(changed)
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
    let archive = crate::archive::read_archive_repositories(archive_path)?;
    restore_repositories_from_archive(archive_path, &archive, workspace_root, None, git, true)
        .map(drop)
}

pub(super) fn restore_repositories_from_archive(
    archive_path: &Path,
    archive: &crate::archive::VerifiedArchive,
    workspace_root: &Path,
    primary_repository_root: Option<&Path>,
    git: &dyn GitCommandRunner,
    restore_contents: bool,
) -> Result<Vec<String>> {
    let mut all_notices = Vec::new();
    for repository in &archive.manifest.repositories {
        // A plain project folder is the user's own directory on a bare local
        // target. Its files were never captured, so a restore must never
        // check out, rewrite or remove anything in it.
        if repository.metadata.origin_delta.is_some() {
            if !restore_contents {
                continue;
            }
            let id = &repository.metadata.id;
            let spooled = spool_origin_delta_snapshot(archive_path, &archive.manifest, repository)?
                .context("origin delta metadata is missing")?;
            let snapshot = spooled.snapshot();
            let path = if repository.metadata.id == archive.manifest.bundle.primary_repository
                && let Some(primary) = primary_repository_root
            {
                if let Some(subdirectory) = &repository.metadata.checkout_subdirectory
                    && primary.ends_with(subdirectory)
                {
                    checkout_top_level(primary, &repository.metadata)
                        .with_context(|| format!("restore repository {id:?}"))?
                } else {
                    primary.to_path_buf()
                }
            } else {
                checkout_top_level(
                    &workspace_root.join(&repository.metadata.relative_destination),
                    &repository.metadata,
                )
                .with_context(|| format!("restore repository {id:?}"))?
            };
            let notices = crate::snapshot::restore_workspace_snapshot(git, snapshot, &path)
                .with_context(|| format!("restore repository {id:?}"))?;
            for notice in &notices {
                tracing::info!(repository_id = %id, "{notice}");
            }
            all_notices.extend(notices);
            crate::snapshot::repair_workspace_snapshot(git, snapshot, &path)
                .with_context(|| format!("repair repository {id:?}"))?;
            restore_managed_base(git, &path, &repository.metadata)
                .with_context(|| format!("restore the launch base of repository {id:?}"))?;
            continue;
        }
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
        let notices = crate::archive::restore_git_snapshot_for_session(
            git,
            &path,
            &snapshot,
            Some(&archive.manifest.session.id),
            restore_contents,
        )
        .with_context(|| format!("restore repository {id:?}"))?;
        for notice in &notices {
            tracing::info!(repository_id = %id, "{notice}");
        }
        all_notices.extend(notices);
    }
    Ok(all_notices)
}

/// An origin-delta snapshot whose pack is spooled to a temporary file. The
/// file goes away with this value, so keep it until the snapshot is applied.
pub struct SpooledWorkspaceSnapshot {
    snapshot: crate::snapshot::WorkspaceSnapshot,
    _pack: Option<tempfile::NamedTempFile>,
}

impl SpooledWorkspaceSnapshot {
    pub fn snapshot(&self) -> &crate::snapshot::WorkspaceSnapshot {
        &self.snapshot
    }
}

/// Spool one repository's origin-delta pack out of the archive and pair it
/// with the snapshot metadata. `None` when the repository is not an origin
/// delta. Resume and export both restore through this one spooling path.
pub fn spool_origin_delta_snapshot(
    archive_path: &Path,
    manifest: &crate::archive::ArchiveManifest,
    repository: &crate::archive::RepositoryManifest,
) -> Result<Option<SpooledWorkspaceSnapshot>> {
    spool_origin_delta_snapshot_unless(archive_path, manifest, repository, &|| false)
}

/// [`spool_origin_delta_snapshot`] that stops copying the pack with
/// [`crate::archive::ArchiveReadCancelled`] once `cancelled` returns true.
pub fn spool_origin_delta_snapshot_unless(
    archive_path: &Path,
    manifest: &crate::archive::ArchiveManifest,
    repository: &crate::archive::RepositoryManifest,
    cancelled: &dyn Fn() -> bool,
) -> Result<Option<SpooledWorkspaceSnapshot>> {
    let Some(delta) = repository.metadata.origin_delta.as_deref() else {
        return Ok(None);
    };
    let id = &repository.metadata.id;
    let pack = repository
        .origin_delta_pack_path
        .as_ref()
        .map(|path| -> Result<_> {
            let descriptor = manifest
                .payloads
                .iter()
                .find(|payload| {
                    &payload.path == path
                        && payload.role
                            == (PayloadRole::GitOriginDeltaPack {
                                repository_id: id.clone(),
                            })
                })
                .context("origin delta pack descriptor is missing")?;
            let mut temp = tempfile::NamedTempFile::new()
                .context("create temporary origin delta pack file")?;
            crate::archive::CheckpointPayloadReader::open(archive_path)?
                .copy_payload_unless(descriptor, temp.as_file_mut(), cancelled)
                .context("stream origin delta pack payload")?;
            std::io::Write::flush(temp.as_file_mut())
                .context("flush temporary origin delta pack file")?;
            Ok(temp)
        })
        .transpose()?;
    Ok(Some(SpooledWorkspaceSnapshot {
        snapshot: crate::snapshot::WorkspaceSnapshot {
            metadata: delta.clone(),
            pack: pack.as_ref().map(|file| file.path().to_path_buf()),
        },
        _pack: pack,
    }))
}

/// Put the archived launch base back on a managed network workspace.
///
/// Provisioning marks the fresh clone with its own `HEAD` as the base. The
/// archived base is the one the session started from, so it replaces that when
/// the restored repository has the commit. An archive that recorded none (it
/// predates this, or the checkout was not a managed workspace) and a checkout
/// without the managed marker keep what they have.
fn restore_managed_base(
    git: &dyn GitCommandRunner,
    repository: &Path,
    metadata: &crate::archive::RepositoryMetadata,
) -> Result<()> {
    if metadata.base_commit.is_empty()
        || crate::archive::remote_workspace_base(git, repository)?.is_none()
    {
        return Ok(());
    }
    if !crate::archive::managed_base_is_available(git, repository, &metadata.base_commit)? {
        tracing::warn!(repository = %repository.display(), base_commit = %metadata.base_commit,
            "archived launch base is absent from the restored repository; keeping the provisioned base");
        return Ok(());
    }
    mj_core::remote_git::finalize_managed_checkout(&metadata.base_commit, true, |arguments| {
        let output = git.run(
            repository,
            &crate::archive::GitCommand {
                arguments: arguments.iter().map(std::ffi::OsString::from).collect(),
                stdin: Vec::new(),
                env: Vec::new(),
            },
        )?;
        ensure!(
            output.status == 0,
            "restore the launch base: git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(())
    })
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
    ensure!(
        repository.metadata.origin_delta.is_none(),
        "origin delta requires managed clone provisioning"
    );
    Ok(RepositorySnapshot {
        origin_delta_pack: None,
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
        linked_worktrees: repository
            .linked_worktrees
            .iter()
            .enumerate()
            .map(|(index, worktree)| {
                let worktree_index = index as u32;
                let repository_id = || id.clone();
                Ok(crate::archive::LinkedWorktreeSnapshot {
                    metadata: worktree.metadata.clone(),
                    staged_patch: archive
                        .payload_by_role(&PayloadRole::GitLinkedWorktreeStagedPatch {
                            repository_id: repository_id(),
                            worktree: worktree_index,
                        })?
                        .to_vec(),
                    unstaged_patch: archive
                        .payload_by_role(&PayloadRole::GitLinkedWorktreeUnstagedPatch {
                            repository_id: repository_id(),
                            worktree: worktree_index,
                        })?
                        .to_vec(),
                    untracked_tar: archive
                        .payload_by_role(&PayloadRole::GitLinkedWorktreeUntrackedTar {
                            repository_id: repository_id(),
                            worktree: worktree_index,
                        })?
                        .to_vec(),
                    operation_payloads: match worktree.metadata.git_operation {
                        None => None,
                        Some(_) => Some(crate::archive::GitOperationPayloads {
                            conflicted_worktree_tar: archive
                                .payload_by_role(
                                    &PayloadRole::GitLinkedWorktreeConflictedWorktreeTar {
                                        repository_id: repository_id(),
                                        worktree: worktree_index,
                                    },
                                )?
                                .to_vec(),
                            state_tar: archive
                                .payload_by_role(
                                    &PayloadRole::GitLinkedWorktreeOperationStateTar {
                                        repository_id: repository_id(),
                                        worktree: worktree_index,
                                    },
                                )?
                                .to_vec(),
                        }),
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?,
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
) -> Result<Vec<String>> {
    ensure!(repository_path.is_dir(), "restore checkout is missing");
    let archive = crate::archive::read_archive_repositories(archive_path)?;
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
    snapshot.metadata.branch = Some(branch.to_owned());
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
    .with_context(|| format!("restore repository {:?}", repository.metadata.id))
}

/// Restore a retired independent clone on the branch saved in its archive.
pub fn restore_single_repository_into_checkout(
    archive_path: &Path,
    repository_path: &Path,
    git: &dyn GitCommandRunner,
) -> Result<Vec<String>> {
    ensure!(repository_path.is_dir(), "restore checkout is missing");
    let archive = crate::archive::read_archive_repositories(archive_path)?;
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

/// Restore a legacy single-repository archive into a fresh managed clone.
/// Checks out the saved commit on the saved branch name, or as a detached HEAD
/// (with a resume notice) if that branch name already exists pointing at a
/// different commit.
pub fn restore_single_repository_into_clone(
    archive_path: &Path,
    repository_path: &Path,
    git: &dyn GitCommandRunner,
    cancel: Option<&crate::archive::ArchivePurgeCancel>,
) -> Result<Vec<String>> {
    ensure!(repository_path.is_dir(), "restore checkout is missing");
    let archive = crate::archive::read_archive_repositories_unless(archive_path, cancel)?;
    let [repository] = archive.manifest.repositories.as_slice() else {
        bail!("an isolated clone requires exactly one archived repository");
    };
    ensure!(
        !repository.metadata.plain_directory,
        "this session's project is a directory without a committed Git checkout, so it cannot be restored into one"
    );
    let mut snapshot = archived_repository_snapshot(&archive, repository)?;
    snapshot.metadata.remote_workspace = false;
    crate::archive::restore_git_snapshot_for_session_with_options(
        git,
        repository_path,
        &snapshot,
        Some(&archive.manifest.session.id),
        true,
        crate::archive::LegacyRestoreOptions {
            detach_on_branch_collision: true,
        },
    )
    .context("restore legacy archive into fresh managed clone")
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
        HarnessKind::Dsh if super::native_scan::is_dsh_native_log_path(relative_path) => {
            if components.next() != Some(Component::Normal("dsh-home".as_ref()))
                || components.next() != Some(Component::Normal("sessions".as_ref()))
                || components.next().is_none()
            {
                return Ok(relative_path.to_path_buf());
            }
            let mut rewritten = PathBuf::from("dsh-home").join("sessions");
            rewritten.push(dsh_project_key(target_cwd)?);
            rewritten.extend(components);
            Ok(rewritten)
        }
        // OpenCode's database records session directories itself. The dsh-acp
        // projection records its directory inside the JSON file; only DSH's
        // native log has a per-workspace path key.
        HarnessKind::Codex
        | HarnessKind::Muse
        | HarnessKind::OpenCode
        | HarnessKind::Dsh
        | HarnessKind::Antigravity => Ok(relative_path.to_path_buf()),
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

enum NativeRelocation {
    Muse,
    DshProjection,
    DshNativeLog,
    Antigravity,
    OpenCode,
    GrokSummary,
    KimiRegistry,
    KimiIndex,
    KimiState,
}

/// The single owner of which native formats actually rewrite their contents.
fn native_artifact_relocation(harness: HarnessKind, path: &Path) -> Option<NativeRelocation> {
    match harness {
        HarnessKind::Muse if path.file_name().is_some_and(|name| name == "session.jsonl") => {
            Some(NativeRelocation::Muse)
        }
        HarnessKind::Dsh if super::native_scan::is_dsh_native_log_path(path) => {
            Some(NativeRelocation::DshNativeLog)
        }
        HarnessKind::Dsh if path.starts_with("sessions") => Some(NativeRelocation::DshProjection),
        HarnessKind::Antigravity
            if super::native_scan::is_sqlite_native_database(harness, path) =>
        {
            Some(NativeRelocation::Antigravity)
        }
        HarnessKind::OpenCode if super::native_scan::is_sqlite_native_database(harness, path) => {
            Some(NativeRelocation::OpenCode)
        }
        HarnessKind::Grok if is_grok_session_summary(path) => Some(NativeRelocation::GrokSummary),
        HarnessKind::Kimi if path == Path::new("workspaces.json") => {
            Some(NativeRelocation::KimiRegistry)
        }
        HarnessKind::Kimi if path == Path::new("session_index.jsonl") => {
            Some(NativeRelocation::KimiIndex)
        }
        HarnessKind::Kimi if is_kimi_session_state(path) => Some(NativeRelocation::KimiState),
        _ => None,
    }
}

pub(super) fn restored_native_artifact_bytes(
    harness: HarnessKind,
    native_session_id: &str,
    relative_path: &Path,
    data: &[u8],
    target_cwd: Option<&Path>,
    harness_home: &Path,
) -> Result<Vec<u8>> {
    let Some(target_cwd) = target_cwd else {
        return Ok(data.to_vec());
    };
    let target_cwd = &harness_storage_cwd(harness, target_cwd);
    match native_artifact_relocation(harness, relative_path) {
        None => return Ok(data.to_vec()),
        Some(NativeRelocation::Muse) => return crate::native::muse::relocate(data, target_cwd),
        Some(NativeRelocation::DshProjection) => {
            return rewrite_dsh_session_cwd(data, target_cwd);
        }
        Some(NativeRelocation::DshNativeLog) => {
            return rewrite_dsh_native_log_cwd(data, native_session_id, target_cwd);
        }
        Some(NativeRelocation::Antigravity) => {
            return crate::native::antigravity::relocate(data, target_cwd).with_context(|| {
                format!(
                    "relocate Antigravity conversation {} to {}",
                    relative_path.display(),
                    target_cwd.display()
                )
            });
        }
        Some(NativeRelocation::OpenCode) => {
            return crate::native::opencode::relocate(data, native_session_id, target_cwd)
                .with_context(|| {
                    format!(
                        "relocate OpenCode session {native_session_id} to {}",
                        target_cwd.display()
                    )
                });
        }
        Some(NativeRelocation::GrokSummary) => {
            return rewrite_grok_session_summary(data, target_cwd, harness_home);
        }
        Some(NativeRelocation::KimiRegistry) => {
            return rewrite_kimi_workspace_registry(data, target_cwd);
        }
        Some(NativeRelocation::KimiIndex) => {
            return rewrite_kimi_session_index(data, target_cwd, harness_home);
        }
        Some(NativeRelocation::KimiState) => {}
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

/// DSH's native session log starts with its own zstd frame containing exactly
/// one JSON header line. Rewrite that frame and recompress the remaining log
/// separately; merging the frames makes DSH reject the session.
fn rewrite_dsh_native_log_cwd(
    data: &[u8],
    native_session_id: &str,
    target_cwd: &Path,
) -> Result<Vec<u8>> {
    let unpacked = zstd::stream::decode_all(data).context("decompress native DSH session log")?;
    let newline = unpacked
        .iter()
        .position(|byte| *byte == b'\n')
        .context("native DSH session log has no header line")?;
    let (header, rest) = unpacked.split_at(newline + 1);
    let mut header: Value =
        serde_json::from_slice(header).context("parse native DSH session header")?;
    let object = header
        .as_object_mut()
        .context("native DSH session header is not a JSON object")?;
    ensure!(
        object.get("type").and_then(Value::as_str) == Some("session"),
        "native DSH session log does not start with a session header"
    );
    ensure!(
        object.get("id").and_then(Value::as_str) == Some(native_session_id),
        "native DSH session header names another session"
    );
    object.insert(
        "cwd".into(),
        Value::String(
            target_cwd
                .to_str()
                .context("native DSH target cwd is not valid UTF-8")?
                .to_owned(),
        ),
    );
    let mut header = serde_json::to_vec(&header)?;
    header.push(b'\n');
    let mut rewritten =
        zstd::stream::encode_all(header.as_slice(), 0).context("compress native DSH header")?;
    rewritten
        .extend(zstd::stream::encode_all(rest, 0).context("compress native DSH session events")?);
    Ok(rewritten)
}

/// DSH names a session-log directory after the UTF-16 code units in its cwd.
pub(super) fn dsh_project_key(cwd: &Path) -> Result<String> {
    let cwd = cwd
        .to_str()
        .context("native DSH target cwd is not valid UTF-8")?;
    ensure!(!cwd.is_empty(), "native DSH target cwd is empty");
    let mut readable = String::new();
    let mut separator_run = false;
    for unit in cwd.encode_utf16() {
        let ascii = u8::try_from(unit).ok();
        if matches!(ascii, Some(b'/' | b'\\' | b':')) {
            if !separator_run {
                readable.push('-');
            }
            separator_run = true;
        } else if let Some(byte) = ascii.filter(|byte| {
            *byte != b'~' && (byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        }) {
            readable.push(char::from(byte));
            separator_run = false;
        } else {
            use std::fmt::Write as _;
            write!(readable, "~{unit:04X}")?;
            separator_run = false;
        }
    }
    let slug = readable.trim_start_matches('-');
    let slug = if slug.is_empty() { "root" } else { slug };
    Ok(format!(
        "--{}--",
        slug.chars().take(251).collect::<String>()
    ))
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
    write_private_reader(root, relative, &mut std::io::Cursor::new(bytes), mode).map(|_| ())
}

pub(super) fn write_private_reader(
    root: &Path,
    relative: &Path,
    reader: &mut dyn Read,
    mode: u32,
) -> Result<u64> {
    write_private_with(root, relative, mode, |file| {
        Ok(std::io::copy(reader, file)?)
    })
}

fn write_missing_private_file(
    root: &Path,
    relative: &Path,
    bytes: &[u8],
    mode: u32,
) -> Result<bool> {
    write_missing_private_with(root, relative, mode, |file| {
        std::io::Write::write_all(file, bytes).context("write staged recovery file")
    })
}

/// Checks the destination and creates its parents, returning the path to
/// write. Both the replacing and the missing-only writers start here.
pub(super) fn prepare_private_destination(root: &Path, relative: &Path) -> Result<PathBuf> {
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
    Ok(path)
}

fn write_private_with<T>(
    root: &Path,
    relative: &Path,
    mode: u32,
    write: impl FnOnce(&mut File) -> Result<T>,
) -> Result<T> {
    let path = prepare_private_destination(root, relative)?;
    let mut options = fs::OpenOptions::new();
    options.write(true);
    options.create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode & 0o700).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("write {}", path.display()))?;
    let result = write(&mut file).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(mode & 0o700))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(result)
}

/// Streams a file into a staged copy and publishes it only when the
/// destination is still absent; returns whether it published.
fn write_missing_private_with(
    root: &Path,
    relative: &Path,
    mode: u32,
    write: impl FnOnce(&mut File) -> Result<()>,
) -> Result<bool> {
    let path = prepare_private_destination(root, relative)?;
    publish_missing_private_file(&path, mode, write)
}

fn publish_missing_private_file(
    path: &Path,
    mode: u32,
    write: impl FnOnce(&mut fs::File) -> Result<()>,
) -> Result<bool> {
    let mut temporary =
        tempfile::NamedTempFile::new_in(path.parent().context("recovery file has no parent")?)?;
    write(temporary.as_file_mut())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(mode & 0o700))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    temporary.as_file().sync_all()?;
    match temporary.persist_noclobber(path) {
        Ok(_) => Ok(true),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("publish recovery file {}", path.display()))
        }
    }
}

#[cfg(test)]
mod publication_tests {
    use super::*;
    use std::io::Write;

    // Hard-won: 337bc5e review: an interrupted missing-only write became permanent truncated history.
    #[test]
    fn failed_recovery_write_never_publishes_a_partial_file_and_retry_keeps_concurrent_work() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("native-history");
        let bytes = vec![b'n'; 128 * 1024];
        assert!(
            publish_missing_private_file(&destination, 0o600, |file| {
                file.write_all(&bytes[..70 * 1024])?;
                anyhow::bail!("injected write failure")
            })
            .is_err()
        );
        assert!(!destination.exists());
        assert!(
            publish_missing_private_file(&destination, 0o600, |file| {
                file.write_all(&bytes)?;
                Ok(())
            })
            .unwrap()
        );
        assert_eq!(fs::read(&destination).unwrap(), bytes);
        fs::remove_file(&destination).unwrap();
        assert!(
            !publish_missing_private_file(&destination, 0o600, |file| {
                file.write_all(&bytes)?;
                fs::write(&destination, b"concurrent native work")?;
                Ok(())
            })
            .unwrap()
        );
        assert_eq!(fs::read(&destination).unwrap(), b"concurrent native work");
    }
}
