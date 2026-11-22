//! Remove legacy untracked-file payloads from stopped sessions' checkpoints.
//!
//! The daemon admits the session and calls in. This module does not take the
//! recovery gate itself, and it does not touch Move source environments.
//! Verification and rewriting do not hold daemon-handoff admission. Only the
//! commit — installing the new archive, swapping the checkpoint row, and
//! unlinking the old archive — takes that admission, and only for those calls.
//! Reported bytes are the archive file's on-disk delta, or for a dry run the
//! compressed size of the removed untracked ZIP entries.

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};

use super::archives::{is_managed_checkpoint_archive_name, recovery_referenced_archive_names};
use super::*;
use mj_checkpoint::archive::{
    ArchivePurgeCancel, ArchiveUntrackedPurgeKind, PreparedUntrackedPurge, StagedUntrackedPurge,
    StashUntrackedNote, archive_purge_not_cancelled, prepare_untracked_purge,
};

/// One session's purge, ready for a later command or HTTP surface to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PurgeSessionOutcome {
    Purged {
        bytes_reclaimed: u64,
        stash_untracked_notes: Vec<StashUntrackedNote>,
        warnings: Vec<String>,
    },
    DryRunWouldPurge {
        bytes_reclaimed: u64,
        stash_untracked_notes: Vec<StashUntrackedNote>,
        warnings: Vec<String>,
    },
    Skipped {
        reason: String,
    },
    /// Verification or rewriting stopped before the archive was replaced.
    ///
    /// The caller decides whether that was a client disconnect or a daemon
    /// handoff. No destination was installed.
    Aborted,
}

/// One row of a batch purge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeSessionResult {
    pub session_id: String,
    pub outcome: PurgeSessionOutcome,
}

/// Every selected session, plus the bytes a live or dry run would reclaim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeUntrackedReport {
    pub sessions: Vec<PurgeSessionResult>,
    pub total_bytes_reclaimed: u64,
}

/// Purge one stopped session's legacy untracked files.
///
/// `admission` must name `session_id`. A running or otherwise active session,
/// a session with no checkpoint, a checkpoint an unfinished Move or Restart
/// still names, an origin-delta archive, and an archive whose untracked
/// payloads are already empty are all [`PurgeSessionOutcome::Skipped`].
pub fn purge_session_untracked_archive(
    admission: &crate::recovery_gate::SessionAdmission,
    session_id: &str,
    dry_run: bool,
    cancelled: &ArchivePurgeCancel,
) -> Result<PurgeSessionOutcome> {
    // Reject a mismatched admission before opening the store. A development
    // build must not write the default instance, and this check needs no row.
    ensure_admission(admission, session_id)?;
    let state = crate::database::load_state()?;
    let operations = crate::database::load_move_operations()?;
    let legacy_operations: Vec<_> = crate::database::load_legacy_move_operations()?
        .into_iter()
        .map(|(_, op)| op)
        .collect();
    let restarts = crate::database::load_session_restarts()?;
    let recovery = recovery_referenced_archive_names(&operations, &legacy_operations, &restarts);
    purge_loaded(admission, session_id, dry_run, &state, &recovery, cancelled)
}

/// Purge each selected session. State, Moves, and Restarts are loaded once.
///
/// An admission that names a different session, an unknown session, a missing
/// archive, or a checkpoint row that changes during the write fails the
/// batch. A session that is simply not eligible is a skip and the rest of
/// the batch still runs.
pub fn purge_sessions_untracked_archives(
    sessions: &[(&crate::recovery_gate::SessionAdmission, &str)],
    dry_run: bool,
) -> Result<PurgeUntrackedReport> {
    for (admission, session_id) in sessions {
        ensure_admission(admission, session_id)?;
    }
    let state = crate::database::load_state()?;
    let operations = crate::database::load_move_operations()?;
    let legacy_operations: Vec<_> = crate::database::load_legacy_move_operations()?
        .into_iter()
        .map(|(_, op)| op)
        .collect();
    let restarts = crate::database::load_session_restarts()?;
    let recovery = recovery_referenced_archive_names(&operations, &legacy_operations, &restarts);
    let mut report = PurgeUntrackedReport {
        sessions: Vec::with_capacity(sessions.len()),
        total_bytes_reclaimed: 0,
    };
    for (admission, session_id) in sessions {
        let outcome = purge_loaded(
            admission,
            session_id,
            dry_run,
            &state,
            &recovery,
            archive_purge_not_cancelled(),
        )?;
        report.total_bytes_reclaimed += reclaimed(&outcome);
        report.sessions.push(PurgeSessionResult {
            session_id: (*session_id).to_owned(),
            outcome,
        });
    }
    Ok(report)
}

fn reclaimed(outcome: &PurgeSessionOutcome) -> u64 {
    match outcome {
        PurgeSessionOutcome::Purged {
            bytes_reclaimed, ..
        }
        | PurgeSessionOutcome::DryRunWouldPurge {
            bytes_reclaimed, ..
        } => *bytes_reclaimed,
        PurgeSessionOutcome::Skipped { .. } | PurgeSessionOutcome::Aborted => 0,
    }
}

