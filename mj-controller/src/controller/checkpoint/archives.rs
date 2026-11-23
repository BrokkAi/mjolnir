use super::*;

/// Remove checkpoint archives installed by a process that exited before its
/// database transaction committed. Call this only while holding the
/// machine-wide controller-store guard and before starting background work.
pub fn reconcile_managed_checkpoint_archives() -> Result<usize> {
    let state = crate::database::load_state()?;
    let mut operations = crate::database::load_move_operations()?;
    // Daemons that shipped before Moves released the archive of an unsealed
    // source left it on the record. The sweep below then deletes the files.
    for operation in &mut operations {
        let source_live = state
            .sessions
            .get(&operation.selection.session_id)
            .is_some_and(|session| {
                matches!(
                    session.state,
                    SessionState::Running | SessionState::Disconnected
                )
            });
        if crate::controller::move_session::forget_unsealed_move_archives(operation, source_live)
            .is_some()
        {
            crate::database::save_move_operation(operation)?;
        }
    }
    let legacy_operations: Vec<mj_core::state::LegacyMoveOperation> =
        crate::database::load_legacy_move_operations()?
            .into_iter()
            .map(|(_, op)| op)
            .collect();
    let restarts = crate::database::load_session_restarts()?;
    let checkpoints = crate::database::load_session_checkpoints()?;
    let removed = reconcile_managed_checkpoint_archives_in(
        &sessions_dir(),
        checkpoints.iter().map(|(_, checkpoint)| checkpoint),
        &operations,
        &legacy_operations,
        &restarts,
    )?;
    // An archive a Move refers to can still be missing (removed by hand, or
    // by a daemon that predates this sweep keeping Move handoffs). The Move
    // record owns that fact; record it before anything reads the Move.
    crate::controller::move_session::record_missing_move_archives(&state, operations)?;
    // This scan is the last thing that removes an archive nothing references, so
    // it is also where a finished move's row stops being able to act. Sweeping
    // here means the startup load below this call already sees a clean store.
    super::reap_finished_move_intents();
    Ok(removed)
}