fn ensure_admission(
    admission: &crate::recovery_gate::SessionAdmission,
    session_id: &str,
) -> Result<()> {
    if admission.session_id() != session_id {
        bail!(
            "session admission is for {}, not {session_id}",
            admission.session_id()
        );
    }
    Ok(())
}

fn purge_loaded(
    admission: &crate::recovery_gate::SessionAdmission,
    session_id: &str,
    dry_run: bool,
    state: &State,
    recovery: &std::collections::BTreeSet<OsString>,
    cancelled: &ArchivePurgeCancel,
) -> Result<PurgeSessionOutcome> {
    ensure_admission(admission, session_id)?;
    let Some(session) = state.sessions.get(session_id) else {
        bail!("unknown session {session_id}");
    };
    if session.state != SessionState::Stopped {
        return Ok(PurgeSessionOutcome::Skipped {
            reason: format!("session is {}", session.state.as_str()),
        });
    }
    let Some(checkpoint) = session.checkpoint.as_deref() else {
        return Ok(PurgeSessionOutcome::Skipped {
            reason: "session has no checkpoint".to_owned(),
        });
    };
    let checkpoint = checkpoint.clone();
    let Some(file_name) = checkpoint.archive_path.file_name().map(OsString::from) else {
        bail!(
            "checkpoint archive {} has no file name",
            checkpoint.archive_path.display()
        );
    };
    if recovery.contains(&file_name) {
        return Ok(PurgeSessionOutcome::Skipped {
            reason: "referenced by durable recovery state".to_owned(),
        });
    }
    if !checkpoint.archive_path.is_file() {
        bail!(
            "checkpoint archive {} is missing",
            checkpoint.archive_path.display()
        );
    }
    // Compare the file that is on disk with the digest the checkpoint row
    // recorded before any rewrite. A mismatch, for a live purge or a dry run,
    // leaves that file and the row alone.
    if cancelled() {
        return Ok(PurgeSessionOutcome::Aborted);
    }
    let archive_sha256 = match mj_checkpoint::archive::verify_archive_streaming_unless(
        &checkpoint.archive_path,
        cancelled,
    ) {
        Ok(verified) => verified.archive_sha256,
        Err(_error) if cancelled() => return Ok(PurgeSessionOutcome::Aborted),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "verify checkpoint archive {} before purging untracked files",
                    checkpoint.archive_path.display()
                )
            });
        }
    };
    ensure!(
        archive_sha256 == checkpoint.sha256,
        "checkpoint archive {} sha256 is {archive_sha256}, not the recorded {}",
        checkpoint.archive_path.display(),
        checkpoint.sha256
    );
    let directory = checkpoint
        .archive_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .context("checkpoint archive has no directory")?
        .to_path_buf();
    let destination = if dry_run {
        directory.join(format!("{session_id}-purge-dry-run.hel.zip"))
    } else {
        unique_managed_archive_path(session_id, checkpoint.event_frontier, &directory)?
    };
    let prepared = match prepare_untracked_purge(
        &checkpoint.archive_path,
        &destination,
        dry_run,
        cancelled,
    ) {
        Ok(prepared) => prepared,
        Err(_error) if cancelled() => return Ok(PurgeSessionOutcome::Aborted),
        Err(error) => return Err(error),
    };
    match prepared {
        PreparedUntrackedPurge::Cancelled => Ok(PurgeSessionOutcome::Aborted),
        PreparedUntrackedPurge::Finished(report) => match report.kind {
            ArchiveUntrackedPurgeKind::AlreadyNewFormat => Ok(PurgeSessionOutcome::Skipped {
                reason: "archive is already origin-delta format".to_owned(),
            }),
            ArchiveUntrackedPurgeKind::AlreadyClean => Ok(PurgeSessionOutcome::Skipped {
                reason: already_empty_reason(&report.stash_untracked_notes, &report.warnings),
            }),
            ArchiveUntrackedPurgeKind::DryRun => {
                ensure!(dry_run, "archive purge reported a dry run for a live purge");
                Ok(PurgeSessionOutcome::DryRunWouldPurge {
                    bytes_reclaimed: report.bytes_reclaimed,
                    stash_untracked_notes: report.stash_untracked_notes,
                    warnings: report.warnings,
                })
            }
            ArchiveUntrackedPurgeKind::Rewritten { .. } => {
                bail!("archive purge installed a file before the commit step")
            }
        },
        PreparedUntrackedPurge::Staged(staged) => {
            ensure!(!dry_run, "archive purge staged a file during a dry run");
            commit_staged(session_id, &checkpoint, staged, cancelled)
        }
    }
}

fn already_empty_reason(notes: &[StashUntrackedNote], warnings: &[String]) -> String {
    if notes.is_empty() && warnings.is_empty() {
        return "untracked payloads are already empty".to_owned();
    }
    let mut reason = String::from("untracked payloads are already empty");
    if !notes.is_empty() {
        reason.push_str("; stash entries still hold untracked files:");
        for note in notes {
            reason.push_str(&format!(
                " {} {} ({});",
                note.repository_id, note.commit, note.message
            ));
        }
    }
    if !warnings.is_empty() {
        reason.push_str("; warnings: ");
        reason.push_str(&warnings.join("; "));
    }
    reason
}

fn unique_managed_archive_path(
    session_id: &str,
    frontier: u64,
    directory: &Path,
) -> Result<PathBuf> {
    for _ in 0..8 {
        let archive_id = new_command_id("archive")?;
        let path = directory.join(format!("{session_id}-{frontier}-{archive_id}.hel.zip"));
        if path.exists() {
            continue;
        }
        let name = path
            .file_name()
            .context("allocated checkpoint archive has no file name")?;
        ensure!(
            is_managed_checkpoint_archive_name(name),
            "purge allocated {}, which startup reconciliation would ignore",
            path.display()
        );
        return Ok(path);
    }
    bail!("could not allocate a unique checkpoint archive for {session_id}")
}

fn commit_staged(
    session_id: &str,
    previous: &CheckpointMetadata,
    staged: StagedUntrackedPurge,
    cancelled: &ArchivePurgeCancel,
) -> Result<PurgeSessionOutcome> {
    // A test can stop here, before handoff admission, and close the upgrade
    // gate. The staged file is still temporary; returning drops it.
    pause_before_purge_commit();
    if cancelled() {
        return Ok(PurgeSessionOutcome::Aborted);
    }
    let _hold = match crate::upgrade::activity_unless_draining("purge untracked commit") {
        Ok(hold) => hold,
        Err(error) => {
            tracing::debug!(
                session_id,
                error = format!("{error:#}"),
                "purge untracked commit deferred for daemon handoff"
            );
            return Ok(PurgeSessionOutcome::Aborted);
        }
    };
    if cancelled() {
        return Ok(PurgeSessionOutcome::Aborted);
    }
    let destination = staged.destination().to_path_buf();
    let report = staged.install()?;
    let ArchiveUntrackedPurgeKind::Rewritten { archive_sha256 } = report.kind.clone() else {
        return Err(discard_unreferenced(
            &destination,
            anyhow::anyhow!("purged archive install did not report a rewritten archive"),
        ));
    };
    commit_replacement(
        session_id,
        previous,
        destination,
        archive_sha256,
        report,
    )
}

fn commit_replacement(
    session_id: &str,
    previous: &CheckpointMetadata,
    destination: PathBuf,
    archive_sha256: String,
    report: mj_checkpoint::archive::ArchiveUntrackedPurgeReport,
) -> Result<PurgeSessionOutcome> {
    let replacement = CheckpointMetadata {
        archive_path: destination.clone(),
        sha256: archive_sha256,
        created_at: previous.created_at.clone(),
        event_frontier: previous.event_frontier,
    };
    let committed = match crate::database::replace_checkpoint_archive_if_current(
        session_id,
        &previous.archive_path,
        &previous.sha256,
        &replacement,
    ) {
        Ok(committed) => committed,
        Err(error) => {
            return Err(discard_unreferenced(&destination, error));
        }
    };
    if !committed {
        return Err(discard_unreferenced(
            &destination,
            anyhow::anyhow!(
                "checkpoint for {session_id} changed while its untracked files were purged"
            ),
        ));
    }
    let mut warnings = report.warnings;
    if let Err(error) = std::fs::remove_file(&previous.archive_path) {
        tracing::warn!(
            session_id,
            path = %previous.archive_path.display(),
            %error,
            "purged checkpoint committed but the old archive could not be deleted"
        );
        warnings.push(format!(
            "purged archive committed, but old archive {} could not be deleted ({error}); space will not be reclaimed until it is removed",
            previous.archive_path.display()
        ));
    }
    Ok(PurgeSessionOutcome::Purged {
        bytes_reclaimed: report.bytes_reclaimed,
        stash_untracked_notes: report.stash_untracked_notes,
        warnings,
    })
}

fn pause_before_purge_commit() {
    #[cfg(any(test, feature = "test-support"))]
    commit_pause::wait_if_armed();
}

#[cfg(any(test, feature = "test-support"))]
mod commit_pause {
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    struct Slot {
        entered: bool,
        release: bool,
    }

    struct State {
        slot: Mutex<Slot>,
        wake: Condvar,
    }

    static PAUSE: Mutex<Option<Arc<State>>> = Mutex::new(None);

    /// Block the next purge commit until [`PurgeCommitPause::release`].
    pub fn arm() -> PurgeCommitPause {
        let state = Arc::new(State {
            slot: Mutex::new(Slot {
                entered: false,
                release: false,
            }),
            wake: Condvar::new(),
        });
        *PAUSE.lock().expect("purge commit pause") = Some(Arc::clone(&state));
        PurgeCommitPause { state }
    }

    pub fn wait_if_armed() {
        let Some(state) = PAUSE.lock().expect("purge commit pause").clone() else {
            return;
        };
        let mut slot = state.slot.lock().expect("purge commit pause");
        if slot.release {
            return;
        }
        slot.entered = true;
        state.wake.notify_all();
        while !slot.release {
            slot = state.wake.wait(slot).expect("purge commit pause");
        }
    }