/// Every archive a session record or an unfinished Move or Restart refers to
/// is kept. Move references come from [`MoveOperation::retained_archives`], the
/// same answer the superseded-checkpoint prune reads.
pub(super) fn reconcile_managed_checkpoint_archives_in<'a>(
    directory: &Path,
    checkpoints: impl IntoIterator<Item = &'a CheckpointMetadata>,
    operations: &[mj_core::state::MoveOperation],
    legacy_operations: &[mj_core::state::LegacyMoveOperation],
    restarts: &[crate::database::SessionRestartIntent],
) -> Result<usize> {
    if !directory.exists() {
        return Ok(0);
    }
    let mut referenced_names =
        recovery_referenced_archive_names(operations, legacy_operations, restarts);
    referenced_names.extend(
        checkpoints
            .into_iter()
            .filter_map(|checkpoint| checkpoint.archive_path.file_name())
            .map(ToOwned::to_owned),
    );
    let mut removed = 0;
    for entry in std::fs::read_dir(directory)
        .with_context(|| format!("scan checkpoint directory {}", directory.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_file()
            || !is_managed_checkpoint_archive_name(&entry.file_name())
            || referenced_names.contains(&entry.file_name())
        {
            continue;
        }
        std::fs::remove_file(entry.path()).with_context(|| {
            format!(
                "remove unreferenced managed checkpoint {}",
                entry.path().display()
            )
        })?;
        removed += 1;
    }
    Ok(removed)
}

/// Archive file names a durable Move or Restart still restores from.
///
/// A session's own checkpoint row is not in this set. Startup reconciliation
/// unions that row in before it deletes anything; purge uses this set alone
/// so it can replace the row and only then delete the previous file.
pub(super) fn recovery_referenced_archive_names(
    operations: &[mj_core::state::MoveOperation],
    legacy_operations: &[mj_core::state::LegacyMoveOperation],
    restarts: &[crate::database::SessionRestartIntent],
) -> BTreeSet<std::ffi::OsString> {
    operations
        .iter()
        .flat_map(mj_core::state::MoveOperation::retained_archives)
        .chain(
            legacy_operations
                .iter()
                .flat_map(mj_core::state::LegacyMoveOperation::retained_archives),
        )
        .chain(restarts.iter().filter_map(|intent| intent.handoff.as_ref()))
        .filter_map(|checkpoint| checkpoint.archive_path.file_name())
        .map(ToOwned::to_owned)
        .collect()
}

pub(crate) use mj_store::managed_archive::managed_checkpoint_archive_name;

pub(super) fn is_managed_checkpoint_archive_name(name: &OsStr) -> bool {
    managed_checkpoint_archive_name(name).is_some()
}

/// Remove every checkpoint archive that belongs to a session being destroyed:
/// the one its record points at, and any other `<session_id>-<n>-archive-<hex>`
/// file in `directory`, such as the handoff archive a Move wrote and the record
/// no longer references. A file belongs to the session only when its parsed
/// session ID equals `session_id`, so a session whose ID is a prefix of this
/// one is never touched. Retained Move sources are workspaces and checkouts
/// recorded in the database, not archive files, so they are not affected.
pub(crate) fn remove_session_checkpoint_archives(
    directory: &Path,
    session_id: &str,
    current: Option<&CheckpointMetadata>,
) -> Result<()> {
    if let Some(checkpoint) = current
        && let Err(error) = std::fs::remove_file(&checkpoint.archive_path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error).with_context(|| {
            format!(
                "remove session recovery archive {}",
                checkpoint.archive_path.display()
            )
        });
    }
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("scan checkpoint directory {}", directory.display()));
        }
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file()
            || managed_checkpoint_archive_name(&entry.file_name())
                .is_none_or(|name| name.session_id != session_id)
        {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("remove session recovery archive {}", entry.path().display())
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::state::{MoveOperation, MovePhase, MoveSelection, ResumeQueueDisposition};

    fn checkpoint(name: &str) -> CheckpointMetadata {
        CheckpointMetadata {
            archive_path: PathBuf::from(name),
            sha256: "a".repeat(64),
            created_at: "2026-08-14T12:00:00Z".into(),
            event_frontier: 7,
        }
    }

    fn move_holding(checkpoint: CheckpointMetadata, phase: MovePhase) -> MoveOperation {
        MoveOperation {
            quota_stop_reason: None,
            quota_continuation: None,
            prepared_destination: None,
            accepted_preparation: None,
            acknowledge_interruption: false,
            transfer_route: None,
            consent: Default::default(),
            skip_keep_paths: Vec::new(),
            granted_keys: Vec::new(),
            granted_ec2_rules: Vec::new(),
            in_place_expected_store_id: None,
            in_place_continuation_command_id: None,
            in_place_held_command_ids: Vec::new(),
            in_place_harness_restarted: false,
            deferred_environment_changes: Vec::new(),
            source_snapshot: None,
            in_place: false,
            source_checkpoint_only: false,
            operation_id: "move-purge".into(),
            selection: MoveSelection {
                oracle: None,
                oracle_choice: None,
                subagents: None,
                consent: Default::default(),
                skip_keep_paths: Vec::new(),
                clear_resource_allocation: false,
                session_id: "1123456789abcdef0123456789abcdef".into(),
                profile_id: None,
                target_template_id: None,
                additional_mounts: None,
                resource_allocation: None,
            },
            source_profile_id: "codex".into(),
            source_target_template_id: "local".into(),
            source_target: None,
            source_native_session_id: None,
            inherited_resource_allocation: None,
            inherited_additional_mounts: Vec::new(),
            source_additional_mounts: Vec::new(),
            source_resource_allocation: None,
            destination_target: None,
            destination_native_session_id: None,
            destination_store_id: None,
            configuration_fingerprint: "purge-test".into(),
            checkpoint: Some(checkpoint),
            recovery_session: None,
            queue: ResumeQueueDisposition::Discard,
            phase,
            queue_admission_started: false,
            queue_admission_finished: false,
            cancellation_requested: false,
            created_at: "2026-08-14T12:00:00Z".into(),
            updated_at: "2026-08-14T12:00:00Z".into(),
            error: None,
        }
    }

    #[test]
    fn recovery_referenced_names_are_move_and_restart_archives_only() {
        let held = checkpoint("sessions/held-7-archive-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.hel.zip");
        let handoff =
            checkpoint("sessions/restart-7-archive-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.hel.zip");
        let own = checkpoint("sessions/own-7-archive-cccccccccccccccccccccccccccccccc.hel.zip");
        let forgotten =
            checkpoint("sessions/done-7-archive-dddddddddddddddddddddddddddddddd.hel.zip");
        let restart = crate::database::SessionRestartIntent {
            session_id: "restart".into(),
            operation_id: "restart-op".into(),
            phase: crate::database::SessionRestartPhase::Stopping,
            checkpoint_sha256: Some(handoff.sha256.clone()),
            handoff: Some(handoff.clone()),
            checkpoint_started: true,
            container_recreation_acknowledged: false,
            automatic: false,
            container_upgrade: None,
        };
        let legacy_handoff =
            checkpoint("sessions/legacy-7-archive-eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee.hel.zip");
        let legacy_failed = mj_core::state::LegacyMoveOperation {
            selection: Some(mj_core::state::LegacyMoveSelection {
                session_id: "legacy".into(),
                profile_id: Some("dest".into()),
                target_template_id: Some("local".into()),
                ..Default::default()
            }),
            quota_stop_reason: None,
            quota_continuation: None,
            prepared_destination: None,
            acknowledge_interruption: false,
            handoff: Some(legacy_handoff.clone()),
            source_checkpoint_only: false,
            in_place: false,
            operation_id: "legacy-retry".into(),
            source_profile_id: "src".into(),
            source_target_template_id: "local".into(),
            source_target: None,
            source_native_session_id: None,
            inherited_resource_allocation: None,
            inherited_additional_mounts: Vec::new(),
            source_additional_mounts: Vec::new(),
            source_resource_allocation: None,
            destination_target: None,
            destination_native_session_id: None,
            destination_store_id: None,
            configuration_fingerprint: String::new(),
            checkpoint: None,
            recovery_session: None,
            queue: ResumeQueueDisposition::Start,
            phase: mj_core::state::LegacyMovePhase::Failed,
            queue_admission_started: false,
            queue_admission_finished: false,
            cancellation_requested: false,
            created_at: "2026-08-14T12:00:00Z".into(),
            updated_at: "2026-08-14T12:00:00Z".into(),
            error: Some("destination failed".into()),
        };
        assert!(!legacy_failed.is_resolved());
        assert!(!legacy_failed.needs_recovery());
        let converted = legacy_failed.into_retryable_move_operation("legacy");
        assert_eq!(converted.checkpoint.as_ref(), Some(&legacy_handoff));
        assert_eq!(converted.selection.profile_id.as_deref(), Some("dest"));
        let names = recovery_referenced_archive_names(
            &[
                move_holding(held.clone(), MovePhase::Capturing),
                move_holding(forgotten.clone(), MovePhase::Completed),
            ],
            &[legacy_failed],
            &[restart],
        );
        assert!(names.contains(held.archive_path.file_name().unwrap()));
        assert!(names.contains(legacy_handoff.archive_path.file_name().unwrap()));
        assert!(names.contains(handoff.archive_path.file_name().unwrap()));
        assert!(!names.contains(own.archive_path.file_name().unwrap()));
        assert!(!names.contains(forgotten.archive_path.file_name().unwrap()));
        assert_eq!(names.len(), 3);
    }
}