    /// Pauses the purge on the far side of archive rewrite, before the commit
    /// takes daemon-handoff admission.
    #[derive(Clone)]
    pub struct PurgeCommitPause {
        state: Arc<State>,
    }

    impl PurgeCommitPause {
        pub fn wait_until_blocked(&self, timeout: Duration) -> bool {
            let mut slot = self.state.slot.lock().expect("purge commit pause");
            let deadline = std::time::Instant::now() + timeout;
            while !slot.entered {
                let now = std::time::Instant::now();
                if now >= deadline {
                    return false;
                }
                let (guard, result) = self
                    .state
                    .wake
                    .wait_timeout(slot, deadline - now)
                    .expect("purge commit pause");
                slot = guard;
                if result.timed_out() && !slot.entered {
                    return false;
                }
            }
            true
        }

        pub fn release(&self) {
            let mut slot = self.state.slot.lock().expect("purge commit pause");
            slot.release = true;
            self.state.wake.notify_all();
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub use commit_pause::{PurgeCommitPause, arm as arm_purge_untracked_commit_pause};

fn discard_unreferenced(path: &std::path::Path, error: anyhow::Error) -> anyhow::Error {
    match std::fs::remove_file(path) {
        Ok(()) => error,
        Err(remove) => error.context(format!(
            "failed to remove unreferenced purged archive {}: {remove}",
            path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use mj_checkpoint::archive::{
        ArchiveInput, RepositoryMetadata, RepositorySnapshot, SnapshotHead,
        WorkspaceSnapshotMetadata, write_archive_atomic,
    };
    use mj_core::config::sessions_dir;
    use mj_core::state::{
        CheckpointMetadata, MoveOperation, MovePhase, MoveSelection, ResumeQueueDisposition,
        SessionState,
    };

    use super::*;
    use crate::controller::test_support::{
        IsolatedTest, checkpoint_archive_input, checkpoint_test_session, session_admission,
        test_name,
    };

    const CHILD: &str = "MJ_PURGE_UNTRACKED_TEST";
    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    fn purge_one(
        admission: &crate::recovery_gate::SessionAdmission,
        session_id: &str,
        dry_run: bool,
    ) -> Result<PurgeSessionOutcome> {
        purge_session_untracked_archive(
            admission,
            session_id,
            dry_run,
            mj_checkpoint::archive::archive_purge_not_cancelled(),
        )
    }

    fn enter_child(test: &str) -> bool {
        if std::env::var_os(CHILD).is_some() {
            return false;
        }
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(test_name(module_path!(), test))
            .env(CHILD, "1")
            .isolated_store(directory.path())
            .run();
        true
    }

    fn untracked_tar(name: &str, contents: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut output);
            let mut header = tar::Header::new_gnu();
            header.set_path(name).unwrap();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, contents).unwrap();
            builder.finish().unwrap();
        }
        output
    }

    fn legacy_repository(untracked: Vec<u8>) -> RepositorySnapshot {
        RepositorySnapshot {
            origin_delta_pack: None,
            metadata: RepositoryMetadata {
                origin_delta: None,
                review_baseline_ref: None,
                review_refs_metadata_only: false,
                saved_refs: Default::default(),
                stash_stack: Vec::new(),
                git_operation: None,
                id: "project".into(),
                relative_destination: "project".into(),
                checkout_subdirectory: None,
                origin: "https://github.com/example/project.git".into(),
                push_urls: Vec::new(),
                remote_workspace: false,
                plain_directory: false,
                base_commit: "a".repeat(40),
                head_commit: "b".repeat(40),
                branch: Some("main".into()),
            },
            committed_bundle: b"bundle-project".to_vec(),
            staged_patch: b"staged".to_vec(),
            unstaged_patch: b"unstaged".to_vec(),
            untracked_tar: untracked,
            operation_payloads: None,
            linked_worktrees: Vec::new(),
        }
    }

    fn origin_delta_repository() -> RepositorySnapshot {
        let mut repository = legacy_repository(Vec::new());
        repository.committed_bundle.clear();
        repository.staged_patch.clear();
        repository.unstaged_patch.clear();
        repository.untracked_tar.clear();
        let origin = repository.metadata.origin.clone();
        let head = repository.metadata.head_commit.clone();
        repository.metadata.origin_delta = Some(Box::new(WorkspaceSnapshotMetadata {
            origin_url: Some(origin),
            object_format: "sha1".into(),
            notices: Vec::new(),
            head: SnapshotHead::Branch {
                name: "main".into(),
                commit: head.clone(),
            },
            refs: [("refs/heads/main".into(), head)].into(),
            branch_tracking: Default::default(),
            stash: Vec::new(),
            working_state: None,
            working_state_skipped: false,
            has_uncommitted_state: false,
            worktrees: Vec::new(),
            initialized_submodules: Vec::new(),
            index_gitlinks: Vec::new(),
            remote_refs_stale: false,
            prerequisites: Vec::new(),
            history_cutoff: false,
        }));
        repository
    }

    fn archive_input(session_id: &str, repository: RepositorySnapshot) -> ArchiveInput {
        checkpoint_archive_input(session_id, 7, vec![repository], Vec::new())
    }

    fn write_managed(session_id: &str, input: &ArchiveInput) -> CheckpointMetadata {
        fs::create_dir_all(sessions_dir()).unwrap();
        let archive_path = sessions_dir().join(format!("{session_id}-7-archive-{NONCE}.hel.zip"));
        let verified = write_archive_atomic(&archive_path, input).unwrap();
        CheckpointMetadata {
            archive_path,
            sha256: verified.archive_sha256,
            created_at: "2026-08-14T12:00:00Z".into(),
            event_frontier: 7,
        }
    }

    fn copy_archive(session_id: &str, source: &CheckpointMetadata) -> CheckpointMetadata {
        let archive_path = sessions_dir().join(format!("{session_id}-7-archive-{NONCE}.hel.zip"));
        fs::copy(&source.archive_path, &archive_path).unwrap();
        CheckpointMetadata {
            archive_path,
            sha256: source.sha256.clone(),
            created_at: source.created_at.clone(),
            event_frontier: source.event_frontier,
        }
    }

    fn save(session_id: &str, state: SessionState, checkpoint: Option<CheckpointMetadata>) {
        let mut session = checkpoint_test_session(session_id);
        session.state = state;
        session.checkpoint = checkpoint.map(Box::new);
        crate::database::save_session(&session).unwrap();
    }

    fn directory_names() -> BTreeSet<std::ffi::OsString> {
        fs::read_dir(sessions_dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect()
    }

    fn move_holding(checkpoint: CheckpointMetadata) -> MoveOperation {
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
                supervisors: None,
                oracle_choice: None,
                subagents: None,
                consent: Default::default(),
                skip_keep_paths: Vec::new(),
                clear_resource_allocation: false,
                session_id: "44444444444444444444444444444444".into(),
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
            phase: MovePhase::Capturing,
            queue_admission_started: false,
            queue_admission_finished: false,
            cancellation_requested: false,
            created_at: "2026-08-14T12:00:00Z".into(),
            updated_at: "2026-08-14T12:00:00Z".into(),
            error: None,
        }
    }

    fn skipped_reason(outcome: PurgeSessionOutcome) -> String {
        match outcome {
            PurgeSessionOutcome::Skipped { reason } => reason,
            other => panic!("expected a skip, got {other:?}"),
        }
    }

    #[test]
    fn purge_rejects_an_admission_for_a_different_session() {
        let admission = session_admission("11111111111111111111111111111111");
        let error =
            purge_one(&admission, "22222222222222222222222222222222", true)
                .unwrap_err();
        assert!(
            error.to_string().contains("session admission is for"),
            "{error:#}"
        );
    }

    #[test]
    fn purge_skips_running_recovery_origin_delta_and_empty_archives() {
        if enter_child("purge_skips_running_recovery_origin_delta_and_empty_archives") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let legacy = write_managed(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            &archive_input(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                legacy_repository(untracked_tar("pad.txt", &vec![b'a'; 8 * 1024])),
            ),
        );
        let legacy_bytes = fs::read(&legacy.archive_path).unwrap();
        let running = copy_archive("11111111111111111111111111111111", &legacy);
        let disconnected = copy_archive("22222222222222222222222222222222", &legacy);
        let referenced = copy_archive("44444444444444444444444444444444", &legacy);
        let restarted = copy_archive("55555555555555555555555555555555", &legacy);
        let batch_stopped = copy_archive("88888888888888888888888888888888", &legacy);
        let empty = write_managed(
            "77777777777777777777777777777777",
            &archive_input(
                "77777777777777777777777777777777",
                legacy_repository(Vec::new()),
            ),
        );
        let origin = write_managed(
            "66666666666666666666666666666666",
            &archive_input(
                "66666666666666666666666666666666",
                origin_delta_repository(),
            ),
        );
        save(
            "11111111111111111111111111111111",
            SessionState::Running,
            Some(running.clone()),
        );
        save(
            "22222222222222222222222222222222",
            SessionState::Disconnected,
            Some(disconnected.clone()),
        );
        save(
            "33333333333333333333333333333333",
            SessionState::Stopped,
            None,
        );
        save(
            "44444444444444444444444444444444",
            SessionState::Stopped,
            Some(referenced.clone()),
        );
        crate::database::save_move_operation(&move_holding(referenced.clone())).unwrap();
        save(
            "55555555555555555555555555555555",
            SessionState::Stopped,
            Some(restarted.clone()),
        );
        crate::database::begin_session_restart(
            "55555555555555555555555555555555",
            "restart-op",
            Arc::new(AtomicBool::new(false)),
            false,
        )
        .unwrap();
        crate::database::record_session_restart_handoff(
            "55555555555555555555555555555555",
            "restart-op",
            &restarted,
        )
        .unwrap();
        save(
            "66666666666666666666666666666666",
            SessionState::Stopped,
            Some(origin.clone()),
        );
        save(
            "77777777777777777777777777777777",
            SessionState::Stopped,
            Some(empty.clone()),
        );
        save(
            "88888888888888888888888888888888",
            SessionState::Stopped,
            Some(batch_stopped.clone()),
        );

        let running_admission = session_admission("11111111111111111111111111111111");
        let names_before = directory_names();
        assert_eq!(
            skipped_reason(
                purge_one(
                    &running_admission,
                    "11111111111111111111111111111111",
                    false,
                )
                .unwrap()
            ),
            "session is running"
        );
        assert_eq!(
            skipped_reason(
                purge_one(
                    &session_admission("22222222222222222222222222222222"),
                    "22222222222222222222222222222222",
                    false,
                )
                .unwrap()
            ),
            "session is disconnected"
        );
        assert_eq!(
            skipped_reason(
                purge_one(
                    &session_admission("33333333333333333333333333333333"),
                    "33333333333333333333333333333333",
                    false,
                )
                .unwrap()
            ),
            "session has no checkpoint"
        );
        assert_eq!(
            skipped_reason(
                purge_one(
                    &session_admission("44444444444444444444444444444444"),
                    "44444444444444444444444444444444",
                    false,
                )
                .unwrap()
            ),
            "referenced by durable recovery state"
        );
        assert_eq!(
            skipped_reason(
                purge_one(
                    &session_admission("55555555555555555555555555555555"),
                    "55555555555555555555555555555555",
                    false,
                )
                .unwrap()
            ),
            "referenced by durable recovery state"
        );
        assert_eq!(
            skipped_reason(
                purge_one(
                    &session_admission("66666666666666666666666666666666"),
                    "66666666666666666666666666666666",
                    false,
                )
                .unwrap()
            ),
            "archive is already origin-delta format"
        );
        assert_eq!(
            skipped_reason(
                purge_one(
                    &session_admission("77777777777777777777777777777777"),
                    "77777777777777777777777777777777",
                    false,
                )
                .unwrap()
            ),
            "untracked payloads are already empty"
        );
        let batch_running = session_admission("11111111111111111111111111111111");
        let batch_stopped_admission = session_admission("88888888888888888888888888888888");
        let batch = purge_sessions_untracked_archives(
            &[
                (&batch_running, "11111111111111111111111111111111"),
                (&batch_stopped_admission, "88888888888888888888888888888888"),
            ],
            true,
        )
        .unwrap();
        assert!(matches!(
            batch.sessions[0].outcome,
            PurgeSessionOutcome::Skipped { .. }
        ));
        let PurgeSessionOutcome::DryRunWouldPurge {
            bytes_reclaimed, ..
        } = &batch.sessions[1].outcome
        else {
            panic!(
                "dry run did not report a purge: {:?}",
                batch.sessions[1].outcome
            );
        };
        assert!(*bytes_reclaimed > 0);
        let inspected =
            mj_checkpoint::archive::verify_archive_streaming(&batch_stopped.archive_path).unwrap();
        let uncompressed: u64 = inspected
            .manifest
            .payloads
            .iter()
            .filter(|payload| {
                matches!(
                    payload.role,
                    mj_core::archive::PayloadRole::GitUntrackedTar { .. }
                        | mj_core::archive::PayloadRole::GitLinkedWorktreeUntrackedTar { .. }
                ) && payload.size > 1024
            })
            .map(|payload| payload.size - 1024)
            .sum();
        assert!(
            *bytes_reclaimed < uncompressed,
            "dry run reported {bytes_reclaimed}, the uncompressed excess is {uncompressed}"
        );
        assert_eq!(batch.total_bytes_reclaimed, *bytes_reclaimed);
        assert_eq!(directory_names(), names_before);
        for path in [
            &running.archive_path,
            &disconnected.archive_path,
            &referenced.archive_path,
            &restarted.archive_path,
            &batch_stopped.archive_path,
        ] {
            assert_eq!(fs::read(path).unwrap(), legacy_bytes);
        }
        assert_eq!(
            crate::database::load_state().unwrap().sessions["44444444444444444444444444444444"]
                .checkpoint
                .as_ref()
                .unwrap()
                .archive_path,
            referenced.archive_path
        );
        let _ = (origin, empty);
    }

    #[test]
    fn purge_rewrites_a_stopped_legacy_archive_and_updates_the_checkpoint_row() {
        if enter_child("purge_rewrites_a_stopped_legacy_archive_and_updates_the_checkpoint_row") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let session_id = "99999999999999999999999999999999";
        let previous = write_managed(
            session_id,
            &archive_input(
                session_id,
                legacy_repository(untracked_tar("pad.txt", &vec![b'a'; 8 * 1024])),
            ),
        );
        let previous_bytes = fs::read(&previous.archive_path).unwrap();
        let old_len = previous_bytes.len() as u64;
        let inspected =
            mj_checkpoint::archive::verify_archive_streaming(&previous.archive_path).unwrap();
        let uncompressed: u64 = inspected
            .manifest
            .payloads
            .iter()
            .filter(|payload| {
                matches!(
                    payload.role,
                    mj_core::archive::PayloadRole::GitUntrackedTar { .. }
                        | mj_core::archive::PayloadRole::GitLinkedWorktreeUntrackedTar { .. }
                ) && payload.size > 1024
            })
            .map(|payload| payload.size - 1024)
            .sum();
        save(session_id, SessionState::Stopped, Some(previous.clone()));
        let outcome =
            purge_one(&session_admission(session_id), session_id, false)
                .unwrap();
        let PurgeSessionOutcome::Purged {
            bytes_reclaimed, ..
        } = outcome
        else {
            panic!("expected a purge, got {outcome:?}");
        };
        assert!(bytes_reclaimed > 0);
        assert!(
            bytes_reclaimed < uncompressed,
            "purge reported {bytes_reclaimed}, the uncompressed excess is {uncompressed}"
        );
        assert!(!previous.archive_path.exists());
        let loaded = crate::database::load_state().unwrap();
        let session = &loaded.sessions[session_id];
        assert_eq!(session.state, SessionState::Stopped);
        let checkpoint = session.checkpoint.as_ref().unwrap();
        assert_ne!(checkpoint.archive_path, previous.archive_path);
        assert_ne!(checkpoint.sha256, previous.sha256);
        assert_eq!(checkpoint.created_at, previous.created_at);
        assert_eq!(checkpoint.event_frontier, previous.event_frontier);
        assert_eq!(
            checkpoint.archive_path.parent(),
            Some(sessions_dir().as_path())
        );
        assert!(is_managed_checkpoint_archive_name(
            checkpoint.archive_path.file_name().unwrap()
        ));
        let verified =
            mj_checkpoint::archive::verify_archive_streaming(&checkpoint.archive_path).unwrap();
        assert_eq!(verified.archive_sha256, checkpoint.sha256);
        let new_len = checkpoint.archive_path.metadata().unwrap().len();
        assert!(new_len < old_len);
        assert_eq!(bytes_reclaimed, old_len.saturating_sub(new_len));
        assert!(
            verified
                .manifest
                .repositories
                .iter()
                .all(|repository| repository.metadata.origin_delta.is_none())
        );
    }

    #[test]
    fn replace_checkpoint_archive_if_current_is_compare_and_swap() {
        if enter_child("replace_checkpoint_archive_if_current_is_compare_and_swap") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let session_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        fs::create_dir_all(sessions_dir()).unwrap();
        let previous = CheckpointMetadata {
            archive_path: sessions_dir().join(format!("{session_id}-7-archive-{NONCE}.hel.zip")),
            sha256: "a".repeat(64),
            created_at: "2026-08-14T12:00:00Z".into(),
            event_frontier: 7,
        };
        save(session_id, SessionState::Stopped, Some(previous.clone()));
        let mut replacement = previous.clone();
        replacement.archive_path = sessions_dir().join(format!(
            "{session_id}-7-archive-ffffffffffffffffffffffffffffffff.hel.zip"
        ));
        replacement.sha256 = "b".repeat(64);
        assert!(
            !crate::database::replace_checkpoint_archive_if_current(
                session_id,
                &previous.archive_path,
                &"c".repeat(64),
                &replacement,
            )
            .unwrap()
        );
        let unchanged = crate::database::load_state().unwrap().sessions[session_id]
            .checkpoint
            .clone()
            .unwrap();
        assert_eq!(unchanged.archive_path, previous.archive_path);
        assert_eq!(unchanged.sha256, previous.sha256);
        assert!(
            crate::database::replace_checkpoint_archive_if_current(
                session_id,
                &previous.archive_path,
                &previous.sha256,
                &replacement,
            )
            .unwrap()
        );
        let updated = crate::database::load_state().unwrap().sessions[session_id]
            .checkpoint
            .clone()
            .unwrap();
        assert_eq!(updated.archive_path, replacement.archive_path);
        assert_eq!(updated.sha256, replacement.sha256);
        assert_eq!(updated.created_at, previous.created_at);
        assert_eq!(updated.event_frontier, previous.event_frontier);
        assert!(
            !crate::database::replace_checkpoint_archive_if_current(
                session_id,
                &previous.archive_path,
                &previous.sha256,
                &previous,
            )
            .unwrap()
        );
        let still = crate::database::load_state().unwrap().sessions[session_id]
            .checkpoint
            .clone()
            .unwrap();
        assert_eq!(still, updated);
    }

    #[test]
    fn purge_refuses_an_archive_whose_bytes_do_not_match_the_checkpoint_sha256() {
        if enter_child("purge_refuses_an_archive_whose_bytes_do_not_match_the_checkpoint_sha256") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let session_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let recorded = write_managed(
            session_id,
            &archive_input(
                session_id,
                legacy_repository(untracked_tar("scratch.txt", b"scratch\n")),
            ),
        );
        let other = write_managed(
            "cccccccccccccccccccccccccccccccc",
            &archive_input(
                "cccccccccccccccccccccccccccccccc",
                legacy_repository(untracked_tar("other.txt", b"other\n")),
            ),
        );
        assert_ne!(recorded.sha256, other.sha256);
        let other_bytes = fs::read(&other.archive_path).unwrap();
        fs::write(&recorded.archive_path, &other_bytes).unwrap();
        save(session_id, SessionState::Stopped, Some(recorded.clone()));
        let names_before = directory_names();
        for dry_run in [false, true] {
            let error = purge_one(
                &session_admission(session_id),
                session_id,
                dry_run,
            )
            .unwrap_err();
            let message = format!("{error:#}");
            assert!(
                message.contains(&recorded.sha256),
                "dry_run={dry_run}: {message}"
            );
            assert!(message.contains("sha256"), "dry_run={dry_run}: {message}");
        }
        let loaded = crate::database::load_state().unwrap();
        let checkpoint = loaded.sessions[session_id].checkpoint.as_ref().unwrap();
        assert_eq!(checkpoint.archive_path, recorded.archive_path);
        assert_eq!(checkpoint.sha256, recorded.sha256);
        assert_eq!(fs::read(&recorded.archive_path).unwrap(), other_bytes);
        assert_eq!(directory_names(), names_before);
    }

    #[test]
    fn purges_checked_in_old_format_fixture_and_resumes_it() {
        if enter_child("purges_checked_in_old_format_fixture_and_resumes_it") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/legacy-untracked-checkpoint.hel.zip");
        assert!(
            fixture_path.is_file(),
            "checked-in fixture {} is missing",
            fixture_path.display()
        );
        let session_id = "dddddddddddddddddddddddddddddddd";
        let verified_before =
            mj_checkpoint::archive::verify_archive_streaming(&fixture_path).unwrap();
        assert_eq!(verified_before.manifest.schema_version, 12);
        assert!(verified_before.manifest.repositories[0].metadata.origin_delta.is_none());

        fs::create_dir_all(sessions_dir()).unwrap();
        let managed_path = sessions_dir().join(format!("{session_id}-1-{NONCE}.hel.zip"));
        fs::copy(&fixture_path, &managed_path).unwrap();
        let original_size = managed_path.metadata().unwrap().len();
        let checkpoint = CheckpointMetadata {
            archive_path: managed_path.clone(),
            created_at: "2026-11-21T00:00:00Z".into(),
            event_frontier: 1,
            sha256: verified_before.archive_sha256.clone(),
        };
        save(session_id, SessionState::Stopped, Some(checkpoint));

        let outcome = purge_one(&session_admission(session_id), session_id, false).unwrap();
        let PurgeSessionOutcome::Purged {
            bytes_reclaimed, ..
        } = outcome
        else {
            panic!("expected Purged outcome for checked-in legacy fixture, got {outcome:?}");
        };
        assert!(bytes_reclaimed > 0, "expected reclaimed on-disk bytes");
        assert!(!managed_path.exists(), "old fixture copy should be unlinked");

        let loaded = crate::database::load_state().unwrap();
        let updated = loaded.sessions[session_id]
            .checkpoint
            .as_ref()
            .expect("updated checkpoint row");
        assert_ne!(updated.archive_path, managed_path);
        assert!(updated.archive_path.metadata().unwrap().len() < original_size);
        let verified_after =
            mj_checkpoint::archive::verify_archive_streaming(&updated.archive_path).unwrap();
        assert_eq!(verified_after.manifest.schema_version, 12);
        assert_eq!(verified_after.archive_sha256, updated.sha256);

        // Resume/restore the purged old-format archive into a fresh checkout.
        let restore_workspace = tempfile::tempdir().unwrap();
        let dest_repo = restore_workspace.path().join("project");
        fs::create_dir_all(&dest_repo).unwrap();
        let run_git_dest = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(&dest_repo)
                .env("GIT_AUTHOR_DATE", "1700000000 +0000")
                .env("GIT_COMMITTER_DATE", "1700000000 +0000")
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        run_git_dest(&["init", "-b", "main"]);
        run_git_dest(&["config", "user.name", "Fixture"]);
        run_git_dest(&["config", "user.email", "fixture@example.test"]);
        fs::write(dest_repo.join("tracked.txt"), "base line\n").unwrap();
        run_git_dest(&["add", "tracked.txt"]);
        run_git_dest(&["commit", "-m", "initial base"]);
        assert_eq!(
            run_git_dest(&["rev-parse", "HEAD"]),
            verified_after.manifest.repositories[0].metadata.base_commit
        );
        mj_checkpoint::checkpoint::restore_repositories(
            &updated.archive_path,
            restore_workspace.path(),
            &mj_checkpoint::archive::SystemGit,
        )
        .unwrap();

        assert_eq!(
            run_git_dest(&["symbolic-ref", "--short", "HEAD"]),
            "feature/legacy"
        );
        assert_eq!(
            fs::read_to_string(dest_repo.join("feature.txt")).unwrap(),
            "committed legacy feature\n"
        );
        assert_eq!(
            run_git_dest(&["show", ":tracked.txt"]),
            "staged change"
        );
        assert_eq!(
            fs::read_to_string(dest_repo.join("tracked.txt")).unwrap(),
            "unstaged change\n"
        );
        assert!(
            !dest_repo.join("scratch.bin").exists(),
            "purged untracked payload must be absent after resume"
        );
    }
}
